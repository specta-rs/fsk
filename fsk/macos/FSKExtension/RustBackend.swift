// High-throughput IPC client for Rust filesystem processes.
import Darwin
import Foundation

private let ipcMagic: UInt32 = 0x4653_4249
// Keep these in sync with `PROTOCOL_MAJOR` and `PROTOCOL_MINOR` in
// `fsk/macos/rust/ipc.rs`; the Rust tests check the declaration text.
let ipcProtocolMajor: UInt16 = 1
let ipcProtocolMinor: UInt16 = 2
private let ipcHeaderLength = 24
private let ipcMaximumFrame = 16 * 1024 * 1024
// Two UInt64s, one UInt32, one UInt16, and one UInt8 on the wire.
private let ipcDirectoryEntryWireSize = 23
let fskDirectoryCookieBias: UInt64 = 2

private enum IPCOperation: UInt16 {
    case hello = 0
    case configuration = 1
    case statfs = 2
    case getattr = 3
    case setattr = 4
    case lookup = 5
    case readdir = 6
    case read = 7
    case write = 8
    case create = 9
    case symlink = 10
    case readlink = 11
    case remove = 12
    case rename = 13
    case sync = 14
    case open = 15
    case close = 16
    case acknowledgeLease = 17
}

enum RustBackendError: Error, Equatable, CustomStringConvertible {
    case posix(Int32)
    case invalidResource
    case invalidProtocol
    case protocolMismatch(
        serverMajor: UInt16,
        serverMinor: UInt16,
        clientMajor: UInt16,
        clientMinor: UInt16
    )
    case protocolTooOld(required: UInt16, negotiated: UInt16)

    var asPOSIXError: POSIXError {
        let code: POSIXErrorCode = POSIXErrorCode(rawValue: posixCode) ?? .EIO
        return POSIXError(code)
    }

    var posixCode: Int32 {
        switch self {
        case let .posix(code): code
        case .invalidResource: EINVAL
        case .invalidProtocol, .protocolMismatch, .protocolTooOld: EPROTO
        }
    }

    var description: String {
        switch self {
        case let .posix(code):
            "POSIX error \(code)"
        case .invalidResource:
            "invalid FSK resource"
        case .invalidProtocol:
            "invalid FSK protocol frame"
        case let .protocolMismatch(serverMajor, serverMinor, clientMajor, clientMinor):
            "FSK.app (protocol \(clientMajor).\(clientMinor)) and this SDK (protocol \(serverMajor).\(serverMinor)) speak incompatible protocol versions; install or update FSK.app, or use a matching SDK"
        case let .protocolTooOld(required, negotiated):
            "FSK.app negotiated protocol revision \(negotiated) but this operation requires \(required); install or update FSK.app"
        }
    }
}

struct FSKAttributes {
    var inode: UInt64 = 0
    var parent: UInt64 = 0
    var size: UInt64 = 0
    var allocated_size: UInt64 = 0
    var generation: UInt64 = 0
    var created_ns: UInt64 = 0
    var modified_ns: UInt64 = 0
    var accessed_ns: UInt64 = 0
    var mode: UInt32 = 0
    var uid: UInt32 = 0
    var gid: UInt32 = 0
    var link_count: UInt32 = 0
    var kind: UInt8 = 0
}

struct FSKSetAttributes {
    var valid: UInt32 = 0
    var mode: UInt32 = 0
    var uid: UInt32 = 0
    var gid: UInt32 = 0
    var size: UInt64 = 0
    var accessed_ns: UInt64 = 0
    var modified_ns: UInt64 = 0
}

struct FSKStatFS {
    var block_size: UInt32 = 0
    var io_size: UInt32 = 0
    var total_bytes: UInt64 = 0
    var free_bytes: UInt64 = 0
    var available_bytes: UInt64 = 0
    var files: UInt64 = 0
    var free_files: UInt64 = 0
}

struct FSKDirEntry {
    var inode: UInt64 = 0
    var next_cookie: UInt64 = 0
    var name_offset: UInt32 = 0
    var name_length: UInt16 = 0
    var kind: UInt8 = 0
}

let FSK_KIND_FILE: UInt8 = 1
let FSK_KIND_DIRECTORY: UInt8 = 2
let FSK_KIND_SYMLINK: UInt8 = 3
let FSK_SET_SIZE: UInt32 = 1 << 0
let FSK_SET_MODE: UInt32 = 1 << 1
let FSK_SET_UID: UInt32 = 1 << 2
let FSK_SET_GID: UInt32 = 1 << 3
let FSK_SET_ACCESSED_NS: UInt32 = 1 << 4
let FSK_SET_MODIFIED_NS: UInt32 = 1 << 5

struct RustDirectoryBatch {
    let entries: [FSKDirEntry]
    let names: Data
    let verifier: UInt64
    let nextCookie: UInt64
    let eof: Bool

    func name(for entry: FSKDirEntry) -> Data {
        let start = Int(entry.name_offset)
        let end = start + Int(entry.name_length)
        return names.subdata(in: start..<end)
    }

    /// Whether the page either ends the directory or advances enumeration.
    ///
    /// An empty page that isn't the end gives FSKit no new `nextCookie`, so it
    /// would request the same cookie forever.
    var madeProgress: Bool {
        eof || !entries.isEmpty
    }
}

struct RustBackendConfiguration: Sendable {
    let isNetwork: Bool
    let isLazy: Bool
    let supportsLocalFiles: Bool
    let kernelCachePolicy: UInt8
}

struct RustOpenFile: Sendable {
    let lease: UInt64
    let generation: UInt64
    let localPath: Data?
}

final class RustOpenHandle: @unchecked Sendable {
    let file: RustOpenFile
    private let pool: IPCConnectionPool
    private let lock = NSLock()
    private var connection: IPCConnection?
    private var failed = false

    fileprivate init(file: RustOpenFile, pool: IPCConnectionPool, connection: IPCConnection) {
        self.file = file
        self.pool = pool
        self.connection = connection
    }

    func acknowledgeLease() throws {
        lock.lock()
        defer { lock.unlock() }
        guard let connection else { throw RustBackendError.invalidProtocol }
        guard connection.negotiatedMinor >= 2 else { return }
        var request = IPCEncoder(capacity: 8)
        request.append(file.lease)
        do {
            let response = try connection.request(.acknowledgeLease, request.data).get()
            guard response.isEmpty else { throw RustBackendError.invalidProtocol }
        } catch {
            failed = true
            throw error
        }
    }

    func reject() throws {
        lock.lock()
        let connection = takeConnectionLocked()
        lock.unlock()
        guard let connection else { return }
        defer { pool.checkin(connection, reusable: false) }
        var request = IPCEncoder(capacity: 8)
        request.append(file.lease)
        _ = try connection.request(.close, request.data).get()
    }

    func finish() {
        lock.lock()
        let connection = takeConnectionLocked()
        let reusable = !failed
        lock.unlock()
        if let connection {
            pool.checkin(connection, reusable: reusable)
        }
    }

    func invalidate() {
        lock.lock()
        let connection = takeConnectionLocked()
        lock.unlock()
        if let connection {
            pool.checkin(connection, reusable: false)
        }
    }

    deinit { invalidate() }

    private func takeConnectionLocked() -> IPCConnection? {
        let connection = connection
        self.connection = nil
        return connection
    }
}

/// A pooled client for one Rust process. Each checked-out connection processes
/// one request at a time, while independent FSKit callbacks run in parallel.
final class RustBackend: @unchecked Sendable {
    static let defaultRequestTimeout: TimeInterval = 30

    private let pool: IPCConnectionPool
    let configuration: RustBackendConfiguration

    static func isValidResource(_ resourceURL: URL) -> Bool {
        (try? resourceParameters(resourceURL)) != nil
    }

    init(
        resourceURL: URL,
        requestTimeout: TimeInterval = RustBackend.defaultRequestTimeout
    ) throws {
        guard requestTimeout.isFinite, requestTimeout > 0 else {
            throw RustBackendError.posix(EINVAL)
        }
        let (socketPath, token) = try Self.resourceParameters(resourceURL)
        pool = IPCConnectionPool(socketPath: socketPath, token: token, requestTimeout: requestTimeout)
        let response = try pool.request(.configuration)
        var decoder = IPCDecoder(response)
        configuration = RustBackendConfiguration(
            isNetwork: try decoder.uint8() != 0,
            isLazy: try decoder.uint8() != 0,
            supportsLocalFiles: try decoder.uint8() != 0,
            kernelCachePolicy: try decoder.uint8()
        )
    }

    private static func resourceParameters(_ resourceURL: URL) throws -> (socketPath: String, token: Data) {
        guard resourceURL.scheme?.lowercased() == "fsk",
              let components = URLComponents(url: resourceURL, resolvingAgainstBaseURL: false),
              let socketPath = components.queryItems?.first(where: { $0.name == "socket" })?.value,
              !socketPath.isEmpty,
              let tokenHex = components.queryItems?.first(where: { $0.name == "token" })?.value,
              let token = Data(hex: tokenHex),
              token.count == 32
        else {
            throw RustBackendError.invalidResource
        }
        _ = try unixSocketPathBytes(socketPath)
        return (socketPath, token)
    }

    func statfs() throws -> FSKStatFS {
        var decoder = IPCDecoder(try pool.request(.statfs))
        return FSKStatFS(
            block_size: try decoder.uint32(),
            io_size: try decoder.uint32(),
            total_bytes: try decoder.uint64(),
            free_bytes: try decoder.uint64(),
            available_bytes: try decoder.uint64(),
            files: try decoder.uint64(),
            free_files: try decoder.uint64()
        )
    }

    func attributes(inode: UInt64) throws -> FSKAttributes {
        var request = IPCEncoder(capacity: 8)
        request.append(inode)
        return try decodeAttributes(pool.request(.getattr, request.data))
    }

    func setAttributes(inode: UInt64, update: FSKSetAttributes) throws -> FSKAttributes {
        var request = IPCEncoder(capacity: 48)
        request.append(inode)
        request.append(update.valid)
        request.append(update.mode)
        request.append(update.uid)
        request.append(update.gid)
        request.append(update.size)
        request.append(update.accessed_ns)
        request.append(update.modified_ns)
        return try decodeAttributes(pool.request(.setattr, request.data))
    }

    func lookup(parent: UInt64, name: Data) throws -> (inode: UInt64, kind: UInt8) {
        var request = IPCEncoder(capacity: 8 + name.count)
        request.append(parent)
        request.append(name)
        var decoder = IPCDecoder(try pool.request(.lookup, request.data))
        return (try decoder.uint64(), try decoder.uint8())
    }

    func readDirectory(
        inode: UInt64,
        cookie: UInt64,
        verifier: UInt64,
        entryCapacity: Int = 512,
        nameCapacity: Int = 64 * 1024
    ) throws -> RustDirectoryBatch {
        var request = IPCEncoder(capacity: 32)
        request.append(inode)
        request.append(cookie)
        request.append(verifier)
        request.append(UInt32(entryCapacity))
        request.append(UInt32(nameCapacity))
        let response = try pool.request(.readdir, request.data)
        var decoder = IPCDecoder(response)
        let resultVerifier = try decoder.uint64()
        let nextCookie = try decoder.uint64()
        let eof = try decoder.uint8() != 0
        let entryCount = Int(try decoder.uint32())
        let nameCount = Int(try decoder.uint32())
        guard entryCount <= decoder.remainingCount / ipcDirectoryEntryWireSize else {
            throw RustBackendError.invalidProtocol
        }
        var entries = [FSKDirEntry]()
        entries.reserveCapacity(entryCount)
        for _ in 0..<entryCount {
            entries.append(
                FSKDirEntry(
                    inode: try decoder.uint64(),
                    next_cookie: try decoder.uint64(),
                    name_offset: try decoder.uint32(),
                    name_length: try decoder.uint16(),
                    kind: try decoder.uint8()
                )
            )
        }
        let names = try decoder.data(count: nameCount)
        for entry in entries {
            let start = Int(entry.name_offset)
            guard start <= names.count,
                  Int(entry.name_length) <= names.count - start
            else {
                throw RustBackendError.invalidProtocol
            }
        }
        guard entries.allSatisfy({ $0.next_cookie > cookie }) else {
            throw RustBackendError.posix(ERANGE)
        }
        guard entries.allSatisfy({ $0.next_cookie <= UInt64.max - fskDirectoryCookieBias }) else {
            throw RustBackendError.posix(EOVERFLOW)
        }
        let batch = RustDirectoryBatch(
            entries: entries,
            names: names,
            verifier: resultVerifier,
            nextCookie: nextCookie,
            eof: eof
        )
        guard batch.madeProgress else {
            // The server refused every entry without reaching the end (for
            // example because none fit). `fsk_readdir` reports that as
            // `ERANGE` so the caller grows its buffers instead of looping on
            // the same cookie; mirror it over IPC.
            throw RustBackendError.posix(ERANGE)
        }
        return batch
    }

    func read(inode: UInt64, offset: UInt64, into buffer: UnsafeMutableRawBufferPointer) throws -> Int {
        var request = IPCEncoder(capacity: 20)
        request.append(inode)
        request.append(offset)
        request.append(UInt32(buffer.count))
        let response = try pool.request(.read, request.data)
        guard response.count <= buffer.count else { throw RustBackendError.invalidProtocol }
        _ = response.copyBytes(to: buffer.bindMemory(to: UInt8.self))
        return response.count
    }

    func write(inode: UInt64, offset: UInt64, data: Data) throws -> Int {
        var request = IPCEncoder(capacity: 16 + data.count)
        request.append(inode)
        request.append(offset)
        request.append(data)
        var decoder = IPCDecoder(try pool.request(.write, request.data))
        let rawCount = try decoder.uint64()
        guard rawCount <= UInt64(data.count), let count = Int(exactly: rawCount) else {
            throw RustBackendError.invalidProtocol
        }
        return count
    }

    func create(parent: UInt64, name: Data, kind: UInt8, mode: UInt32) throws -> UInt64 {
        var request = IPCEncoder(capacity: 13 + name.count)
        request.append(parent)
        request.append(kind)
        request.append(mode)
        request.append(name)
        var decoder = IPCDecoder(try pool.request(.create, request.data))
        return try decoder.uint64()
    }

    func symlink(parent: UInt64, name: Data, target: Data, mode: UInt32) throws -> UInt64 {
        var request = IPCEncoder(capacity: 16 + name.count + target.count)
        request.append(parent)
        request.append(mode)
        request.append(UInt32(name.count))
        request.append(name)
        request.append(target)
        var decoder = IPCDecoder(try pool.request(.symlink, request.data))
        return try decoder.uint64()
    }

    func readlink(inode: UInt64, capacity: Int = 4096) throws -> Data {
        var request = IPCEncoder(capacity: 12)
        request.append(inode)
        request.append(UInt32(capacity))
        let response = try pool.request(.readlink, request.data)
        guard response.count <= capacity else { throw RustBackendError.invalidProtocol }
        return response
    }

    func remove(parent: UInt64, name: Data, inode: UInt64) throws {
        var request = IPCEncoder(capacity: 16 + name.count)
        request.append(parent)
        request.append(inode)
        request.append(name)
        _ = try pool.request(.remove, request.data)
    }

    func rename(
        sourceParent: UInt64,
        sourceName: Data,
        destinationParent: UInt64,
        destinationName: Data,
        replacedInode: UInt64?
    ) throws {
        var request = IPCEncoder(capacity: 28 + sourceName.count + destinationName.count)
        request.append(sourceParent)
        request.append(destinationParent)
        request.append(replacedInode ?? 0)
        request.append(UInt32(sourceName.count))
        request.append(sourceName)
        request.append(destinationName)
        _ = try pool.request(.rename, request.data)
    }

    func synchronize() throws {
        _ = try pool.request(.sync)
    }

    func open(inode: UInt64, writable: Bool) throws -> RustOpenHandle {
        try pool.open(inode: inode, writable: writable)
    }

    func close(lease: UInt64) throws {
        var request = IPCEncoder(capacity: 8)
        request.append(lease)
        _ = try pool.request(.close, request.data)
    }
}

private func decodeAttributes(_ data: Data) throws -> FSKAttributes {
    var decoder = IPCDecoder(data)
    return FSKAttributes(
        inode: try decoder.uint64(),
        parent: try decoder.uint64(),
        size: try decoder.uint64(),
        allocated_size: try decoder.uint64(),
        generation: try decoder.uint64(),
        created_ns: try decoder.uint64(),
        modified_ns: try decoder.uint64(),
        accessed_ns: try decoder.uint64(),
        mode: try decoder.uint32(),
        uid: try decoder.uint32(),
        gid: try decoder.uint32(),
        link_count: try decoder.uint32(),
        kind: try decoder.uint8()
    )
}

private final class IPCConnectionPool: @unchecked Sendable {
    private let socketPath: String
    private let token: Data
    private let requestTimeout: TimeInterval
    private let condition = NSCondition()
    private var idle = [IPCConnection]()
    private var connectionCount = 0
    private let maximumConnections = max(4, min(32, ProcessInfo.processInfo.activeProcessorCount))

    init(socketPath: String, token: Data, requestTimeout: TimeInterval) {
        self.socketPath = socketPath
        self.token = token
        self.requestTimeout = requestTimeout
    }

    /// Send a request. `minimumMinor` is the protocol revision the operation
    /// needs; a connection that negotiated less throws `protocolTooOld` instead
    /// of sending a frame an older SDK cannot interpret.
    func request(
        _ operation: IPCOperation,
        _ body: Data = Data(),
        minimumMinor: UInt16 = 0
    ) throws -> Data {
        let connection = try checkout()
        var reusable = false
        defer { checkin(connection, reusable: reusable) }
        // A thrown exchange failure may leave the stream out of sync. Drop
        // the socket without replaying the request: the server may have run it.
        let result = try connection.request(operation, body, minimumMinor: minimumMinor)
        reusable = true
        return try result.get()
    }

    private func checkout() throws -> IPCConnection {
        condition.lock()
        while true {
            if let connection = idle.popLast() {
                condition.unlock()
                return connection
            }
            if connectionCount < maximumConnections {
                connectionCount += 1
                condition.unlock()
                do {
                    return try IPCConnection(
                        socketPath: socketPath,
                        token: token,
                        requestTimeout: requestTimeout
                    )
                } catch {
                    condition.lock()
                    connectionCount -= 1
                    condition.broadcast()
                    condition.unlock()
                    throw error
                }
            }
            condition.wait()
        }
    }

    fileprivate func checkin(_ connection: IPCConnection, reusable: Bool) {
        condition.lock()
        if reusable {
            idle.append(connection)
        } else {
            connectionCount -= 1
        }
        condition.signal()
        condition.unlock()
    }

    func open(inode: UInt64, writable: Bool) throws -> RustOpenHandle {
        let connection = try checkout()
        do {
            var request = IPCEncoder(capacity: 9)
            request.append(inode)
            request.append(writable ? UInt8(1) : UInt8(0))
            var decoder = IPCDecoder(try connection.request(.open, request.data).get())
            let lease = try decoder.uint64()
            let source = try decoder.uint8()
            let generation = try decoder.uint64()
            let localPath = source == 1 ? decoder.remainingData() : nil
            return RustOpenHandle(
                file: RustOpenFile(lease: lease, generation: generation, localPath: localPath),
                pool: self,
                connection: connection
            )
        } catch {
            checkin(connection, reusable: false)
            throw error
        }
    }
}

private final class IPCConnection {
    private var descriptor: Int32
    private let requestTimeout: TimeInterval
    private var nextRequestID: UInt64 = 2
    /// Protocol revision agreed during `hello`: the lower of the two sides.
    private(set) var negotiatedMinor: UInt16 = 0

    init(socketPath: String, token: Data, requestTimeout: TimeInterval) throws {
        self.requestTimeout = requestTimeout
        descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw RustBackendError.posix(errno) }
        do {
            let seconds = Int(requestTimeout)
            let microseconds = Int((requestTimeout - TimeInterval(seconds)) * 1_000_000)
            var timeout = timeval(tv_sec: seconds, tv_usec: Int32(microseconds))
            for option in [SO_RCVTIMEO, SO_SNDTIMEO] {
                guard setsockopt(
                    descriptor,
                    SOL_SOCKET,
                    option,
                    &timeout,
                    socklen_t(MemoryLayout<timeval>.size)
                ) == 0 else {
                    throw RustBackendError.posix(errno)
                }
            }
            try connect(path: socketPath)
            let hello = try requestRaw(.hello, token, requestID: 1)
            _ = try hello.result.get()
            negotiatedMinor = hello.minor
        } catch {
            Darwin.close(descriptor)
            descriptor = -1
            throw error
        }
    }

    deinit {
        if descriptor >= 0 {
            Darwin.close(descriptor)
        }
    }

    /// Returns errors from a complete reply or local validation as a result;
    /// throws only when the stream can no longer be reused.
    func request(
        _ operation: IPCOperation,
        _ body: Data = Data(),
        minimumMinor: UInt16 = 0
    ) throws -> Result<Data, RustBackendError> {
        guard negotiatedMinor >= minimumMinor else {
            return .failure(.protocolTooOld(
                required: minimumMinor,
                negotiated: negotiatedMinor
            ))
        }
        guard body.count <= ipcMaximumFrame else { return .failure(.posix(E2BIG)) }
        let requestID = nextRequestID
        nextRequestID &+= 1
        return try requestRaw(operation, body, requestID: requestID).result
    }

    private func requestRaw(
        _ operation: IPCOperation,
        _ body: Data,
        requestID: UInt64
    ) throws -> (result: Result<Data, RustBackendError>, minor: UInt16) {
        var header = IPCEncoder(capacity: ipcHeaderLength)
        header.append(ipcMagic)
        header.append(ipcProtocolMajor)
        header.append(operation.rawValue)
        header.append(requestID)
        header.append(UInt32(body.count))
        header.append(ipcProtocolMinor)
        header.append(UInt16(0))
        let deadline = DispatchTime.now() + requestTimeout
        try writePair(header.data, body, deadline: deadline)

        let responseHeader = try readExactly(ipcHeaderLength, deadline: deadline)
        var decoder = IPCDecoder(responseHeader)
        guard try decoder.uint32() == ipcMagic else {
            throw RustBackendError.invalidProtocol
        }
        let serverMajor = try decoder.uint16()
        let serverMinor = try decoder.uint16()
        guard serverMajor == ipcProtocolMajor else {
            throw RustBackendError.protocolMismatch(
                serverMajor: serverMajor,
                serverMinor: serverMinor,
                clientMajor: ipcProtocolMajor,
                clientMinor: ipcProtocolMinor
            )
        }
        guard try decoder.uint64() == requestID else { throw RustBackendError.invalidProtocol }
        let status = try decoder.int32()
        let length = Int(try decoder.uint32())
        guard length <= ipcMaximumFrame else { throw RustBackendError.invalidProtocol }
        let response = try readExactly(length, deadline: deadline)
        return (status == 0 ? .success(response) : .failure(.posix(status)), serverMinor)
    }

    private func connect(path: String) throws {
        var noSignal: Int32 = 1
        guard setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSignal,
            socklen_t(MemoryLayout<Int32>.size)
        ) == 0 else {
            throw RustBackendError.posix(errno)
        }
        var (address, length) = try unixSocketAddress(path: path)
        let status = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(descriptor, $0, length)
            }
        }
        guard status == 0 else { throw RustBackendError.posix(errno) }
    }

    private func writeAll(_ data: Data, deadline: DispatchTime) throws {
        try data.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                try waitUntilReady(Int16(POLLOUT), deadline: deadline)
                let count = Darwin.write(descriptor, raw.baseAddress! + offset, raw.count - offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { throw RustBackendError.posix(errno == 0 ? EPIPE : errno) }
                offset += count
            }
        }
    }

    private func writePair(_ header: Data, _ body: Data, deadline: DispatchTime) throws {
        if body.isEmpty {
            try writeAll(header, deadline: deadline)
            return
        }
        try header.withUnsafeBytes { headerBytes in
            try body.withUnsafeBytes { bodyBytes in
                var vectors = [
                    iovec(
                        iov_base: UnsafeMutableRawPointer(mutating: headerBytes.baseAddress),
                        iov_len: headerBytes.count
                    ),
                    iovec(
                        iov_base: UnsafeMutableRawPointer(mutating: bodyBytes.baseAddress),
                        iov_len: bodyBytes.count
                    ),
                ]
                try waitUntilReady(Int16(POLLOUT), deadline: deadline)
                let count = Darwin.writev(descriptor, &vectors, 2)
                if count < 0 && errno == EINTR {
                    try writeAll(header, deadline: deadline)
                    try writeAll(body, deadline: deadline)
                    return
                }
                guard count > 0 else {
                    throw RustBackendError.posix(errno == 0 ? EPIPE : errno)
                }
                if count < headerBytes.count {
                    try writeAll(header.subdata(in: count..<headerBytes.count), deadline: deadline)
                    try writeAll(body, deadline: deadline)
                } else {
                    let bodyOffset = count - headerBytes.count
                    if bodyOffset < bodyBytes.count {
                        try writeAll(body.subdata(in: bodyOffset..<bodyBytes.count), deadline: deadline)
                    }
                }
            }
        }
    }

    private func readExactly(_ length: Int, deadline: DispatchTime) throws -> Data {
        if length == 0 { return Data() }
        var data = Data(count: length)
        try data.withUnsafeMutableBytes { raw in
            var offset = 0
            while offset < raw.count {
                try waitUntilReady(Int16(POLLIN), deadline: deadline)
                let count = Darwin.read(descriptor, raw.baseAddress! + offset, raw.count - offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { throw RustBackendError.posix(errno == 0 ? ECONNRESET : errno) }
                offset += count
            }
        }
        return data
    }

    private func waitUntilReady(_ events: Int16, deadline: DispatchTime) throws {
        while true {
            let now = DispatchTime.now()
            guard now.uptimeNanoseconds < deadline.uptimeNanoseconds else {
                throw RustBackendError.posix(ETIMEDOUT)
            }
            let remaining = Double(deadline.uptimeNanoseconds - now.uptimeNanoseconds) / 1_000_000
            let timeout = Int32(min(ceil(remaining), Double(Int32.max)))
            var descriptor = pollfd(fd: self.descriptor, events: events, revents: 0)
            let result = Darwin.poll(&descriptor, 1, timeout)
            if result < 0 {
                if errno == EINTR { continue }
                throw RustBackendError.posix(errno)
            }
            guard result > 0 else { throw RustBackendError.posix(ETIMEDOUT) }
            if descriptor.revents & events != 0 { return }
            if descriptor.revents & Int16(POLLHUP) != 0 {
                throw RustBackendError.posix(ECONNRESET)
            }
            if descriptor.revents & Int16(POLLERR | POLLNVAL) != 0 { return }
        }
    }
}

func unixSocketAddress(path: String) throws -> (sockaddr_un, socklen_t) {
    let bytes = try unixSocketPathBytes(path)
    let pathOffset = MemoryLayout<sockaddr_un>.offset(of: \.sun_path)!
    let length = pathOffset + bytes.count - 1
    var address = sockaddr_un()
    address.sun_len = UInt8(length)
    address.sun_family = sa_family_t(AF_UNIX)
    withUnsafeMutableBytes(of: &address) { destination in
        bytes.withUnsafeBytes { source in
            destination[pathOffset..<(pathOffset + bytes.count)].copyBytes(from: source)
        }
    }
    return (address, socklen_t(length))
}

private func unixSocketPathBytes(_ path: String) throws -> [CChar] {
    guard !path.utf8.contains(0) else { throw RustBackendError.posix(EINVAL) }
    let bytes = Array(path.utf8CString)
    let pathOffset = MemoryLayout<sockaddr_un>.offset(of: \.sun_path)!
    guard bytes.count <= MemoryLayout.size(ofValue: sockaddr_un().sun_path) else {
        throw RustBackendError.posix(ENAMETOOLONG)
    }
    let length = pathOffset + bytes.count - 1
    guard length <= UInt8.max else {
        throw RustBackendError.posix(ENAMETOOLONG)
    }
    return bytes
}

private struct IPCEncoder {
    private(set) var data: Data

    init(capacity: Int) {
        data = Data()
        data.reserveCapacity(capacity)
    }

    mutating func append<T: FixedWidthInteger>(_ value: T) {
        var littleEndian = value.littleEndian
        withUnsafeBytes(of: &littleEndian) { data.append(contentsOf: $0) }
    }

    mutating func append(_ value: Data) {
        data.append(value)
    }
}

private struct IPCDecoder {
    private let data: Data
    private var offset = 0

    var remainingCount: Int { data.count - offset }

    init(_ data: Data) {
        self.data = data
    }

    mutating func uint8() throws -> UInt8 { try integer() }
    mutating func uint16() throws -> UInt16 { try integer() }
    mutating func uint32() throws -> UInt32 { try integer() }
    mutating func int32() throws -> Int32 { try integer() }
    mutating func uint64() throws -> UInt64 { try integer() }

    mutating func data(count: Int) throws -> Data {
        guard count >= 0, offset <= data.count - count else {
            throw RustBackendError.invalidProtocol
        }
        defer { offset += count }
        return data.subdata(in: offset..<(offset + count))
    }

    mutating func remainingData() -> Data {
        defer { offset = data.count }
        return data.subdata(in: offset..<data.count)
    }

    private mutating func integer<T: FixedWidthInteger>() throws -> T {
        let size = MemoryLayout<T>.size
        guard offset <= data.count - size else { throw RustBackendError.invalidProtocol }
        let value = data.withUnsafeBytes {
            $0.loadUnaligned(fromByteOffset: offset, as: T.self)
        }
        offset += size
        return T(littleEndian: value)
    }
}

private extension Data {
    init?(hex: String) {
        guard hex.count.isMultiple(of: 2) else { return nil }
        self.init(capacity: hex.count / 2)
        var index = hex.startIndex
        while index < hex.endIndex {
            let next = hex.index(index, offsetBy: 2)
            guard let byte = UInt8(hex[index..<next], radix: 16) else { return nil }
            append(byte)
            index = next
        }
    }
}
