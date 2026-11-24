// Tests for the Rust-process bridge.
import Foundation
import FSKit
import Testing
import Darwin
import ObjectiveC

@Suite("Rust process bridge")
struct RustBackendTests {
    @Test("rejects resources which do not contain a private IPC capability")
    func rejectsInvalidResource() {
        #expect(throws: RustBackendError.invalidResource) {
            try RustBackend(resourceURL: URL(string: "https://example.com")!)
        }
    }

    @Test("packed directory names are sliced without string conversion")
    func packedDirectoryNames() {
        let names = Data("firstsecond".utf8)
        let entry = FSKDirEntry(
            inode: 2,
            next_cookie: 1,
            name_offset: 5,
            name_length: 6,
            kind: FSK_KIND_FILE
        )
        let batch = RustDirectoryBatch(
            entries: [entry],
            names: names,
            verifier: 1,
            nextCookie: 1,
            eof: true
        )
        #expect(String(decoding: batch.name(for: entry), as: UTF8.self) == "second")
    }

    @Test("Unix socket addresses preserve the Darwin family and path layout")
    func unixSocketAddressLayout() throws {
        let path = "/tmp/fsk.sock"
        var (address, length) = try unixSocketAddress(path: path)
        let pathOffset = MemoryLayout<sockaddr_un>.offset(of: \.sun_path)!

        #expect(address.sun_len == UInt8(length))
        #expect(address.sun_family == sa_family_t(AF_UNIX))
        #expect(Int(length) == pathOffset + path.utf8.count)

        let encodedPath = withUnsafeBytes(of: &address) {
            Array($0[pathOffset..<(pathOffset + path.utf8.count)])
        }
        #expect(encodedPath == Array(path.utf8))
        withUnsafeBytes(of: &address) {
            #expect($0[Int(length)] == 0)
        }
    }

    @Test("a dropped IPC connection is replaced without replaying the interrupted write")
    func reconnectsAfterTransportFailure() async throws {
        try await withIPCPeer([
            [.init(opcode: 1, body: Data(count: 4)), .init(opcode: 8, body: nil)],
            [.init(opcode: 14)],
        ]) { backend in
            #expect(throws: RustBackendError.self) {
                try backend.write(inode: 2, offset: 0, data: Data([1]))
            }
            try backend.synchronize()
        }
    }

    @Test("a server-reported POSIX error leaves the connection reusable")
    func reusesConnectionAfterServerError() async throws {
        try await withIPCPeer([
            [
                .init(opcode: 1, body: Data(count: 4)),
                .init(opcode: 5, status: EPIPE),
                .init(opcode: 14),
            ],
        ]) { backend in
            // The same errno can come from the socket itself. Only its
            // origin, not its value, determines whether reuse is safe.
            #expect(throws: RustBackendError.posix(EPIPE)) {
                try backend.lookup(parent: 1, name: Data("file".utf8))
            }
            try backend.synchronize()
        }
    }

    @Test("local network files read through a leased descriptor")
    func localFileLease() throws {
        let url = auditTemporaryDirectory()
            .appending(path: "fsk-local-\(UUID().uuidString)")
        try Data("downloaded".utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }

        let item = FSKItem(inode: 99, kind: .file)
        item.install(
            RustOpenFile(
                lease: 7,
                generation: 1,
                localPath: Data(url.path.utf8)
            )
        )
        var bytes = [UInt8](repeating: 0, count: 10)
        let count = try bytes.withUnsafeMutableBytes {
            try item.readLocal(offset: 0, length: $0.count, into: $0)
        }
        #expect(count == 10)
        #expect(String(decoding: bytes, as: UTF8.self) == "downloaded")
        #expect(item.releaseOpenState() == 7)
    }

    @Test("a second open reuses the first lease instead of failing")
    func duplicateOpenSharesLease() {
        let item = FSKItem(inode: 99, kind: .file)
        #expect(
            item.install(RustOpenFile(lease: 7, generation: 1, localPath: nil))
        )
        #expect(
            !item.install(RustOpenFile(lease: 8, generation: 1, localPath: nil))
        )
        #expect(item.releaseOpenState() == 7)
        #expect(item.releaseOpenState() == nil)
    }

    @Test("an unreadable local file falls back to backend reads")
    func unreadableLocalFileFallsBack() throws {
        let missing = auditTemporaryDirectory()
            .appending(path: "fsk-missing-\(UUID().uuidString)")
        let item = FSKItem(inode: 99, kind: .file)
        // The lease stays usable even though the cached path can't be opened.
        #expect(
            item.install(
                RustOpenFile(
                    lease: 9,
                    generation: 1,
                    localPath: Data(missing.path.utf8)
                )
            )
        )
        var bytes = [UInt8](repeating: 0, count: 4)
        let count = try bytes.withUnsafeMutableBytes {
            try item.readLocal(offset: 0, length: $0.count, into: $0)
        }
        #expect(count == nil)
        #expect(item.releaseOpenState() == 9)
    }

    @Test("an empty non-final directory page is not progress")
    func emptyDirectoryPageDoesNotLoop() {
        func batch(eof: Bool, entries: [FSKDirEntry]) -> RustDirectoryBatch {
            RustDirectoryBatch(
                entries: entries,
                names: Data(),
                verifier: 1,
                nextCookie: 0,
                eof: eof
            )
        }
        let entry = FSKDirEntry(
            inode: 2,
            next_cookie: 1,
            name_offset: 0,
            name_length: 0,
            kind: FSK_KIND_FILE
        )
        #expect(!batch(eof: false, entries: []).madeProgress)
        #expect(batch(eof: true, entries: []).madeProgress)
        #expect(batch(eof: false, entries: [entry]).madeProgress)
    }

    @Test("volume exposes the FSKit lifecycle protocol and selectors")
    func volumeOperationsSurface() throws {
        let operations = try #require(NSProtocolFromString("FSVolumeOperations"))
        #expect(class_conformsToProtocol(FSKVolume.self, operations))
        #expect(
            class_getInstanceMethod(
                FSKVolume.self,
                NSSelectorFromString("activateWithOptions:replyHandler:")
            ) != nil
        )
        #expect(
            class_getInstanceMethod(
                FSKVolume.self,
                NSSelectorFromString("deactivateWithOptions:replyHandler:")
            ) != nil
        )
    }

    @Test("the extension declares the IPC protocol version it implements")
    func protocolVersionMatchesInfoPlist() throws {
        // The Rust side checks the same declaration text; keep both in sync so
        // the app can never advertise a protocol its code does not speak.
        let info = URL(filePath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appending(path: "FSKExtension/Info.plist")
        guard FileManager.default.fileExists(atPath: info.path) else { return }

        let data = try Data(contentsOf: info)
        let object = try PropertyListSerialization.propertyList(from: data, format: nil)
        let plist = try #require(object as? [String: Any])
        #expect(plist["FSKProtocolMajor"] as? Int == Int(ipcProtocolMajor))
        #expect(plist["FSKProtocolMinor"] as? Int == Int(ipcProtocolMinor))
    }
}

private struct IPCReply: Sendable {
    let opcode: UInt16
    var status: Int32 = 0
    // nil closes the connection after receiving the complete request.
    var body: Data? = Data()
}

/// One script per accepted connection. Each starts with a HELLO exchange.
private func withIPCPeer(
    _ scripts: [[IPCReply]],
    client: @escaping @Sendable (RustBackend) throws -> Void
) async throws {
    let uniqueDirectory = "fsk-\(UUID().uuidString.prefix(8))"
    let directory = auditTemporaryDirectory().appending(path: uniqueDirectory)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: directory) }
    let path = directory.appending(path: "ipc").path
    var (address, length) = try unixSocketAddress(path: path)
    let listener = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
    try #require(listener >= 0)
    defer { Darwin.close(listener) }
    let bound = withUnsafePointer(to: &address) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.bind(listener, $0, length)
        }
    }
    try #require(bound == 0)
    try #require(Darwin.listen(listener, 2) == 0)

    var resource = URLComponents()
    resource.scheme = "fsk"
    resource.host = "mount"
    resource.queryItems = [
        URLQueryItem(name: "socket", value: path),
        URLQueryItem(name: "token", value: String(repeating: "00", count: 32)),
    ]
    let url = try #require(resource.url)
    try await withThrowingTaskGroup(of: Void.self) { group in
        group.addTask {
            for script in scripts {
                try serveIPCConnection(listener, script: [.init(opcode: 0)] + script)
            }
        }
        try client(RustBackend(resourceURL: url))
        try await group.waitForAll()
    }
}

private func serveIPCConnection(_ listener: Int32, script: [IPCReply]) throws {
    // Bound waits so a missing reconnect or missed frame fails instead of
    // hanging the test runner.
    var ready = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
    try #require(Darwin.poll(&ready, 1, 2_000) == 1)
    let peer = Darwin.accept(listener, nil, nil)
    try #require(peer >= 0)
    defer { Darwin.close(peer) }
    var timeout = timeval(tv_sec: 2, tv_usec: 0)
    let receiveTimeout = setsockopt(
        peer, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)
    )
    try #require(receiveTimeout == 0)
    var noSignal: Int32 = 1
    let suppressSignal = setsockopt(
        peer, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, socklen_t(MemoryLayout<Int32>.size)
    )
    try #require(suppressSignal == 0)

    for reply in script {
        let header = try readIPCBytes(24, from: peer)
        let opcode = header.withUnsafeBytes {
            UInt16(littleEndian: $0.loadUnaligned(fromByteOffset: 6, as: UInt16.self))
        }
        try #require(opcode == reply.opcode)
        let length = header.withUnsafeBytes {
            UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 16, as: UInt32.self))
        }
        try #require(length <= 4_096)
        _ = try readIPCBytes(Int(length), from: peer)
        guard let body = reply.body else { return }

        var response = Data()
        appendIPC(UInt32(0x4653_4249), to: &response)
        appendIPC(ipcProtocolMajor, to: &response)
        appendIPC(ipcProtocolMinor, to: &response)
        response.append(header[8..<16]) // request ID
        appendIPC(reply.status, to: &response)
        appendIPC(UInt32(body.count), to: &response)
        response.append(body)
        try response.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let sent = Darwin.write(peer, bytes.baseAddress! + offset, bytes.count - offset)
                if sent < 0 && errno == EINTR { continue }
                try #require(sent > 0)
                offset += sent
            }
        }
    }
}

private func readIPCBytes(_ count: Int, from peer: Int32) throws -> Data {
    var data = Data(count: count)
    try data.withUnsafeMutableBytes { bytes in
        var offset = 0
        while offset < bytes.count {
            let received = Darwin.read(peer, bytes.baseAddress! + offset, bytes.count - offset)
            if received < 0 && errno == EINTR { continue }
            try #require(received > 0)
            offset += received
        }
    }
    return data
}

private func appendIPC<T: FixedWidthInteger>(_ value: T, to bytes: inout Data) {
    var littleEndian = value.littleEndian
    withUnsafeBytes(of: &littleEndian) { bytes.append(contentsOf: $0) }
}
