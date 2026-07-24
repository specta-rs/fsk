import Foundation
import Testing

@Suite("Rust backend C ABI")
struct RustBackendTests {
    @Test("factory exposes the example file")
    func exampleFile() throws {
        let backend = RustBackend()
        let result = try backend.lookup(parent: 1, name: Data("hello.txt".utf8))
        let attributes = try backend.attributes(inode: result.inode)
        var bytes = [UInt8](repeating: 0, count: Int(attributes.size))
        let count = try bytes.withUnsafeMutableBytes {
            try backend.read(inode: result.inode, offset: 0, into: $0)
        }
        #expect(String(decoding: bytes.prefix(count), as: UTF8.self).contains("Hello from Rust"))
    }

    @Test("create, sparse write, read, and remove round trip")
    func mutableRoundTrip() throws {
        let backend = RustBackend()
        let name = Data("round-trip".utf8)
        let inode = try backend.create(
            parent: 1,
            name: name,
            kind: UInt8(FSB_KIND_FILE),
            mode: 0o600
        )
        #expect(try backend.write(inode: inode, offset: 3, data: Data("ok".utf8)) == 2)
        var bytes = [UInt8](repeating: 0xff, count: 5)
        let count = try bytes.withUnsafeMutableBytes {
            try backend.read(inode: inode, offset: 0, into: $0)
        }
        #expect(count == 5)
        #expect(bytes == [0, 0, 0, 111, 107])
        try backend.remove(parent: 1, name: name, inode: inode)
        #expect(throws: RustBackendError.posix(2)) {
            try backend.lookup(parent: 1, name: name)
        }
    }

    @Test("directory batch packs names into one arena")
    func packedDirectoryBatch() throws {
        let backend = RustBackend()
        for index in 0..<20 {
            _ = try backend.create(
                parent: 1,
                name: Data("entry-\(index)".utf8),
                kind: UInt8(FSB_KIND_FILE),
                mode: 0o644
            )
        }
        let first = try backend.readDirectory(
            inode: 1,
            cookie: 0,
            verifier: 0,
            entryCapacity: 8,
            nameCapacity: 1024
        )
        #expect(first.entries.count == 8)
        #expect(!first.eof)
        let names = first.entries.map { String(decoding: first.name(for: $0), as: UTF8.self) }
        #expect(names.first == "hello.txt")

        let second = try backend.readDirectory(
            inode: 1,
            cookie: first.nextCookie,
            verifier: first.verifier,
            entryCapacity: 32,
            nameCapacity: 4096
        )
        #expect(second.eof)
        #expect(second.entries.count == 13)
    }
}

