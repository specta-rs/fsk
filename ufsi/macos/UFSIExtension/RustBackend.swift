// High-throughput IPC client for Rust filesystem processes.
import Darwin
import Foundation

private let ipcMagic: UInt32 = 0x4653_4249
private let ipcVersion: UInt16 = 1
private let ipcHeaderLength = 24
private let ipcMaximumFrame = 16 * 1024 * 1024

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
}

enum RustBackendError: Error, Equatable {
    case posix(Int32)
    case invalidResource
    case invalidProtocol

    var asPOSIXError: POSIXError {
        let code: POSIXErrorCode = POSIXErrorCode(rawValue: posixCode) ?? .EIO
        return POSIXError(code)
    }

    var posixCode: Int32 {
        switch self {
        case let .posix(code): code
        case .invalidResource: EINVAL
        case .invalidProtocol: EPROTO
        }
    }
}

struct UFSIAttributes {
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

struct UFSISetAttributes {
    var valid: UInt32 = 0
    var mode: UInt32 = 0
    var uid: UInt32 = 0
    var gid: UInt32 = 0
    var size: UInt64 = 0
    var accessed_ns: UInt64 = 0
    var modified_ns: UInt64 = 0
}

struct UFSIStatFS {
    var block_size: UInt32 = 0
    var io_size: UInt32 = 0
    var total_bytes: UInt64 = 0
    var free_bytes: UInt64 = 0
    var available_bytes: UInt64 = 0
    var files: UInt64 = 0
    var free_files: UInt64 = 0
}

struct UFSIDirEntry {
    var inode: UInt64 = 0
    var next_cookie: UInt64 = 0
    var name_offset: UInt32 = 0
    var name_length: UInt16 = 0
    var kind: UInt8 = 0
}

let UFSI_KIND_FILE: UInt8 = 1
let UFSI_KIND_DIRECTORY: UInt8 = 2
let UFSI_KIND_SYMLINK: UInt8 = 3
let UFSI_SET_SIZE: UInt32 = 1 << 0
let UFSI_SET_MODE: UInt32 = 1 << 1
let UFSI_SET_UID: UInt32 = 1 << 2
let UFSI_SET_GID: UInt32 = 1 << 3
let UFSI_SET_ACCESSED_NS: UInt32 = 1 << 4
let UFSI_SET_MODIFIED_NS: UInt32 = 1 << 5

struct RustDirectoryBatch {
    let entries: [UFSIDirEntry]
    let names: Data
    let verifier: UInt64
    let nextCookie: UInt64
    let eof: Bool

    func name(for entry: UFSIDirEntry) -> Data {
        let start = Int(entry.name_offset)
        let end = start + Int(entry.name_length)
        return names.subdata(in: start..<end)
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

/// A pooled client for one Rust process. Each checked-out connection processes
/// one request at a time, while independent FSKit callbacks run in parallel.
final class RustBackend: @unchecked Sendable {
    private let pool: IPCConnectionPool
    let configuration: RustBackendConfiguration

    init(resourceURL: URL) throws {
        guard resourceURL.scheme?.lowercased() == "ufsi",
              let components = URLComponents(url: resourceURL, resolvingAgainstBaseURL: false),
              let socketPath = components.queryItems?.first(where: { $0.name == "socket" })?.value,
              let tokenHex = components.queryItems?.first(where: { $0.name == "token" })?.value,
              let token = Data(hex: tokenHex),
              token.count == 32
        else {
            throw RustBackendError.invalidResource
        }
        pool = IPCConnectionPool(socketPath: socketPath, token: token)
        let response = try pool.request(.configuration)
        var decoder = IPCDecoder(response)
        configuration = RustBackendConfiguration(
            isNetwork: try decoder.uint8() != 0,
            isLazy: try decoder.uint8() != 0,
            supportsLocalFiles: try decoder.uint8() != 0,
            kernelCachePolicy: try decoder.uint8()
        )
    }

    func statfs() throws -> UFSIStatFS {
        var decoder = IPCDecoder(try pool.request(.statfs))
        return UFSIStatFS(
            block_size: try decoder.uint32(),
            io_size: try decoder.uint32(),
            total_bytes: try decoder.uint64(),
            free_bytes: try decoder.uint64(),
            available_bytes: try decoder.uint64(),
            files: try decoder.uint64(),
            free_files: try decoder.uint64()
        )
    }

    func attributes(inode: UInt64) throws -> UFSIAttributes {
        var request = IPCEncoder(capacity: 8)
        request.append(inode)
        return try decodeAttributes(pool.request(.getattr, request.data))
    }

    func setAttributes(inode: UInt64, update: UFSISetAttributes) throws -> UFSIAttributes {
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
        var entries = [UFSIDirEntry]()
        entries.reserveCapacity(entryCount)
        for _ in 0..<entryCount {
            entries.append(
                UFSIDirEntry(
                    inode: try decoder.uint64(),
                    next_cookie: try decoder.uint64(),
                    name_offset: try decoder.uint32(),
                    name_length: try decoder.uint16(),
                    kind: try decoder.uint8()
                )
            )
        }
        let names = try decoder.data(count: nameCount)
        return RustDirectoryBatch(
            entries: entries,
            names: names,
            verifier: resultVerifier,
            nextCookie: nextCookie,
            eof: eof
        )
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
        return Int(try decoder.uint64())
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
        return try pool.request(.readlink, request.data)
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

    func open(inode: UInt64, writable: Bool) throws -> RustOpenFile {
        var request = IPCEncoder(capacity: 9)
        request.append(inode)
        request.append(writable ? UInt8(1) : UInt8(0))
        var decoder = IPCDecoder(try pool.request(.open, request.data))
        let lease = try decoder.uint64()
        let source = try decoder.uint8()
        let generation = try decoder.uint64()
        let localPath = source == 1 ? decoder.remainingData() : nil
        return RustOpenFile(lease: lease, generation: generation, localPath: localPath)
    }

    func close(lease: UInt64) throws {
        var request = IPCEncoder(capacity: 8)
        request.append(lease)
        _ = try pool.request(.close, request.data)
    }
}

private func decodeAttributes(_ data: Data) throws -> UFSIAttributes {
    var decoder = IPCDecoder(data)
    return UFSIAttributes(
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
    private let condition = NSCondition()
    private var idle = [IPCConnection]()
    private var connectionCount = 0
    private let maximumConnections = max(4, min(32, ProcessInfo.processInfo.activeProcessorCount))

    init(socketPath: String, token: Data) {
        self.socketPath = socketPath
        self.token = token
    }

    func request(_ operation: IPCOperation, _ body: Data = Data()) throws -> Data {
        let connection = try checkout()
        var reusable = true
        defer { checkin(connection, reusable: reusable) }
        do {
            return try connection.request(operation, body)
        } catch {
            reusable = false
            throw error
        }
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
                    return try IPCConnection(socketPath: socketPath, token: token)
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

    private func checkin(_ connection: IPCConnection, reusable: Bool) {
        condition.lock()
        if reusable {
            idle.append(connection)
        } else {
            connectionCount -= 1
        }
        condition.signal()
        condition.unlock()
    }
}

private final class IPCConnection {
    private var descriptor: Int32
    private var nextRequestID: UInt64 = 2

    init(socketPath: String, token: Data) throws {
        descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw RustBackendError.posix(errno) }
        do {
            try connect(path: socketPath)
            _ = try request(.hello, token, requestID: 1)
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

    func request(_ operation: IPCOperation, _ body: Data) throws -> Data {
        let requestID = nextRequestID
        nextRequestID &+= 1
        return try request(operation, body, requestID: requestID)
    }

    private func request(_ operation: IPCOperation, _ body: Data, requestID: UInt64) throws -> Data {
        guard body.count <= ipcMaximumFrame else { throw RustBackendError.posix(E2BIG) }
        var header = IPCEncoder(capacity: ipcHeaderLength)
        header.append(ipcMagic)
        header.append(ipcVersion)
        header.append(operation.rawValue)
        header.append(requestID)
        header.append(UInt32(body.count))
        header.append(UInt32(0))
        try writePair(header.data, body)

        let responseHeader = try readExactly(ipcHeaderLength)
        var decoder = IPCDecoder(responseHeader)
        guard try decoder.uint32() == ipcMagic,
              try decoder.uint16() == ipcVersion
        else {
            throw RustBackendError.invalidProtocol
        }
        _ = try decoder.uint16()
        guard try decoder.uint64() == requestID else { throw RustBackendError.invalidProtocol }
        let status = try decoder.int32()
        let length = Int(try decoder.uint32())
        guard length <= ipcMaximumFrame else { throw RustBackendError.invalidProtocol }
        let response = try readExactly(length)
        if status != 0 {
            throw RustBackendError.posix(status)
        }
        return response
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
        let bytes = Array(path.utf8CString)
        guard bytes.count <= MemoryLayout.size(ofValue: sockaddr_un().sun_path) else {
            throw RustBackendError.posix(ENAMETOOLONG)
        }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address) { destination in
            bytes.withUnsafeBytes { source in
                destination[
                    MemoryLayout<sa_family_t>.size..<(MemoryLayout<sa_family_t>.size + bytes.count)
                ].copyBytes(from: source)
            }
        }
        let length = socklen_t(MemoryLayout<sa_family_t>.size + bytes.count)
        let status = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(descriptor, $0, length)
            }
        }
        guard status == 0 else { throw RustBackendError.posix(errno) }
    }

    private func writeAll(_ data: Data) throws {
        try data.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                let count = Darwin.write(descriptor, raw.baseAddress! + offset, raw.count - offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { throw RustBackendError.posix(errno == 0 ? EPIPE : errno) }
                offset += count
            }
        }
    }

    private func writePair(_ header: Data, _ body: Data) throws {
        if body.isEmpty {
            try writeAll(header)
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
                let count = Darwin.writev(descriptor, &vectors, 2)
                if count < 0 && errno == EINTR {
                    try writeAll(header)
                    try writeAll(body)
                    return
                }
                guard count > 0 else {
                    throw RustBackendError.posix(errno == 0 ? EPIPE : errno)
                }
                if count < headerBytes.count {
                    try writeAll(header.subdata(in: count..<headerBytes.count))
                    try writeAll(body)
                } else {
                    let bodyOffset = count - headerBytes.count
                    if bodyOffset < bodyBytes.count {
                        try writeAll(body.subdata(in: bodyOffset..<bodyBytes.count))
                    }
                }
            }
        }
    }

    private func readExactly(_ length: Int) throws -> Data {
        if length == 0 { return Data() }
        var data = Data(count: length)
        try data.withUnsafeMutableBytes { raw in
            var offset = 0
            while offset < raw.count {
                let count = Darwin.read(descriptor, raw.baseAddress! + offset, raw.count - offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { throw RustBackendError.posix(errno == 0 ? ECONNRESET : errno) }
                offset += count
            }
        }
        return data
    }
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
