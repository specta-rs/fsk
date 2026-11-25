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

    @Test("maximum non-root inode identifiers report overflow")
    func maximumInodeIdentifierOverflows() throws {
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            auditGetAttributes(auditAttributes(inode: UInt64.max, parent: 1)),
            auditGetAttributes(auditAttributes(inode: 2, parent: UInt64.max)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            var attributes: FSItem.Attributes?
            var error: Error?
            let fileRequest = FSItem.GetAttributesRequest()
            fileRequest.wantedAttributes = [.fileID]
            volume.getAttributes(fileRequest, of: FSKItem(inode: UInt64.max, kind: .file)) { attributes = $0; error = $1 }
            #expect(attributes == nil)
            #expect(auditPOSIX(error) == EOVERFLOW)

            let parentRequest = FSItem.GetAttributesRequest()
            parentRequest.wantedAttributes = [.parentID]
            volume.getAttributes(parentRequest, of: FSKItem(inode: 2, kind: .file)) { attributes = $0; error = $1 }
            #expect(attributes == nil)
            #expect(auditPOSIX(error) == EOVERFLOW)
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

    @Test("an in-flight close keeps reclaim and deactivation retryable")
    func inFlightCloseRetainsState() throws {
        func startedLease(_ result: FSKItem.CloseBeginResult) -> UInt64? {
            if case .started(let lease) = result { return lease }
            return nil
        }

        var inode = Data()
        auditAppend(UInt64(2), to: &inode)
        var lookup = Data()
        auditAppend(UInt64(2), to: &lookup)
        auditAppend(FSK_KIND_FILE, to: &lookup)
        try withAuditBackend([[
            auditHello(), auditLocalConfiguration(),
            .init(opcode: 9, behavior: .respond(body: inode)),
            .init(opcode: 5, behavior: .respond(body: lookup)),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            var created: FSKItem?
            volume.createItem(named: FSFileName(string: "file"), type: .file, inDirectory: volume.rootItem, attributes: FSItem.SetAttributesRequest()) { item, _, error in
                #expect(error == nil)
                created = item as? FSKItem
            }
            let item = try #require(created)
            #expect(item.install(RustOpenFile(lease: 7, generation: 1, localPath: nil)))
            let firstLease = try #require(startedLease(item.beginClose()))
            #expect(firstLease == 7)

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

            var error: Error?
            volume.reclaimItem(item) { error = $0 }
            #expect(auditPOSIX(error) == EBUSY)
            item.finishClose(lease: 7, succeeded: false)

            let retryLease = try #require(startedLease(item.beginClose()))
            #expect(retryLease == 7)
            volume.deactivate { error = $0 }
            #expect(auditPOSIX(error) == EBUSY)
            item.finishClose(lease: retryLease, succeeded: false)

            let finalLease = try #require(startedLease(item.beginClose()))
            #expect(finalLease == 7)
            item.finishClose(lease: finalLease, succeeded: false)

            var found: FSKItem?
            volume.lookupItem(named: FSFileName(string: "file"), inDirectory: volume.rootItem) { candidate, _, lookupError in
                #expect(lookupError == nil)
                found = candidate as? FSKItem
            }
            #expect(found === item)
            #expect(item.releaseOpenState() == 7)
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
        ], [
            auditHello(), .init(opcode: 14),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(error == nil)
            error = nil
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(auditPOSIX(error) == EIO)
            try backend.synchronize()
        }
    }

    @Test("successful open acknowledges the installed lease exactly once")
    func openAcknowledgesInstalledLease() throws {
        let opened = auditOpenBody(lease: 7)
        let acknowledgement = auditLeaseAcknowledgement(lease: 7)
        try withAuditBackend([[
            auditHello(minor: 2), auditNetworkConfiguration(),
            .init(opcode: 15, behavior: .respond(body: opened)),
            .init(opcode: 17, behavior: .respond(body: Data()), expectedRequestBody: acknowledgement),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(error == nil)
            #expect(item.releaseOpenState() == 7)
        }
    }

    @Test("open retains its connection through lease acknowledgement")
    func openAcknowledgesOnCreatingConnection() throws {
        let opened = auditOpenBody(lease: 7)
        let acknowledgement = auditLeaseAcknowledgement(lease: 7)
        let statfs = AuditIPCScript(opcode: 2, behavior: .respond(body: Data(count: 52)))
        let server = try AuditIPCServer(scripts: [
            [
                auditHello(minor: 2), auditNetworkConfiguration(),
                .init(opcode: 15, behavior: .respond(body: opened)),
                .init(opcode: 17, behavior: .respond(body: Data()), expectedRequestBody: acknowledgement),
            ],
            [auditHello(minor: 2), statfs],
        ])
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(resourceURL: server.resourceURL, requestTimeout: auditIPCRequestTimeout)
        let handle = try backend.open(inode: 2, writable: false)
        #expect(handle.file.lease == 7)
        _ = try backend.statfs()
        try handle.acknowledgeLease()
        handle.finish()
        try server.wait()
    }

    @Test("legacy open peers receive no lease acknowledgement")
    func legacyOpenDoesNotAcknowledge() throws {
        for minor: UInt16 in [0, 1] {
            try withAuditBackend([[
                auditHello(minor: minor), auditNetworkConfiguration(),
                .init(opcode: 15, behavior: .respond(body: auditOpenBody(lease: 7))),
            ]]) { backend in
                let volume = FSKVolume(backend: backend)
                let item = FSKItem(inode: 2, kind: .file)
                var error: Error?
                volume.openItem(item, modes: [.read]) { error = $0 }
                #expect(error == nil)
                #expect(item.releaseOpenState() == 7)
            }
        }
    }

    @Test("failed lease acknowledgement rolls back install and discards the connection")
    func failedAcknowledgementDiscardsConnection() throws {
        let opened = auditOpenBody(lease: 7)
        let acknowledgement = auditLeaseAcknowledgement(lease: 7)
        try withAuditBackend([[
            auditHello(minor: 2), auditNetworkConfiguration(),
            .init(opcode: 15, behavior: .respond(body: opened)),
            .init(opcode: 17, behavior: .respond(status: EIO), expectedRequestBody: acknowledgement),
            .init(opcode: 16, behavior: .respond(body: Data()), expectedRequestBody: acknowledgement),
        ], [
            auditHello(minor: 2), .init(opcode: 14),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            #expect(auditPOSIX(error) == EIO)
            #expect(item.releaseOpenState() == nil)
            try backend.synchronize()
        }
    }

    @Test("acknowledgement timeout discards the pending lease connection")
    func acknowledgementTimeoutDiscardsConnection() throws {
        let acknowledgement = auditLeaseAcknowledgement(lease: 7)
        try withAuditBackend([[
            auditHello(minor: 2), auditNetworkConfiguration(),
            .init(opcode: 15, behavior: .respond(body: auditOpenBody(lease: 7))),
            .init(opcode: 17, behavior: .hang),
        ], [
            auditHello(minor: 2),
            .init(opcode: 16, behavior: .respond(body: Data()), expectedRequestBody: acknowledgement),
            .init(opcode: 14),
        ]], requireComplete: false) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            let start = DispatchTime.now()
            volume.openItem(item, modes: [.read]) { error = $0 }
            let elapsed = DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds
            #expect(auditPOSIX(error) == ETIMEDOUT)
            #expect(elapsed < 450_000_000)
            #expect(item.releaseOpenState() == nil)
            try backend.synchronize()
        }
    }

    @Test("ambiguous acknowledgement closes the lease on a healthy connection")
    func ambiguousAcknowledgementUsesHealthyConnection() throws {
        let acknowledgement = auditLeaseAcknowledgement(lease: 7)
        try withAuditBackend([[
            auditHello(minor: 2), auditNetworkConfiguration(),
            .init(opcode: 15, behavior: .respond(body: auditOpenBody(lease: 7))),
            .init(opcode: 17, behavior: .applyThenClose, expectedRequestBody: acknowledgement),
        ], [
            auditHello(minor: 2),
            .init(opcode: 16, behavior: .respond(body: Data()), expectedRequestBody: acknowledgement),
            .init(opcode: 14),
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            let start = DispatchTime.now()
            volume.openItem(item, modes: [.read]) { error = $0 }
            let elapsed = DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds
            if let code = auditPOSIX(error) {
                #expect([EINVAL, ECONNRESET, EPIPE].contains(code))
            } else {
                Issue.record("expected an ACK transport error")
            }
            #expect(elapsed < 1_000_000_000)
            #expect(item.releaseOpenState() == nil)
            try backend.synchronize()
        }
    }

    @Test("unconfirmed ambiguous cleanup remains retryable")
    func unconfirmedAmbiguousCleanupIsRetried() throws {
        let acknowledgement = auditLeaseAcknowledgement(lease: 7)
        let statfs = AuditIPCScript(opcode: 2, behavior: .respond(body: Data(count: 52)))
        try withAuditBackend([[
            auditHello(minor: 2), auditNetworkConfiguration(),
            .init(opcode: 15, behavior: .respond(body: auditOpenBody(lease: 7))),
            .init(opcode: 17, behavior: .applyThenClose, expectedRequestBody: acknowledgement),
        ], [
            auditHello(minor: 2),
            .init(opcode: 16, behavior: .respond(status: EIO), expectedRequestBody: acknowledgement),
            .init(opcode: 16, behavior: .respond(body: Data()), expectedRequestBody: acknowledgement),
            statfs,
        ]]) { backend in
            let volume = FSKVolume(backend: backend)
            let item = FSKItem(inode: 2, kind: .file)
            var error: Error?
            volume.openItem(item, modes: [.read]) { error = $0 }
            if let code = auditPOSIX(error) {
                #expect([EINVAL, ECONNRESET, EPIPE].contains(code))
            } else {
                Issue.record("expected an ACK transport error")
            }
            #expect(item.releaseOpenState() == nil)
            _ = try backend.statfs()
        }
    }

    @Test("lease acknowledgement from another connection is rejected")
    func wrongConnectionAcknowledgementIsRejected() throws {
        let lease: UInt64 = 91
        let server = try AuditIPCServer(scripts: [
            [auditHello(minor: 2), .init(opcode: 15, behavior: .respond(body: auditOpenBody(lease: lease)))],
            [
                auditHello(minor: 2),
                .init(opcode: 17, behavior: .respond(status: EINVAL), expectedRequestBody: auditLeaseAcknowledgement(lease: lease)),
            ],
        ])
        server.start()
        defer { server.stop() }

        let opener = try auditConnect(to: server)
        defer { Darwin.close(opener) }
        try auditSendRequest(opener, opcode: 0, minor: 2, body: Data(repeating: 0, count: 32))
        _ = try auditReadResponse(opener)
        var openRequest = Data()
        auditAppend(UInt64(2), to: &openRequest)
        auditAppend(UInt8(0), to: &openRequest)
        try auditSendRequest(opener, opcode: 15, minor: 2, body: openRequest)
        let opened = try auditReadResponse(opener)
        #expect(opened.status == 0)

        let other = try auditConnect(to: server)
        defer { Darwin.close(other) }
        try auditSendRequest(other, opcode: 0, minor: 2, body: Data(repeating: 0, count: 32))
        _ = try auditReadResponse(other)
        try auditSendRequest(other, opcode: 17, minor: 2, body: auditLeaseAcknowledgement(lease: lease))
        let rejected = try auditReadResponse(other)
        #expect(rejected.status == EINVAL)
        try server.wait()
    }

    @Test("concurrent requests use independent authenticated connections")
    func concurrentIPCRequests() throws {
        let barrier = AuditIPCBarrier(expecting: 4)
        let server = try AuditIPCServer(barrier: barrier)
        server.start()
        defer { server.stop() }
        let backend = try RustBackend(
            resourceURL: server.resourceURL,
            requestTimeout: 5
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
        #expect(group.wait(timeout: .now() + 10) == .success)
        #expect(errors.values.isEmpty)
        try server.wait(timeout: 10)
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
            requestTimeout: 2
        )
        let start = DispatchTime.now()
        #expect(throws: RustBackendError.self) {
            _ = try backend.statfs()
        }
        let elapsed = DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds
        #expect(elapsed < 1_500_000_000)
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

private func auditOpenBody(lease: UInt64) -> Data {
    var body = Data()
    auditAppend(lease, to: &body)
    auditAppend(UInt8(0), to: &body)
    auditAppend(UInt64(0), to: &body)
    return body
}

private func auditLeaseAcknowledgement(lease: UInt64) -> Data {
    var body = Data()
    auditAppend(lease, to: &body)
    return body
}

private func auditConnect(to server: AuditIPCServer) throws -> Int32 {
    var (address, length) = try unixSocketAddress(path: server.socketPathForTesting)
    let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
    try #require(descriptor >= 0)
    let status = withUnsafePointer(to: &address) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            Darwin.connect(descriptor, $0, length)
        }
    }
    guard status == 0 else {
        let error = errno
        Darwin.close(descriptor)
        throw POSIXError(POSIXErrorCode(rawValue: error) ?? .EIO)
    }
    return descriptor
}

private func auditSendRequest(
    _ descriptor: Int32,
    opcode: UInt16,
    minor: UInt16,
    body: Data,
    requestID: UInt64 = 1
) throws {
    var frame = Data()
    auditAppend(UInt32(0x4653_4249), to: &frame)
    auditAppend(ipcProtocolMajor, to: &frame)
    auditAppend(opcode, to: &frame)
    auditAppend(requestID, to: &frame)
    auditAppend(UInt32(body.count), to: &frame)
    auditAppend(minor, to: &frame)
    auditAppend(UInt16(0), to: &frame)
    frame.append(body)
    try frame.withUnsafeBytes { bytes in
        var offset = 0
        while offset < bytes.count {
            let sent = Darwin.write(descriptor, bytes.baseAddress! + offset, bytes.count - offset)
            if sent < 0 && errno == EINTR { continue }
            try #require(sent > 0)
            offset += sent
        }
    }
}

private func auditReadResponse(_ descriptor: Int32) throws -> (status: Int32, body: Data) {
    let header = try auditReadExactBytes(24, from: descriptor)
    let magic = header.withUnsafeBytes { UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 0, as: UInt32.self)) }
    let major = header.withUnsafeBytes { UInt16(littleEndian: $0.loadUnaligned(fromByteOffset: 4, as: UInt16.self)) }
    try #require(magic == 0x4653_4249)
    try #require(major == ipcProtocolMajor)
    let status = header.withUnsafeBytes { Int32(littleEndian: $0.loadUnaligned(fromByteOffset: 16, as: Int32.self)) }
    let length = header.withUnsafeBytes { UInt32(littleEndian: $0.loadUnaligned(fromByteOffset: 20, as: UInt32.self)) }
    try #require(length <= 16 * 1024 * 1024)
    return (status, try auditReadExactBytes(Int(length), from: descriptor))
}

private func auditReadExactBytes(_ count: Int, from descriptor: Int32) throws -> Data {
    var data = Data(count: count)
    try data.withUnsafeMutableBytes { bytes in
        var offset = 0
        while offset < bytes.count {
            let received = Darwin.read(descriptor, bytes.baseAddress! + offset, bytes.count - offset)
            if received < 0 && errno == EINTR { continue }
            try #require(received > 0)
            offset += received
        }
    }
    return data
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
