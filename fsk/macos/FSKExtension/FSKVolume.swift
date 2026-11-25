import Foundation
// FSKit volume operations.
import FSKit
import ObjectiveC
import os

private let rootInode: UInt64 = 1
private let directoryCookieBias: UInt64 = fskDirectoryCookieBias
private typealias FSKTryReclaimIMP = @convention(c) (
    FSItem,
    Selector,
    @convention(block) () -> Void
) -> Bool

private final class FSKItemCache: @unchecked Sendable {
    var items: [UInt64: FSKItem] = [:]
}

final class FSKVolume: FSVolume,
    FSVolume.Operations,
    FSVolume.ReadWriteOperations,
    FSVolume.OpenCloseOperations,
    @unchecked Sendable
{
    let backend: RustBackend
    let rootItem = FSKItem(inode: rootInode, kind: .directory)
    private let itemCache = OSAllocatedUnfairLock(initialState: FSKItemCache())

    init(backend: RustBackend) {
        self.backend = backend
        super.init(
            volumeID: FSVolume.Identifier(uuid: UUID()),
            volumeName: FSFileName(string: "FSK")
        )
        itemCache.withLock { $0.items[rootInode] = rootItem }
    }

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumFileSizeInBits: Int { 64 }
    var maximumXattrSizeInBits: Int { 0 }
    var enableOpenUnlinkEmulation: Bool { true }

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let capabilities = FSVolume.SupportedCapabilities()
        capabilities.supportsSymbolicLinks = true
        capabilities.supportsSparseFiles = true
        capabilities.supportsZeroRuns = true
        capabilities.supportsFastStatFS = !backend.configuration.isNetwork
        capabilities.supports2TBFiles = true
        capabilities.supports64BitObjectIDs = true
        capabilities.caseFormat = .sensitive
        return capabilities
    }

    var volumeStatistics: FSStatFSResult {
        let result = FSStatFSResult(fileSystemTypeName: "fsk")
        guard let stat = try? backend.statfs() else {
            return result
        }
        result.blockSize = Int(stat.block_size)
        result.ioSize = Int(stat.io_size)
        result.totalBytes = stat.total_bytes
        result.freeBytes = stat.free_bytes
        result.availableBytes = stat.available_bytes
        result.usedBytes = stat.total_bytes >= stat.free_bytes
            ? stat.total_bytes - stat.free_bytes
            : 0
        result.totalFiles = stat.files
        result.freeFiles = stat.free_files
        return result
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        replyHandler()
    }

    func activate(
        options: FSTaskOptions,
        replyHandler: @escaping (FSItem?, (any Error)?) -> Void
    ) {
        replyHandler(rootItem, nil)
    }

    func deactivate(
        options: FSDeactivateOptions = [],
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        var firstError = backend.retryPendingCloses().map { posix($0) }
        let items = itemCache.withLock { cache -> [FSKItem] in
            let items = Array(cache.items.values)
            cache.items.removeAll(keepingCapacity: false)
            return items
        }
        for item in items {
            do {
                try closeOpenState(of: item)
            } catch {
                if firstError == nil { firstError = posix(error) }
                itemCache.withLock { $0.items[item.inode] = item }
            }
        }
        if firstError == nil, let pendingError = backend.retryPendingCloses() {
            firstError = posix(pendingError)
        }
        replyHandler(firstError)
    }

    func synchronize(flags: FSSyncFlags, replyHandler: @escaping ((any Error)?) -> Void) {
        do {
            try backend.synchronize()
            replyHandler(nil)
        } catch {
            replyHandler(posix(error))
        }
    }

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest,
        of item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem else {
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        do {
            let value = try backend.attributes(inode: item.inode)
            replyHandler(try attributes(value, wanted: desiredAttributes), nil)
        } catch {
            replyHandler(nil, posix(error))
        }
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest,
        on item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem else {
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        var update = FSKSetAttributes()
        if newAttributes.isValid(.size) {
            update.valid |= UInt32(FSK_SET_SIZE)
            update.size = newAttributes.size
        }
        if newAttributes.isValid(.mode) {
            update.valid |= UInt32(FSK_SET_MODE)
            update.mode = newAttributes.mode
        }
        if newAttributes.isValid(.uid) {
            update.valid |= UInt32(FSK_SET_UID)
            update.uid = newAttributes.uid
        }
        if newAttributes.isValid(.gid) {
            update.valid |= UInt32(FSK_SET_GID)
            update.gid = newAttributes.gid
        }
        if newAttributes.isValid(.accessTime) {
            guard let value = nanoseconds(newAttributes.accessTime) else {
                replyHandler(nil, POSIXError(.EOVERFLOW))
                return
            }
            update.valid |= UInt32(FSK_SET_ACCESSED_NS)
            update.accessed_ns = value
        }
        if newAttributes.isValid(.modifyTime) {
            guard let value = nanoseconds(newAttributes.modifyTime) else {
                replyHandler(nil, POSIXError(.EOVERFLOW))
                return
            }
            update.valid |= UInt32(FSK_SET_MODIFIED_NS)
            update.modified_ns = value
        }
        do {
            let value = try backend.setAttributes(inode: item.inode, update: update)
            if newAttributes.isValid(.size) { newAttributes.consumedAttributes.insert(.size) }
            if newAttributes.isValid(.mode) { newAttributes.consumedAttributes.insert(.mode) }
            if newAttributes.isValid(.uid) { newAttributes.consumedAttributes.insert(.uid) }
            if newAttributes.isValid(.gid) { newAttributes.consumedAttributes.insert(.gid) }
            if newAttributes.isValid(.accessTime) { newAttributes.consumedAttributes.insert(.accessTime) }
            if newAttributes.isValid(.modifyTime) { newAttributes.consumedAttributes.insert(.modifyTime) }
            replyHandler(try attributes(value, wanted: nil), nil)
        } catch {
            replyHandler(nil, posix(error))
        }
    }

    func lookupItem(
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        guard let directory = directory as? FSKItem else {
            replyHandler(nil, nil, POSIXError(.EINVAL))
            return
        }
        do {
            let result = try backend.lookup(parent: directory.inode, name: name.data)
            let item = cachedItem(inode: result.inode, kind: itemType(result.kind))
            replyHandler(item, name, nil)
        } catch {
            replyHandler(nil, nil, posix(error))
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler: @escaping ((any Error)?) -> Void) {
        guard let item = item as? FSKItem else {
            replyHandler(POSIXError(.EINVAL))
            return
        }
        let reclaimError = OSAllocatedUnfairLock<Error?>(initialState: nil)
        let wasReclaimed = itemCache.withLock { cache -> Bool in
            let cleanup: () -> Void = { [self] in
                do {
                    try closeOpenState(of: item)
                } catch {
                    reclaimError.withLockUnchecked { $0 = posix(error) }
                }
            }
            let accepted: Bool
            if #available(macOS 27.0, *) {
                accepted = tryReclaim(item, block: cleanup)
            } else {
                cleanup()
                accepted = true
            }
            if accepted,
               reclaimError.withLock({ $0 }) == nil,
               item.inode != rootInode
            {
                cache.items.removeValue(forKey: item.inode)
            }
            return accepted
        }
        let error = reclaimError.withLock { $0 }
        replyHandler(wasReclaimed ? error : nil)
    }

    func readSymbolicLink(
        _ item: FSItem,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem else {
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        do {
            replyHandler(FSFileName(data: try backend.readlink(inode: item.inode)), nil)
        } catch {
            replyHandler(nil, posix(error))
        }
    }

    func createItem(
        named name: FSFileName,
        type: FSItem.ItemType,
        inDirectory directory: FSItem,
        attributes: FSItem.SetAttributesRequest,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        guard let directory = directory as? FSKItem,
              let kind = backendKind(type)
        else {
            replyHandler(nil, nil, POSIXError(.EINVAL))
            return
        }
        let mode = attributes.isValid(.mode) ? attributes.mode : (type == .directory ? 0o755 : 0o644)
        do {
            let inode = try backend.create(parent: directory.inode, name: name.data, kind: kind, mode: mode)
            if attributes.isValid(.mode) { attributes.consumedAttributes.insert(.mode) }
            replyHandler(cachedItem(inode: inode, kind: type), name, nil)
        } catch {
            replyHandler(nil, nil, posix(error))
        }
    }

    func createSymbolicLink(
        named name: FSFileName,
        inDirectory directory: FSItem,
        attributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        guard let directory = directory as? FSKItem else {
            replyHandler(nil, nil, POSIXError(.EINVAL))
            return
        }
        let mode = attributes.isValid(.mode) ? attributes.mode : 0o777
        do {
            let inode = try backend.symlink(
                parent: directory.inode,
                name: name.data,
                target: contents.data,
                mode: mode
            )
            if attributes.isValid(.mode) { attributes.consumedAttributes.insert(.mode) }
            replyHandler(cachedItem(inode: inode, kind: .symlink), name, nil)
        } catch {
            replyHandler(nil, nil, posix(error))
        }
    }

    func createLink(
        to item: FSItem,
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, POSIXError(.ENOTSUP))
    }

    func removeItem(
        _ item: FSItem,
        named name: FSFileName,
        fromDirectory directory: FSItem,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem,
              let directory = directory as? FSKItem
        else {
            replyHandler(POSIXError(.EINVAL))
            return
        }
        do {
            try backend.remove(parent: directory.inode, name: name.data, inode: item.inode)
            replyHandler(nil)
        } catch {
            replyHandler(posix(error))
        }
    }

    func renameItem(
        _ item: FSItem,
        inDirectory sourceDirectory: FSItem,
        named sourceName: FSFileName,
        to destinationName: FSFileName,
        inDirectory destinationDirectory: FSItem,
        overItem: FSItem?,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        guard let sourceDirectory = sourceDirectory as? FSKItem,
              let destinationDirectory = destinationDirectory as? FSKItem
        else {
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        do {
            try backend.rename(
                sourceParent: sourceDirectory.inode,
                sourceName: sourceName.data,
                destinationParent: destinationDirectory.inode,
                destinationName: destinationName.data,
                replacedInode: (overItem as? FSKItem)?.inode
            )
            replyHandler(destinationName, nil)
        } catch {
            replyHandler(nil, posix(error))
        }
    }

    func enumerateDirectory(
        _ directory: FSItem,
        startingAt cookie: FSDirectoryCookie,
        verifier: FSDirectoryVerifier,
        attributes requestedAttributes: FSItem.GetAttributesRequest?,
        packer: FSDirectoryEntryPacker,
        replyHandler: @escaping (FSDirectoryVerifier, (any Error)?) -> Void
    ) {
        guard let directory = directory as? FSKItem else {
            replyHandler(verifier, POSIXError(.EINVAL))
            return
        }

        do {
            if requestedAttributes == nil && cookie.rawValue < directoryCookieBias {
                if cookie.rawValue == 0 {
                    let itemID = try itemIdentifier(directory.inode)
                    if !packer.packEntry(
                        name: FSFileName(string: "."),
                        itemType: .directory,
                        itemID: itemID,
                        nextCookie: FSDirectoryCookie(1),
                        attributes: nil
                    ) {
                        replyHandler(verifier, nil)
                        return
                    }
                }
                let parent = (try? backend.attributes(inode: directory.inode).parent) ?? directory.inode
                let parentID: FSItem.Identifier = directory.inode == rootInode
                    ? .parentOfRoot
                    : try itemIdentifier(parent)
                if !packer.packEntry(
                    name: FSFileName(string: ".."),
                    itemType: .directory,
                    itemID: parentID,
                    nextCookie: FSDirectoryCookie(directoryCookieBias),
                    attributes: nil
                ) {
                    replyHandler(verifier, nil)
                    return
                }
            }

            let rustCookie = cookie.rawValue >= directoryCookieBias
                ? cookie.rawValue - directoryCookieBias
                : 0
            let batch = try backend.readDirectory(
                inode: directory.inode,
                cookie: rustCookie,
                verifier: verifier.rawValue
            )
            for entry in batch.entries {
                let entryAttributes: FSItem.Attributes?
                if let requestedAttributes,
                   let raw = try? backend.attributes(inode: entry.inode) {
                    entryAttributes = try attributes(raw, wanted: requestedAttributes)
                } else {
                    entryAttributes = nil
                }
                let itemID = try itemIdentifier(entry.inode)
                if !packer.packEntry(
                    name: FSFileName(data: batch.name(for: entry)),
                    itemType: itemType(entry.kind),
                    itemID: itemID,
                    nextCookie: FSDirectoryCookie(entry.next_cookie + directoryCookieBias),
                    attributes: entryAttributes
                ) {
                    break
                }
            }
            replyHandler(FSDirectoryVerifier(batch.verifier), nil)
        } catch {
            replyHandler(verifier, posix(error))
        }
    }

    func read(
        from item: FSItem,
        at offset: off_t,
        length: Int,
        into buffer: FSMutableFileDataBuffer,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem, offset >= 0 else {
            replyHandler(0, POSIXError(.EINVAL))
            return
        }
        do {
            var count = 0
            var operationError: Error?
            buffer.withUnsafeMutableBytes { bytes in
                do {
                    let target = UnsafeMutableRawBufferPointer(rebasing: bytes.prefix(length))
                    if let localCount = try item.readLocal(
                        offset: offset,
                        length: length,
                        into: target
                    ) {
                        count = localCount
                    } else {
                        count = try backend.read(
                            inode: item.inode,
                            offset: UInt64(offset),
                            into: target
                        )
                    }
                } catch {
                    operationError = error
                }
            }
            if let operationError {
                throw operationError
            }
            replyHandler(count, nil)
        } catch {
            replyHandler(0, posix(error))
        }
    }

    func write(
        contents: Data,
        to item: FSItem,
        at offset: off_t,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem, offset >= 0 else {
            replyHandler(0, POSIXError(.EINVAL))
            return
        }
        do {
            replyHandler(try backend.write(inode: item.inode, offset: UInt64(offset), data: contents), nil)
        } catch {
            replyHandler(0, posix(error))
        }
    }

    func openItem(
        _ item: FSItem,
        modes: FSVolume.OpenModes,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem else {
            replyHandler(POSIXError(.EINVAL))
            return
        }
        guard backend.configuration.isNetwork, item.kind == .file else {
            replyHandler(nil)
            return
        }
        let opened: RustOpenHandle
        do {
            opened = try backend.open(inode: item.inode, writable: modes.contains(.write))
        } catch {
            replyHandler(posix(error))
            return
        }
        guard item.install(opened.file) else {
            // A second open shares the first lease. Release the duplicate on
            // its creating connection, then discard that connection even if
            // the close fails so pending-lease cleanup can run.
            do {
                try opened.reject()
                replyHandler(nil)
            } catch {
                replyHandler(posix(error))
            }
            return
        }
        do {
            try opened.acknowledgeLease()
        } catch let error as RustOpenAcknowledgementError {
            item.removeOpenState(lease: opened.file.lease)
            if !error.isTransport {
                try? opened.reject()
            }
            replyHandler(posix(error.underlying))
            return
        } catch {
            item.removeOpenState(lease: opened.file.lease)
            try? opened.reject()
            replyHandler(posix(error))
            return
        }
        opened.finish()
        replyHandler(nil)
    }

    func closeItem(
        _ item: FSItem,
        modes: FSVolume.OpenModes,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        guard let item = item as? FSKItem else {
            replyHandler(POSIXError(.EINVAL))
            return
        }
        guard modes.isEmpty else {
            replyHandler(nil)
            return
        }
        let lease: UInt64
        switch item.beginClose() {
        case .started(let value):
            lease = value
        case .idle, .closing:
            replyHandler(nil)
            return
        }
        do {
            try backend.close(lease: lease)
            item.finishClose(lease: lease, succeeded: true)
            replyHandler(nil)
        } catch {
            item.finishClose(lease: lease, succeeded: false)
            replyHandler(posix(error))
        }
    }

    private func closeOpenState(of item: FSKItem) throws {
        let lease: UInt64
        switch item.beginClose() {
        case .started(let value):
            lease = value
        case .idle:
            return
        case .closing:
            throw POSIXError(.EBUSY)
        }
        do {
            try backend.close(lease: lease)
            item.finishClose(lease: lease, succeeded: true)
        } catch {
            item.finishClose(lease: lease, succeeded: false)
            throw error
        }
    }

    private func tryReclaim(_ item: FSItem, block: @escaping () -> Void) -> Bool {
        let selector = NSSelectorFromString("tryReclaimWithBlock:")
        guard let method = class_getInstanceMethod(FSItem.self, selector) else {
            return false
        }
        let implementation = unsafeBitCast(method_getImplementation(method), to: FSKTryReclaimIMP.self)
        let reclaimBlock: @convention(block) () -> Void = block
        return implementation(item, selector, reclaimBlock)
    }

    private func cachedItem(inode: UInt64, kind: FSItem.ItemType) -> FSKItem {
        itemCache.withLock { cache in
            if let existing = cache.items[inode] {
                return existing
            }
            let item = FSKItem(inode: inode, kind: kind)
            cache.items[inode] = item
            return item
        }
    }

    private func attributes(
        _ value: FSKAttributes,
        wanted: FSItem.GetAttributesRequest?
    ) throws -> FSItem.Attributes {
        let result = FSItem.Attributes()
        func wants(_ attribute: FSItem.Attribute) -> Bool {
            wanted?.isAttributeWanted(attribute) ?? true
        }
        if wants(.fileID) { result.fileID = try itemIdentifier(value.inode) }
        if wants(.parentID) {
            result.parentID = value.inode == rootInode ? .parentOfRoot : try itemIdentifier(value.parent)
        }
        if wants(.type) { result.type = itemType(value.kind) }
        if wants(.mode) { result.mode = value.mode }
        if wants(.uid) { result.uid = value.uid }
        if wants(.gid) { result.gid = value.gid }
        if wants(.linkCount) { result.linkCount = value.link_count }
        if wants(.size) { result.size = value.size }
        if wants(.allocSize) { result.allocSize = value.allocated_size }
        if wants(.birthTime) { result.birthTime = timespec(value.created_ns) }
        if wants(.modifyTime) { result.modifyTime = timespec(value.modified_ns) }
        if wants(.accessTime) { result.accessTime = timespec(value.accessed_ns) }
        if wants(.inhibitKernelOffloadedIO),
           backend.configuration.isNetwork,
           backend.configuration.isLazy
        {
            result.inhibitKernelOffloadedIO = true
        }
        return result
    }

    private func itemIdentifier(_ inode: UInt64) throws -> FSItem.Identifier {
        guard inode != rootInode else { return .rootDirectory }
        guard inode != 0 else { return .invalid }
        let (rawValue, overflow) = inode.addingReportingOverflow(1)
        guard !overflow, let identifier = FSItem.Identifier(rawValue: rawValue) else {
            throw POSIXError(.EOVERFLOW)
        }
        return identifier
    }

    private func itemType(_ kind: UInt8) -> FSItem.ItemType {
        switch kind {
        case UInt8(FSK_KIND_FILE): .file
        case UInt8(FSK_KIND_DIRECTORY): .directory
        case UInt8(FSK_KIND_SYMLINK): .symlink
        default: .unknown
        }
    }

    private func backendKind(_ type: FSItem.ItemType) -> UInt8? {
        switch type {
        case .file: UInt8(FSK_KIND_FILE)
        case .directory: UInt8(FSK_KIND_DIRECTORY)
        default: nil
        }
    }

    private func posix(_ error: Error) -> Error {
        (error as? RustBackendError)?.asPOSIXError ?? error
    }

    private func timespec(_ nanoseconds: UInt64) -> timespec {
        Foundation.timespec(
            tv_sec: Int(nanoseconds / 1_000_000_000),
            tv_nsec: Int(nanoseconds % 1_000_000_000)
        )
    }

    private func nanoseconds(_ time: timespec) -> UInt64? {
        guard time.tv_sec >= 0, time.tv_nsec >= 0, time.tv_nsec < 1_000_000_000 else { return nil }
        let (seconds, secondsOverflow) = UInt64(time.tv_sec).multipliedReportingOverflow(by: 1_000_000_000)
        let (total, additionOverflow) = seconds.addingReportingOverflow(UInt64(time.tv_nsec))
        guard !secondsOverflow, !additionOverflow else { return nil }
        return total
    }
}
