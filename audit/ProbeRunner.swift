import Foundation
import Darwin

@main
struct AuditProbeRunner {
    static func main() {
        guard CommandLine.arguments.count == 2 else {
            fail("usage: fsk-probes readdir-name-bounds|write-count-overflow|readlink-capacity")
        }
        do {
            switch CommandLine.arguments[1] {
            case "readdir-entry-count":
                try probeDirectoryBody(entryCount: UInt32.max, records: Data(), names: Data())
            case "readdir-name-bounds":
                var records = Data()
                auditAppend(UInt64(2), to: &records)
                auditAppend(UInt64(1), to: &records)
                auditAppend(UInt32.max, to: &records)
                auditAppend(UInt16(1), to: &records)
                auditAppend(FSK_KIND_FILE, to: &records)
                try probeDirectoryBody(entryCount: 1, records: records, names: Data("a".utf8))
            case "write-count-overflow":
                var count = Data()
                auditAppend(UInt64.max, to: &count)
                try probeWriteCount(count)
            case "readlink-capacity":
                try probeReadlinkCapacity()
            default:
                fail("unknown probe")
            }
        } catch {
            fail("unexpected rejection: \(error)")
        }
    }

    private static func probeDirectoryBody(entryCount: UInt32, records: Data, names: Data) throws {
        var body = Data()
        auditAppend(UInt64(1), to: &body)
        auditAppend(UInt64(1), to: &body)
        auditAppend(UInt8(0), to: &body)
        auditAppend(entryCount, to: &body)
        auditAppend(UInt32(names.count), to: &body)
        body.append(records)
        body.append(names)

        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 6, behavior: .respond(body: body)),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(resourceURL: server.resourceURL)
        do {
            let batch = try backend.readDirectory(inode: 1, cookie: 0, verifier: 0)
            for entry in batch.entries { _ = batch.name(for: entry) }
            try server.wait()
            fail("malformed directory response was accepted")
        } catch let error as RustBackendError {
            try server.wait()
            print("rejected: \(error)")
            exit(0)
        }
    }

    private static func probeWriteCount(_ body: Data) throws {
        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 8, behavior: .respond(body: body)),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(resourceURL: server.resourceURL)
        do {
            _ = try backend.write(inode: 2, offset: 0, data: Data([1]))
            try server.wait()
            fail("invalid write count was accepted")
        } catch let error as RustBackendError {
            try server.wait()
            print("rejected: \(error)")
            exit(0)
        }
    }

    private static func probeReadlinkCapacity() throws {
        let body = Data(repeating: 0x61, count: 4_097)
        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 11, behavior: .respond(body: body)),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(resourceURL: server.resourceURL)
        do {
            _ = try backend.readlink(inode: 2, capacity: 4_096)
            try server.wait()
            fail("oversized readlink response was accepted")
        } catch let error as RustBackendError {
            try server.wait()
            print("rejected: \(error)")
            exit(0)
        }
    }

    private static func fail(_ message: String) -> Never {
        FileHandle.standardError.write(Data("\(message)\n".utf8))
        exit(2)
    }
}
