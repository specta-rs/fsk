import Foundation
import Darwin

struct AuditIPCPeerError: Error, CustomStringConvertible {
    let description: String
}

enum AuditIPCBehavior: Sendable {
    case respond(status: Int32 = 0, body: Data = Data())
    case trickle(body: Data)
    case close
    case applyThenClose
    case hang
}

struct AuditIPCScript: Sendable {
    let opcode: UInt16
    var behavior: AuditIPCBehavior = .respond()
    var expectedRequestBody: Data? = nil
    var replyMinor: UInt16 = ipcProtocolMinor
}

let auditIPCRequestTimeout: TimeInterval = 0.25

final class AuditIPCBarrier: @unchecked Sendable {
    let responseBody: Data
    private let expected: Int
    private let lock = NSLock()
    private let arrivals = DispatchGroup()
    private let completions = DispatchGroup()
    private var arrivalCount = 0
    private var completionCount = 0
    private var cancelled = false

    init(expecting: Int, responseBody: Data = Data(count: 52)) {
        precondition(expecting > 0)
        self.expected = expecting
        self.responseBody = responseBody
        for _ in 0..<expecting {
            arrivals.enter()
            completions.enter()
        }
    }

    func arriveAndWait() {
        lock.lock()
        if arrivalCount < expected {
            arrivalCount += 1
            arrivals.leave()
        }
        lock.unlock()
        arrivals.wait()
    }

    func complete() {
        lock.lock()
        if completionCount < expected {
            completionCount += 1
            completions.leave()
        }
        lock.unlock()
    }

    func cancel() {
        lock.lock()
        guard !cancelled else {
            lock.unlock()
            return
        }
        cancelled = true
        while arrivalCount < expected {
            arrivalCount += 1
            arrivals.leave()
        }
        while completionCount < expected {
            completionCount += 1
            completions.leave()
        }
        lock.unlock()
    }

    func wait(timeout: TimeInterval) -> DispatchTimeoutResult {
        completions.wait(timeout: .now() + timeout)
    }

    var completedNormally: Bool {
        lock.lock(); defer { lock.unlock() }
        return !cancelled && completionCount == expected
    }

    var isCancelled: Bool {
        lock.lock(); defer { lock.unlock() }
        return cancelled
    }
}

/// Deterministic loopback-only FSKit IPC peer used by the audit regressions.
final class AuditIPCServer: @unchecked Sendable {
    private enum ServerMode: Sendable {
        case scripts([[AuditIPCScript]])
        case concurrent(AuditIPCBarrier)
    }

    private let listener: Int32
    private let socketPath: String
    private let directory: URL
    private let mode: ServerMode
    private let lock = NSLock()
    private let scriptsDone = DispatchSemaphore(value: 0)
    private let done = DispatchSemaphore(value: 0)
    private var scriptsSignaled = false
    private var stopping = false
    private var listenerClosed = false
    private var peers = Set<Int32>()
    private var failures: [String] = []
    private var completedConnections = 0
    private var leaseConnections: [UInt64: Int32] = [:]

    private init(mode: ServerMode) throws {
        self.mode = mode
        let uniqueDirectory = "fsk-\(UUID().uuidString.prefix(8))"
        directory = auditTemporaryDirectory().appending(path: uniqueDirectory)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        socketPath = directory.appending(path: "ipc.sock").path
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

    convenience init(scripts: [[AuditIPCScript]]) throws {
        try self.init(mode: .scripts(scripts))
    }

    convenience init(barrier: AuditIPCBarrier) throws {
        try self.init(mode: .concurrent(barrier))
    }

    deinit { closeListener() }

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

    var socketPathForTesting: String { socketPath }

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
        if case .concurrent(let barrier) = mode {
            barrier.cancel()
        }
        let currentPeers = Array(peers)
        lock.unlock()
        for peer in currentPeers { Darwin.shutdown(peer, SHUT_RDWR) }
        let wake = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        if wake >= 0 {
            _ = Darwin.connect(wake, nil, 0)
            Darwin.close(wake)
        }
        _ = done.wait(timeout: .now() + 3)
    }

    func wait(timeout: TimeInterval = 3) throws {
        switch mode {
        case .scripts(let scripts):
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
        case .concurrent(let barrier):
            let result = barrier.wait(timeout: timeout)
            lock.lock()
            let errors = failures
            lock.unlock()
            if !errors.isEmpty { throw AuditIPCPeerError(description: errors.joined(separator: "; ")) }
            guard result == .success, barrier.completedNormally else {
                throw AuditIPCPeerError(description: "IPC peer did not complete its concurrent requests")
            }
        }
    }

    private func run() {
        switch mode {
        case .scripts(let scripts):
            runScripted(scripts)
        case .concurrent(let barrier):
            runConcurrent(barrier)
        }
    }

    private func runScripted(_ scripts: [[AuditIPCScript]]) {
        defer {
            signalScriptsDone()
            closeListener()
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

    private func runConcurrent(_ barrier: AuditIPCBarrier) {
        defer {
            signalScriptsDone()
            closeListener()
            try? FileManager.default.removeItem(at: directory)
            done.signal()
        }
        let group = DispatchGroup()
        var accepted = 0
        while true {
            var ready = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
            let polled = Darwin.poll(&ready, 1, 100)
            if polled < 0 && errno == EINTR { continue }
            lock.lock()
            let shouldStop = stopping
            lock.unlock()
            if shouldStop { break }
            guard polled > 0 else {
                if polled < 0 { record("poll failed: \(errno)"); break }
                continue
            }
            let peer = Darwin.accept(listener, nil, nil)
            guard peer >= 0 else { record("accept failed: \(errno)"); break }
            let includeConfiguration = accepted == 0
            accepted += 1
            lock.lock(); peers.insert(peer); lock.unlock()
            group.enter()
            DispatchQueue.global(qos: .userInitiated).async { [self] in
                defer { group.leave() }
                do {
                    try serveConcurrent(
                        peer,
                        barrier: barrier,
                        includeConfiguration: includeConfiguration
                    )
                } catch {
                    record(String(describing: error))
                }
            }
        }
        lock.lock()
        let currentPeers = Array(peers)
        lock.unlock()
        for peer in currentPeers { Darwin.shutdown(peer, SHUT_RDWR) }
        group.wait()
        lock.lock()
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

    private func closeListener() {
        lock.lock()
        guard !listenerClosed else {
            lock.unlock()
            return
        }
        listenerClosed = true
        lock.unlock()
        Darwin.close(listener)
    }

    private func serve(_ peer: Int32, script: [AuditIPCScript]) throws {
        var negotiatedMinor = ipcProtocolMinor
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
            if opcode == 0 { negotiatedMinor = expected.replyMinor }
            let length = header.withUnsafeBytes { UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 16, as: UInt32.self)) }
            guard length <= 16 * 1024 * 1024 else { throw AuditIPCPeerError(description: "oversized request") }
            let requestBody = try auditReadExactly(Int(length), from: peer)
            if let expectedBody = expected.expectedRequestBody, requestBody != expectedBody {
                throw AuditIPCPeerError(description: "unexpected request body for opcode \(opcode)")
            }
            if opcode == 17 {
                guard negotiatedMinor >= 2 else {
                    try writeAuditResponse(header, to: peer, status: EOPNOTSUPP, body: Data(), minor: negotiatedMinor)
                    return
                }
                guard requestBody.count == 8 else {
                    try writeAuditResponse(header, to: peer, status: EINVAL, body: Data(), minor: negotiatedMinor)
                    return
                }
                let lease = requestBody.withUnsafeBytes { UInt64(littleEndian: $0.loadUnaligned(fromByteOffset: 0, as: UInt64.self)) }
                lock.lock()
                let owner = leaseConnections[lease]
                lock.unlock()
                guard owner == peer else {
                    try writeAuditResponse(header, to: peer, status: EINVAL, body: Data(), minor: negotiatedMinor)
                    return
                }
            }
            switch expected.behavior {
            case let .respond(status, body):
                try writeAuditResponse(header, to: peer, status: status, body: body, minor: negotiatedMinor)
                if status == 0 {
                    lock.lock()
                    switch opcode {
                    case 15 where body.count >= 8:
                        let lease = body.withUnsafeBytes { UInt64(littleEndian: $0.loadUnaligned(fromByteOffset: 0, as: UInt64.self)) }
                        if lease != 0 { leaseConnections[lease] = peer }
                    case 16 where requestBody.count >= 8:
                        let lease = requestBody.withUnsafeBytes { UInt64(littleEndian: $0.loadUnaligned(fromByteOffset: 0, as: UInt64.self)) }
                        leaseConnections.removeValue(forKey: lease)
                    case 17:
                        let lease = requestBody.withUnsafeBytes { UInt64(littleEndian: $0.loadUnaligned(fromByteOffset: 0, as: UInt64.self)) }
                        leaseConnections.removeValue(forKey: lease)
                    default:
                        break
                    }
                    lock.unlock()
                }
            case let .trickle(body):
                var response = Data()
                auditAppend(UInt32(0x4653_4249), to: &response)
                auditAppend(ipcProtocolMajor, to: &response)
                auditAppend(negotiatedMinor, to: &response)
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
                lock.lock()
                peers.remove(peer)
                lock.unlock()
                Darwin.close(peer)
                return
            case .applyThenClose:
                guard opcode == 17, requestBody.count == 8 else {
                    throw AuditIPCPeerError(description: "applyThenClose requires an ACK request")
                }
                let lease = requestBody.withUnsafeBytes { UInt64(littleEndian: $0.loadUnaligned(fromByteOffset: 0, as: UInt64.self)) }
                lock.lock()
                leaseConnections.removeValue(forKey: lease)
                peers.remove(peer)
                lock.unlock()
                Darwin.close(peer)
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

    private func serveConcurrent(
        _ peer: Int32,
        barrier: AuditIPCBarrier,
        includeConfiguration: Bool
    ) throws {
        var handshake = [auditHello(minor: ipcProtocolMinor)]
        if includeConfiguration { handshake.append(auditLocalConfiguration()) }
        try serve(peer, script: handshake)
        var timeout = timeval(tv_sec: 10, tv_usec: 0)
        guard setsockopt(peer, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0 else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        while true {
            let header: Data
            do {
                header = try auditReadExactly(24, from: peer)
            } catch let error as POSIXError where error.code == .ECONNRESET || error.code == .ETIMEDOUT {
                return
            }
            let opcode = header.withUnsafeBytes { UInt16(littleEndian: $0.loadUnaligned(fromByteOffset: 6, as: UInt16.self)) }
            guard opcode == 2 else {
                throw AuditIPCPeerError(description: "expected concurrent statfs request, got \(opcode)")
            }
            let length = header.withUnsafeBytes { UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 16, as: UInt32.self)) }
            guard length <= 16 * 1024 * 1024 else { throw AuditIPCPeerError(description: "oversized request") }
            _ = try auditReadExactly(Int(length), from: peer)

            barrier.arriveAndWait()
            guard !barrier.isCancelled else { return }
            var response = Data()
            auditAppend(UInt32(0x4653_4249), to: &response)
            auditAppend(ipcProtocolMajor, to: &response)
            auditAppend(ipcProtocolMinor, to: &response)
            response.append(header[8..<16])
            auditAppend(Int32(0), to: &response)
            auditAppend(UInt32(barrier.responseBody.count), to: &response)
            response.append(barrier.responseBody)
            try auditWriteAll(response, to: peer)
            barrier.complete()
        }
    }

    private func record(_ message: String) {
        lock.lock(); failures.append(message); lock.unlock()
    }

    private func writeAuditResponse(
        _ requestHeader: Data,
        to peer: Int32,
        status: Int32,
        body: Data,
        minor: UInt16
    ) throws {
        var response = Data()
        auditAppend(UInt32(0x4653_4249), to: &response)
        auditAppend(ipcProtocolMajor, to: &response)
        auditAppend(minor, to: &response)
        response.append(requestHeader[8..<16])
        auditAppend(status, to: &response)
        auditAppend(UInt32(body.count), to: &response)
        response.append(body)
        try auditWriteAll(response, to: peer)
    }
}

func auditTemporaryDirectory() -> URL {
    let path = ProcessInfo.processInfo.environment["FSK_AUDIT_TMP"] ?? NSTemporaryDirectory()
    if path.hasPrefix("/") {
        return URL(fileURLWithPath: path, isDirectory: true).standardizedFileURL
    }
    return URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true)
        .appending(path: path)
        .standardizedFileURL
}

func withAuditBackend(
    _ script: [[AuditIPCScript]],
    requireComplete: Bool = true,
    _ body: (RustBackend) throws -> Void
) throws {
    let server = try AuditIPCServer(scripts: script)
    server.start()
    defer { server.stop() }
    let backend = try RustBackend(
        resourceURL: server.resourceURL,
        requestTimeout: auditIPCRequestTimeout
    )
    try body(backend)
    if requireComplete { try server.wait() }
}

func auditHello(minor: UInt16 = 1) -> AuditIPCScript {
    .init(opcode: 0, expectedRequestBody: Data(repeating: 0, count: 32), replyMinor: minor)
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
