// Tests for the Rust-process bridge.
import Foundation
import FSKit
import Testing

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

    @Test("local network files read through a leased descriptor")
    func localFileLease() throws {
        let url = FileManager.default.temporaryDirectory
            .appending(path: "fsk-local-\(UUID().uuidString)")
        try Data("downloaded".utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }

        let item = FSKItem(inode: 99, kind: .file)
        try item.install(
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
}
