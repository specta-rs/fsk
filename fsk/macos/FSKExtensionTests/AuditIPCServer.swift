import Foundation
import Darwin

struct AuditIPCPeerError: Error, CustomStringConvertible {
    let description: String
}

enum AuditIPCBehavior: Sendable {
    case respond(status: Int32 = 0, body: Data = Data())
    case trickle(body: Data)
    case close
    case hang
}

struct AuditIPCScript: Sendable {
    let opcode: UInt16
    var behavior: AuditIPCBehavior = .respond()
    var expectedRequestBody: Data? = nil
}

/// Deterministic loopback-only FSKit IPC peer used by the audit regressions.
final class AuditIPCServer: @unchecked Sendable {
    private let listener: Int32
    private let socketURL: URL
    private let socketPath: String
    private let directory: URL
    private let scripts: [[AuditIPCScript]]
    private let lock = NSLock()
    private let scriptsDone = DispatchSemaphore(value: 0)
    private let done = DispatchSemaphore(value: 0)
    private var scriptsSignaled = false
    private var stopping = false
    private var peers = Set<Int32>()
    private var failures: [String] = []
    private var completedConnections = 0

    init(scripts: [[AuditIPCScript]]) throws {
        self.scripts = scripts
        let uniqueDirectory = "fsk-audit-\(UUID().uuidString)"
        let relativeDirectory = "audit/tmp/\(uniqueDirectory)"
        directory = auditTemporaryDirectory().appending(path: uniqueDirectory)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        socketPath = "\(relativeDirectory)/ipc.sock"
        socketURL = directory.appending(path: "ipc.sock")
        var (address, length) = try unixSocketAddress(path: socketPath)
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        listener = fd
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(fd, $0, length)
            }
        }
        guard bound == 0 else {
            Darwin.close(fd)
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        guard Darwin.listen(fd, 8) == 0 else {
            let error = errno
            Darwin.close(fd)
            try? FileManager.default.removeItem(at: directory)
            throw POSIXError(POSIXErrorCode(rawValue: error) ?? .EIO)
        }
    }

    deinit { Darwin.close(listener) }

    var resourceURL: URL {
        get throws {
            var components = URLComponents()
            components.scheme = "fsk"
            components.host = "audit"
            components.queryItems = [
                URLQueryItem(name: "socket", value: socketPath),
                URLQueryItem(name: "token", value: String(repeating: "00", count: 32)),
            ]
            guard let url = components.url else { throw AuditIPCPeerError(description: "invalid peer URL") }
            return url
        }
    }

    func start() {
        DispatchQueue.global(qos: .userInitiated).async { [self] in run() }
    }

    func stop() {
        lock.lock()
        if stopping {
            lock.unlock()
            return
        }
        stopping = true
        let currentPeers = Array(peers)
        lock.unlock()
        for peer in currentPeers { Darwin.shutdown(peer, SHUT_RDWR) }
        _ = Darwin.connect(listener, nil, 0)
        _ = done.wait(timeout: .now() + 3)
    }

    func wait(timeout: TimeInterval = 3) throws {
        if scriptsDone.wait(timeout: .now() + timeout) == .timedOut {
            throw AuditIPCPeerError(description: "IPC peer did not consume its script")
        }
        lock.lock()
        let errors = failures
        let count = completedConnections
        lock.unlock()
        if !errors.isEmpty { throw AuditIPCPeerError(description: errors.joined(separator: "; ")) }
        if count != scripts.count {
            throw AuditIPCPeerError(description: "IPC peer served \(count) of \(scripts.count) connections")
        }
    }

    private func run() {
        defer {
            signalScriptsDone()
            Darwin.close(listener)
            try? FileManager.default.removeItem(at: directory)
            done.signal()
        }
        let group = DispatchGroup()
        var accepted = 0
        while accepted < scripts.count {
            var ready = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
            let polled = Darwin.poll(&ready, 1, 100)
            if polled < 0 && errno == EINTR { continue }
            if polled <= 0 {
                lock.lock()
                let shouldStop = stopping
                lock.unlock()
                if shouldStop { return }
                if polled < 0 { record("poll failed: \(errno)"); return }
                continue
            }
            let peer = Darwin.accept(listener, nil, nil)
            guard peer >= 0 else { record("accept failed: \(errno)"); return }
            let script = scripts[accepted]
            accepted += 1
            lock.lock(); peers.insert(peer); lock.unlock()
            group.enter()
            DispatchQueue.global(qos: .userInitiated).async { [self] in
                defer { group.leave() }
                do {
                    try serve(peer, script: script)
                    lock.lock(); completedConnections += 1; lock.unlock()
                } catch {
                    record(String(describing: error))
                }
            }
        }
        group.wait()
        signalScriptsDone()
        while true {
            lock.lock(); let shouldStop = stopping; lock.unlock()
            if shouldStop { break }
            usleep(10_000)
        }
        lock.lock()
        let currentPeers = Array(peers)
        peers.removeAll()
        lock.unlock()
        for peer in currentPeers { Darwin.close(peer) }
    }

    private func signalScriptsDone() {
        lock.lock()
        if !scriptsSignaled {
            scriptsSignaled = true
            scriptsDone.signal()
        }
        lock.unlock()
    }

    private func serve(_ peer: Int32, script: [AuditIPCScript]) throws {
        var timeout = timeval(tv_sec: 2, tv_usec: 0)
        guard setsockopt(peer, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0 else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        var noSignal: Int32 = 1
        guard setsockopt(peer, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, socklen_t(MemoryLayout<Int32>.size)) == 0 else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        for expected in script {
            let header = try auditReadExactly(24, from: peer)
            let opcode = header.withUnsafeBytes { UInt16(littleEndian: $0.loadUnaligned(fromByteOffset: 6, as: UInt16.self)) }
            guard opcode == expected.opcode else {
                throw AuditIPCPeerError(description: "expected opcode \(expected.opcode), got \(opcode)")
            }
            let length = header.withUnsafeBytes { UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 16, as: UInt32.self)) }
            guard length <= 16 * 1024 * 1024 else { throw AuditIPCPeerError(description: "oversized request") }
            let requestBody = try auditReadExactly(Int(length), from: peer)
            if let expectedBody = expected.expectedRequestBody, requestBody != expectedBody {
                throw AuditIPCPeerError(description: "unexpected request body for opcode \(opcode)")
            }
            switch expected.behavior {
            case let .respond(status, body):
                var response = Data()
                auditAppend(UInt32(0x4653_4249), to: &response)
                auditAppend(ipcProtocolMajor, to: &response)
                auditAppend(ipcProtocolMinor, to: &response)
                response.append(header[8..<16])
                auditAppend(status, to: &response)
                auditAppend(UInt32(body.count), to: &response)
                response.append(body)
                try auditWriteAll(response, to: peer)
            case let .trickle(body):
                var response = Data()
                auditAppend(UInt32(0x4653_4249), to: &response)
                auditAppend(ipcProtocolMajor, to: &response)
                auditAppend(ipcProtocolMinor, to: &response)
                response.append(header[8..<16])
                auditAppend(Int32(0), to: &response)
                auditAppend(UInt32(body.count), to: &response)
                response.append(body)
                try response.withUnsafeBytes { bytes in
                    for index in 0..<bytes.count {
                        lock.lock(); let stop = stopping; lock.unlock()
                        if stop { return }
                        let sent = Darwin.write(peer, bytes.baseAddress! + index, 1)
                        if sent < 0 && errno == EINTR { continue }
                        guard sent == 1 else {
                            throw POSIXError(POSIXErrorCode(rawValue: errno == 0 ? EPIPE : errno) ?? .EIO)
                        }
                        usleep(100_000)
                    }
                }
            case .close:
                return
            case .hang:
                while true {
                    lock.lock(); let stop = stopping; lock.unlock()
                    if stop { return }
                    usleep(10_000)
                }
            }
        }
    }

    private func record(_ message: String) {
        lock.lock(); failures.append(message); lock.unlock()
    }
}

func auditTemporaryDirectory() -> URL {
    let path = ProcessInfo.processInfo.environment["FSK_AUDIT_TMP"] ?? NSTemporaryDirectory()
    return URL(fileURLWithPath: path, isDirectory: true)
}

func withAuditBackend(
    _ script: [[AuditIPCScript]],
    requireComplete: Bool = true,
    _ body: (RustBackend) throws -> Void
) throws {
    let server = try AuditIPCServer(scripts: script)
    server.start()
    defer { server.stop() }
    let backend = try RustBackend(resourceURL: server.resourceURL)
    try body(backend)
    if requireComplete { try server.wait() }
}

func auditHello() -> AuditIPCScript {
    .init(opcode: 0, expectedRequestBody: Data(repeating: 0, count: 32))
}
func auditLocalConfiguration() -> AuditIPCScript { .init(opcode: 1, behavior: .respond(body: Data([0, 0, 0, 0]))) }
func auditNetworkConfiguration() -> AuditIPCScript { .init(opcode: 1, behavior: .respond(body: Data([1, 1, 1, 1]))) }

func auditAttributes(
    inode: UInt64,
    parent: UInt64 = 1,
    size: UInt64 = 0,
    modified: UInt64 = 0,
    accessed: UInt64 = 0,
    kind: UInt8 = FSK_KIND_FILE
) -> Data {
    var data = Data()
    auditAppend(inode, to: &data)
    auditAppend(parent, to: &data)
    auditAppend(size, to: &data)
    auditAppend(UInt64(0), to: &data)
    auditAppend(UInt64(1), to: &data)
    auditAppend(UInt64(1_000_000_000), to: &data)
    auditAppend(modified, to: &data)
    auditAppend(accessed, to: &data)
    auditAppend(UInt32(0o644), to: &data)
    auditAppend(UInt32(501), to: &data)
    auditAppend(UInt32(20), to: &data)
    auditAppend(UInt32(1), to: &data)
    auditAppend(kind, to: &data)
    return data
}

func auditGetAttributes(_ body: Data) -> AuditIPCScript {
    .init(opcode: 3, behavior: .respond(body: body))
}

private func auditReadExactly(_ count: Int, from fd: Int32) throws -> Data {
    var data = Data(count: count)
    try data.withUnsafeMutableBytes { raw in
        var offset = 0
        while offset < raw.count {
            let received = Darwin.read(fd, raw.baseAddress! + offset, raw.count - offset)
            if received < 0 && errno == EINTR { continue }
            guard received > 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno == 0 ? ECONNRESET : errno) ?? .EIO) }
            offset += received
        }
    }
    return data
}

private func auditWriteAll(_ data: Data, to fd: Int32) throws {
    try data.withUnsafeBytes { raw in
        var offset = 0
        while offset < raw.count {
            let sent = Darwin.write(fd, raw.baseAddress! + offset, raw.count - offset)
            if sent < 0 && errno == EINTR { continue }
            guard sent > 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno == 0 ? EPIPE : errno) ?? .EIO) }
            offset += sent
        }
    }
}

func auditAppend<T: FixedWidthInteger>(_ value: T, to data: inout Data) {
    var littleEndian = value.littleEndian
    withUnsafeBytes(of: &littleEndian) { data.append(contentsOf: $0) }
}
