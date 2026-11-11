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

    @Test("local network files read through a leased descriptor")
    func localFileLease() throws {
        let url = FileManager.default.temporaryDirectory
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
        let missing = FileManager.default.temporaryDirectory
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
