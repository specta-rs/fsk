import Foundation
import FSKit
import Testing
import Darwin
import ObjectiveC
import XCTest

@Suite("FSK audit regressions", .serialized)
struct AuditRegressionTests {
    @Test("local reads honor offsets and partial end-of-file")
    func localReadOffsets() throws {
        let url = auditTemporaryDirectory().appending(path: "fsk-read-\(UUID().uuidString)")
        try Data("abcdef".utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        let item = FSKItem(inode: 2, kind: .file)
        #expect(item.install(RustOpenFile(lease: 7, generation: 1, localPath: Data(url.path.utf8))))
        defer { _ = item.releaseOpenState() }
        var bytes = [UInt8](repeating: 0, count: 4)
        let count = try bytes.withUnsafeMutableBytes { try item.readLocal(offset: 2, length: 4, into: $0) }
        #expect(count == 4)
        #expect(String(decoding: bytes, as: UTF8.self) == "cdef")
        bytes = [UInt8](repeating: 0, count: 4)
        let partial = try bytes.withUnsafeMutableBytes { try item.readLocal(offset: 5, length: 4, into: $0) }
        #expect(partial == 1)
        #expect(bytes[0] == 102)
        let eof = try bytes.withUnsafeMutableBytes { try item.readLocal(offset: 6, length: 4, into: $0) }
        #expect(eof == 0)
    }

    @Test("backend read and write preserve 64-bit offsets")
    func backendReadWriteOffsets() throws {
        let inode: UInt64 = 0x0102_0304_0506_0708
        let offset: UInt64 = 0x1112_1314_1516_1718
        var writeRequest = Data()
        auditAppend(inode, to: &writeRequest)
        auditAppend(offset, to: &writeRequest)
        writeRequest.append(Data([1, 2, 3]))
        var writeCount = Data()
        auditAppend(UInt64(3), to: &writeCount)
        var readRequest = Data()
        auditAppend(inode, to: &readRequest)
        auditAppend(offset, to: &readRequest)
        auditAppend(UInt32(4), to: &readRequest)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 8, behavior: .respond(body: writeCount), expectedRequestBody: writeRequest),
            .init(opcode: 7, behavior: .respond(body: Data([9, 8, 7, 6])), expectedRequestBody: readRequest),
        ]]) { backend in
            #expect(try backend.write(inode: inode, offset: offset, data: Data([1, 2, 3])) == 3)
            var bytes = [UInt8](repeating: 0, count: 4)
            let count = try bytes.withUnsafeMutableBytes { try backend.read(inode: inode, offset: offset, into: $0) }
            #expect(count == 4)
            #expect(bytes == [9, 8, 7, 6])
        }
    }

    @Test("root attributes use FSKit root identifiers")
    func rootIdentifiers() throws {
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            auditGetAttributes(auditAttributes(inode: 1, parent: 1, kind: FSK_KIND_DIRECTORY)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let request = FSItem.GetAttributesRequest()
            request.wantedAttributes = [.fileID, .parentID]
            var attributes: FSItem.Attributes?
            var error: Error?
            volume.getAttributes(request, of: volume.rootItem) { attributes = $0; error = $1 }
            #expect(error == nil)
            #expect(attributes?.fileID == .rootDirectory)
            #expect(attributes?.parentID == .parentOfRoot)
        }
    }

    @Test("child attributes identify root as rootDirectory")
    func childParentIdentifier() throws {
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            auditGetAttributes(auditAttributes(inode: 2, parent: 1)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            let request = FSItem.GetAttributesRequest()
            request.wantedAttributes = [.parentID]
            var attributes: FSItem.Attributes?
            var error: Error?
            volume.getAttributes(request, of: item) { attributes = $0; error = $1 }
            #expect(error == nil)
            #expect(attributes?.parentID == .rootDirectory)
        }
    }

    @Test("child inode identifiers avoid reserved FSKit values")
    func childIdentifiersAvoidReservedValues() throws {
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            auditGetAttributes(auditAttributes(inode: 2, parent: 1)),
            auditGetAttributes(auditAttributes(inode: 3, parent: 1)),
            auditGetAttributes(auditAttributes(inode: 0, parent: 1)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let request = FSItem.GetAttributesRequest()
            request.wantedAttributes = [.fileID]
            var first: FSItem.Attributes?
            var second: FSItem.Attributes?
            var zero: FSItem.Attributes?
            var error: Error?
            volume.getAttributes(request, of: FSKItem(inode: 2, kind: .file)) { first = $0; error = $1 }
            #expect(error == nil)
            volume.getAttributes(request, of: FSKItem(inode: 3, kind: .file)) { second = $0; error = $1 }
            #expect(error == nil)
            volume.getAttributes(request, of: FSKItem(inode: 0, kind: .file)) { zero = $0; error = $1 }
            #expect(error == nil)
            let firstID = try #require(first?.fileID)
            let secondID = try #require(second?.fileID)
            let zeroID = try #require(zero?.fileID)
            #expect(firstID != .rootDirectory)
            #expect(firstID != .parentOfRoot)
            #expect(firstID != secondID)
            #expect(zeroID == .invalid)
        }
    }

    @Test("failed setAttributes does not claim attributes were consumed")
    func failedSetAttributesConsumption() throws {
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 4, behavior: .respond(status: EPERM)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            let request = FSItem.SetAttributesRequest()
            request.mode = 0o600
            var error: Error?
            volume.setAttributes(request, on: item) { _, failure in error = failure }
            #expect(auditPOSIX(error) == EPERM)
            #expect(!request.wasAttributeConsumed(.mode))
        }
    }

    @Test("createItem reports the mode it applied")
    func createConsumesMode() throws {
        var inode = Data()
        auditAppend(UInt64(2), to: &inode)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 9, behavior: .respond(body: inode)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let request = FSItem.SetAttributesRequest()
            request.mode = 0o600
            var error: Error?
            volume.createItem(named: FSFileName(string: "file"), type: .file, inDirectory: volume.rootItem, attributes: request) { _, _, failure in error = failure }
            #expect(error == nil)
            #expect(request.wasAttributeConsumed(.mode))
        }
    }

    @Test("createSymbolicLink reports the mode it applied")
    func symlinkConsumesMode() throws {
        var inode = Data()
        auditAppend(UInt64(3), to: &inode)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 10, behavior: .respond(body: inode)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let request = FSItem.SetAttributesRequest()
            request.mode = 0o777
            var error: Error?
            volume.createSymbolicLink(named: FSFileName(string: "link"), inDirectory: volume.rootItem, attributes: request, linkContents: FSFileName(data: Data("target".utf8))) { _, _, failure in error = failure }
            #expect(error == nil)
            #expect(request.wasAttributeConsumed(.mode))
        }
    }

    @Test("changeTime is not fabricated from modifyTime")
    func changeTimeIsNotModifyTime() throws {
        let body = auditAttributes(inode: 2, parent: 1, modified: 2_000_000_000, accessed: 3_000_000_000)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(), auditGetAttributes(body),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            let request = FSItem.GetAttributesRequest()
            request.wantedAttributes = [.changeTime]
            var attributes: FSItem.Attributes?
            var error: Error?
            volume.getAttributes(request, of: item) { attributes = $0; error = $1 }
            #expect(error == nil)
            #expect(attributes != nil)
            #expect(!attributes!.isValid(.changeTime))
        }
    }

    @Test("overflowing attribute timestamps fail instead of wrapping")
    func timestampOverflow() throws {
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 4, behavior: .respond(body: auditAttributes(inode: 2, parent: 1))),
        ]], requireComplete: false) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            let request = FSItem.SetAttributesRequest()
            request.accessTime = timespec(tv_sec: Int.max, tv_nsec: 999_999_999)
            var error: Error?
            volume.setAttributes(request, on: item) { _, failure in error = failure }
            #expect(auditPOSIX(error) == EOVERFLOW)
            #expect(!request.wasAttributeConsumed(.accessTime))
        }
    }

    @Test("a failed lease close remains retryable")
    func failedCloseRetainsLease() throws {
        var opened = Data()
        auditAppend(UInt64(7), to: &opened)
        auditAppend(UInt8(0), to: &opened)
        auditAppend(UInt64(0), to: &opened)
        try withAuditBackend([[
            auditHello(), auditNetworkConfiguration(),
            .init(opcode: 15, behavior: .respond(body: opened)),
            .init(opcode: 16, behavior: .respond(status: EIO)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(error == nil)
            error = nil
            volume.closeItem(item, modes: []) { error = $0 }
            #expect(auditPOSIX(error) == EIO)
            #expect(item.releaseOpenState() == 7)
        }
    }

    @Test("deactivate reports a failed lease close")
    func deactivateCloseFailure() throws {
        var inode = Data()
        auditAppend(UInt64(2), to: &inode)
        var opened = Data()
        auditAppend(UInt64(7), to: &opened)
        auditAppend(UInt8(0), to: &opened)
        auditAppend(UInt64(0), to: &opened)
        try withAuditBackend([[
            auditHello(), auditNetworkConfiguration(),
            .init(opcode: 9, behavior: .respond(body: inode)),
            .init(opcode: 15, behavior: .respond(body: opened)),
            .init(opcode: 16, behavior: .respond(status: EIO)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            var created: FSKItem?
            volume.createItem(named: FSFileName(string: "file"), type: .file, inDirectory: volume.rootItem, attributes: FSItem.SetAttributesRequest()) { item, _, error in
                #expect(error == nil)
                created = item as? FSKItem
            }
            let item = try #require(created)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(error == nil)
            error = nil
            volume.deactivate { error = $0 }
            #expect(auditPOSIX(error) == EIO)
            #expect(item.releaseOpenState() == 7)
        }
    }

    @Test("a failed reclaim close retains the lease and cache entry")
    func failedReclaimCloseRetainsState() throws {
        var inode = Data()
        auditAppend(UInt64(2), to: &inode)
        var lookup = Data()
        auditAppend(UInt64(2), to: &lookup)
        auditAppend(FSK_KIND_FILE, to: &lookup)
        var opened = Data()
        auditAppend(UInt64(7), to: &opened)
        auditAppend(UInt8(0), to: &opened)
        auditAppend(UInt64(0), to: &opened)
        try withAuditBackend([[
            auditHello(), auditNetworkConfiguration(),
            .init(opcode: 9, behavior: .respond(body: inode)),
            .init(opcode: 15, behavior: .respond(body: opened)),
            .init(opcode: 16, behavior: .respond(status: EIO)),
            .init(opcode: 5, behavior: .respond(body: lookup)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            var created: FSKItem?
            volume.createItem(named: FSFileName(string: "file"), type: .file, inDirectory: volume.rootItem, attributes: FSItem.SetAttributesRequest()) { item, _, error in
                #expect(error == nil)
                created = item as? FSKItem
            }
            let item = try #require(created)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(error == nil)

            var reclaimMethod: Method?
            var originalReclaimIMP: IMP?
            if #available(macOS 27.0, *) {
                let method = try #require(class_getInstanceMethod(FSItem.self, NSSelectorFromString("tryReclaimWithBlock:")))
                reclaimMethod = method
                let accept: @convention(c) (AnyObject, Selector, @convention(block) () -> Void) -> Bool = { _, _, block in
                    block()
                    return true
                }
                originalReclaimIMP = method_setImplementation(method, unsafeBitCast(accept, to: IMP.self))
            }
            defer {
                if let reclaimMethod, let originalReclaimIMP {
                    method_setImplementation(reclaimMethod, originalReclaimIMP)
                }
            }

            volume.reclaimItem(item) { error = $0 }
            #expect(auditPOSIX(error) == EIO)
            #expect(item.releaseOpenState() == 7)

            var found: FSKItem?
            volume.lookupItem(named: FSFileName(string: "file"), inDirectory: volume.rootItem) { candidate, _, lookupError in
                #expect(lookupError == nil)
                found = candidate as? FSKItem
            }
            #expect(found === item)
        }
    }

    @Test("a non-final directory cookie must advance")
    func rewindingDirectoryCookie() throws {
        let body = auditDirectoryBody(entries: [(inode: 2, next: 0, name: "a")], next: 0, eof: false)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 6, behavior: .respond(body: body)),
        ]]) { backend in
            #expect(throws: RustBackendError.posix(ERANGE)) {
                _ = try backend.readDirectory(inode: 1, cookie: 0, verifier: 0)
            }
        }
    }

    @Test("directory cookies leave room for the FSKit bias")
    func overflowingDirectoryCookie() throws {
        let body = auditDirectoryBody(entries: [(inode: 2, next: UInt64.max, name: "a")], next: UInt64.max, eof: true)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 6, behavior: .respond(body: body)),
        ]]) { backend in
            #expect(throws: RustBackendError.posix(EOVERFLOW)) {
                _ = try backend.readDirectory(inode: 1, cookie: 0, verifier: 0)
            }
        }
    }

    @Test("probe rejects an FSK URL without a private capability")
    func probeRejectsIncompleteResource() throws {
        let fileSystem = FSKFileSystem()
        let resource = FSGenericURLResource(url: URL(string: "fsk://mount")!)
        var result: FSProbeResult?
        var error: Error?
        fileSystem.probeResource(resource: resource) { result = $0; error = $1 }
        #expect(result == nil)
        #expect(error != nil)
    }

    @Test("probe rejects an FSK URL whose socket path contains NUL")
    func probeRejectsNULSocketPath() throws {
        let resource = try auditResource(socketPath: "/tmp/a\0b")
        let fileSystem = FSKFileSystem()
        var result: FSProbeResult?
        var error: Error?
        fileSystem.probeResource(resource: resource) { result = $0; error = $1 }
        #expect(result == nil)
        #expect(error != nil)
    }

    @Test("probe rejects an FSK URL whose socket path is too long")
    func probeRejectsOverlongSocketPath() throws {
        let resource = try auditResource(socketPath: "/" + String(repeating: "a", count: 200))
        let fileSystem = FSKFileSystem()
        var result: FSProbeResult?
        var error: Error?
        fileSystem.probeResource(resource: resource) { result = $0; error = $1 }
        #expect(result == nil)
        #expect(error != nil)
    }

    @Test("Unix socket paths reject embedded NUL bytes")
    func socketPathRejectsNUL() {
        #expect(throws: RustBackendError.posix(EINVAL)) {
            _ = try unixSocketAddress(path: "audit/tmp/a\0b")
        }
    }

    @Test("duplicate-open lease cleanup errors reach FSKit")
    func duplicateOpenCloseFailure() throws {
        func openBody(_ lease: UInt64) -> Data {
            var body = Data()
            auditAppend(lease, to: &body)
            auditAppend(UInt8(0), to: &body)
            auditAppend(UInt64(0), to: &body)
            return body
        }
        try withAuditBackend([[
            auditHello(), auditNetworkConfiguration(),
            .init(opcode: 15, behavior: .respond(body: openBody(7))),
            .init(opcode: 15, behavior: .respond(body: openBody(8))),
            .init(opcode: 16, behavior: .respond(status: EIO)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(error == nil)
            error = nil
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(auditPOSIX(error) == EIO)
        }
    }

    @Test("concurrent requests use independent authenticated connections")
    func concurrentIPCRequests() throws {
        let reply = AuditIPCScript(opcode: 2, behavior: .respond(body: Data(count: 52)))
        var scripts = [[auditHello(), auditLocalConfiguration(), reply]]
        for _ in 0..<3 { scripts.append([auditHello(), reply]) }
        let server = try AuditIPCServer(scripts: scripts)
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: auditIPCRequestTimeout
        )
        let group = DispatchGroup()
        let errors = AuditErrorList()
        for _ in 0..<4 {
            group.enter()
            DispatchQueue.global(qos: .userInitiated).async {
                defer { group.leave() }
                do {
                    _ = try backend.statfs()
                } catch {
                    errors.append(error)
                }
            }
        }
        #expect(group.wait(timeout: .now() + 3) == .success)
        #expect(errors.values.isEmpty)
        try server.wait()
    }

    @Test("a stalled IPC peer cannot block a request forever")
    func stalledIPCRequest() throws {
        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 2, behavior: .hang),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: auditIPCRequestTimeout
        )
        let finished = DispatchSemaphore(value: 0)
        let box = AuditResultBox()
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                _ = try backend.statfs()
                box.set(nil)
            } catch {
                box.set(error)
            }
            finished.signal()
        }
        let result = finished.wait(timeout: .now() + 1)
        #expect(result == .success)
        server.stop()
        try server.wait()
        if result == .timedOut {
            let completion = finished.wait(timeout: .now() + 2)
            #expect(completion == .success)
        }
        #expect(box.value != nil)
    }

    @Test("an immediately closed IPC peer fails without waiting for the deadline")
    func immediatelyClosedIPCRequest() throws {
        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 2, behavior: .close),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: auditIPCRequestTimeout
        )
        let start = DispatchTime.now()
        #expect(throws: RustBackendError.self) {
            _ = try backend.statfs()
        }
        let elapsed = DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds
        #expect(elapsed < UInt64(auditIPCRequestTimeout * 800_000_000))
        try server.wait()
    }

    @Test("a trickling IPC peer cannot exceed the request deadline")
    func tricklingIPCRequest() throws {
        let server = try AuditIPCServer(scripts: [[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 2, behavior: .trickle(body: Data(count: 52))),
        ]])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: auditIPCRequestTimeout
        )
        let finished = DispatchSemaphore(value: 0)
        let box = AuditResultBox()
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                _ = try backend.statfs()
                box.set(nil)
            } catch {
                box.set(error)
            }
            finished.signal()
        }
        let result = finished.wait(timeout: .now() + 1)
        #expect(result == .success)
        server.stop()
        try server.wait()
        if result == .timedOut {
            let completion = finished.wait(timeout: .now() + 2)
            #expect(completion == .success)
        }
        #expect(box.value != nil)
    }

    @Test("reclaimItem calls tryReclaim and honors a false result")
    func reclaimHonorsTryReclaim() throws {
        guard #available(macOS 27.0, *) else { return }
        var lookup = Data()
        auditAppend(UInt64(2), to: &lookup)
        auditAppend(FSK_KIND_FILE, to: &lookup)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 5, behavior: .respond(body: lookup)),
            .init(opcode: 5, behavior: .respond(body: lookup)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            var first: FSKItem?
            volume.lookupItem(named: FSFileName(string: "file"), inDirectory: volume.rootItem) { item, _, error in
                #expect(error == nil)
                first = item as? FSKItem
            }
            let item = try #require(first)

            let method = try #require(class_getInstanceMethod(FSItem.self, NSSelectorFromString("tryReclaimWithBlock:")))
            let reject: @convention(c) (AnyObject, Selector, AnyObject) -> Bool = { _, _, _ in
                AuditReclaimProbe.calls += 1
                return false
            }
            let original = method_setImplementation(method, unsafeBitCast(reject, to: IMP.self))
            defer { method_setImplementation(method, original) }
            AuditReclaimProbe.calls = 0

            var reclaimError: Error?
            volume.reclaimItem(item) { reclaimError = $0 }
            #expect(reclaimError == nil)
            #expect(AuditReclaimProbe.calls == 1)

            var second: FSKItem?
            volume.lookupItem(named: FSFileName(string: "file"), inDirectory: volume.rootItem) { found, _, error in
                #expect(error == nil)
                second = found as? FSKItem
            }
            #expect(second === item)
        }
    }
}

private final class AuditErrorList: @unchecked Sendable {
    private let lock = NSLock()
    private var errors: [Error] = []

    var values: [Error] {
        lock.lock(); defer { lock.unlock() }
        return errors
    }

    func append(_ error: Error) {
        lock.lock(); errors.append(error); lock.unlock()
    }
}

private final class AuditResultBox: @unchecked Sendable {
    private let lock = NSLock()
    private var error: Error?

    var value: Error? {
        lock.lock(); defer { lock.unlock() }
        return error
    }

    func set(_ error: Error?) {
        lock.lock(); self.error = error; lock.unlock()
    }
}

private enum AuditReclaimProbe {
    nonisolated(unsafe) static var calls = 0
}

private func auditPOSIX(_ error: Error?) -> Int32? {
    (error as? POSIXError)?.code.rawValue
}

private func auditResource(socketPath: String) throws -> FSGenericURLResource {
    var components = URLComponents()
    components.scheme = "fsk"
    components.host = "audit"
    components.queryItems = [
        URLQueryItem(name: "socket", value: socketPath),
        URLQueryItem(name: "token", value: String(repeating: "00", count: 32)),
    ]
    return FSGenericURLResource(url: try #require(components.url))
}

private func auditDirectoryBody(entries: [(inode: UInt64, next: UInt64, name: String)], next: UInt64, eof: Bool) -> Data {
    var names = Data()
    var records = Data()
    for entry in entries {
        let name = Data(entry.name.utf8)
        auditAppend(entry.inode, to: &records)
        auditAppend(entry.next, to: &records)
        auditAppend(UInt32(names.count), to: &records)
        auditAppend(UInt16(name.count), to: &records)
        auditAppend(FSK_KIND_FILE, to: &records)
        names.append(name)
    }
    var body = Data()
    auditAppend(UInt64(1), to: &body)
    auditAppend(next, to: &body)
    auditAppend(eof ? UInt8(1) : UInt8(0), to: &body)
    auditAppend(UInt32(entries.count), to: &body)
    auditAppend(UInt32(names.count), to: &body)
    body.append(records)
    body.append(names)
    return body
}
