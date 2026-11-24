import Foundation
import Darwin

@main
struct AuditProbeRunner {
    static func main() {
        guard CommandLine.arguments.count == 2 else {
            fail("usage: fsk-probes readdir-entry-count|readdir-name-bounds|write-count-overflow|readlink-capacity")
        }
        do {
            let result: RustBackendError
            switch CommandLine.arguments[1] {
            case "readdir-entry-count":
                result = try probeDirectoryBody(entryCount: UInt32.max, records: Data(), names: Data())
            case "readdir-name-bounds":
                var records = Data()
                auditAppend(UInt64(2), to: &records)
                auditAppend(UInt64(1), to: &records)
                auditAppend(UInt32.max, to: &records)
                auditAppend(UInt16(1), to: &records)
                auditAppend(FSK_KIND_FILE, to: &records)
                result = try probeDirectoryBody(entryCount: 1, records: records, names: Data("a".utf8))
            case "write-count-overflow":
                var count = Data()
                auditAppend(UInt64.max, to: &count)
                result = try probeWriteCount(count)
            case "readlink-capacity":
                result = try probeReadlinkCapacity()
            default:
                fail("unknown probe")
            }
            print("rejected: \(result)")
        } catch {
            fail("probe failed: \(error)")
        }
    }

    private static func probeDirectoryBody(
        entryCount: UInt32,
        records: Data,
        names: Data
    ) throws -> RustBackendError {
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
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: auditIPCRequestTimeout
        )
        do {
            let batch = try backend.readDirectory(inode: 1, cookie: 0, verifier: 0)
            for entry in batch.entries { _ = batch.name(for: entry) }
            throw AuditIPCPeerError(description: "malformed directory response was accepted")
        } catch let error as RustBackendError {
            try server.wait()
            return error
        }
    }

    private static func probeWriteCount(_ body: Data) throws -> RustBackendError {
        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 8, behavior: .respond(body: body)),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: auditIPCRequestTimeout
        )
        do {
            _ = try backend.write(inode: 2, offset: 0, data: Data([1]))
            throw AuditIPCPeerError(description: "invalid write count was accepted")
        } catch let error as RustBackendError {
            try server.wait()
            return error
        }
    }

    private static func probeReadlinkCapacity() throws -> RustBackendError {
        let body = Data(repeating: 0x61, count: 4_097)
        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 11, behavior: .respond(body: body)),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: auditIPCRequestTimeout
        )
        do {
            _ = try backend.readlink(inode: 2, capacity: 4_096)
            throw AuditIPCPeerError(description: "oversized readlink response was accepted")
        } catch let error as RustBackendError {
            try server.wait()
            return error
        }
    }

    private static func fail(_ message: String) -> Never {
        FileHandle.standardError.write(Data("\(message)\n".utf8))
        exit(2)
    }
}
