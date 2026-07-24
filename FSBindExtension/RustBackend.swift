import Foundation

enum RustBackendError: Error, Equatable {
    case posix(Int32)

    var asPOSIXError: POSIXError {
        let code: POSIXErrorCode = POSIXErrorCode(rawValue: posixCode) ?? .EIO
        return POSIXError(code)
    }

    var posixCode: Int32 {
        switch self {
        case let .posix(code): code
        }
    }
}

struct RustDirectoryBatch {
    let entries: [FSBDirEntry]
    let names: Data
    let verifier: UInt64
    let nextCookie: UInt64
    let eof: Bool

    func name(for entry: FSBDirEntry) -> Data {
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

final class RustBackend: @unchecked Sendable {
    private let handle: FSBVolumeRef
    let configuration: RustBackendConfiguration

    init(resourceURL: URL = URL(fileURLWithPath: "/")) throws {
        precondition(
            fsb_abi_version() == FSB_ABI_VERSION,
            "The linked Rust backend uses an incompatible FSBind ABI"
        )
        var creationError: Int32 = 0
        let resource = Data(resourceURL.absoluteString.utf8)
        let createdHandle = resource.withUnsafeBytes { bytes in
            fsb_volume_create(
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                &creationError
            )
        }
        guard let createdHandle else {
            throw RustBackendError.posix(creationError == 0 ? Int32(EIO) : creationError)
        }
        handle = createdHandle
        var rawConfiguration = FSBConfiguration()
        let status = fsb_configuration(handle, &rawConfiguration)
        guard status == 0 else {
            fsb_volume_destroy(handle)
            throw RustBackendError.posix(status)
        }
        configuration = RustBackendConfiguration(
            isNetwork: rawConfiguration.kind == UInt8(FSB_FILESYSTEM_NETWORK),
            isLazy: rawConfiguration.lazy != 0,
            supportsLocalFiles: rawConfiguration.supports_local_files != 0,
            kernelCachePolicy: rawConfiguration.kernel_cache_policy
        )
    }

    deinit {
        fsb_volume_destroy(handle)
    }

    func statfs() throws -> FSBStatFS {
        var value = FSBStatFS()
        try check(fsb_statfs(handle, &value))
        return value
    }

    func attributes(inode: UInt64) throws -> FSBAttributes {
        var value = FSBAttributes()
        try check(fsb_getattr(handle, inode, &value))
        return value
    }

    func setAttributes(inode: UInt64, update: FSBSetAttributes) throws -> FSBAttributes {
        var update = update
        var value = FSBAttributes()
        try check(fsb_setattr(handle, inode, &update, &value))
        return value
    }

    func lookup(parent: UInt64, name: Data) throws -> (inode: UInt64, kind: UInt8) {
        var inode: UInt64 = 0
        var kind: UInt8 = 0
        let status = name.withUnsafeBytes { bytes in
            fsb_lookup(
                handle,
                parent,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                &inode,
                &kind
            )
        }
        try check(status)
        return (inode, kind)
    }

    func readDirectory(
        inode: UInt64,
        cookie: UInt64,
        verifier: UInt64,
        entryCapacity: Int = 512,
        nameCapacity: Int = 64 * 1024
    ) throws -> RustDirectoryBatch {
        precondition(entryCapacity > 0 && nameCapacity > 0)
        var entries = Array(repeating: FSBDirEntry(), count: entryCapacity)
        var names = Data(count: nameCapacity)
        var entryCount = 0
        var nameCount = 0
        var nextCookie: UInt64 = cookie
        var currentVerifier: UInt64 = verifier
        var eof: UInt8 = 0

        let status = entries.withUnsafeMutableBufferPointer { entriesBuffer in
            names.withUnsafeMutableBytes { namesBuffer in
                fsb_readdir(
                    handle,
                    inode,
                    cookie,
                    verifier,
                    entriesBuffer.baseAddress,
                    entriesBuffer.count,
                    namesBuffer.bindMemory(to: UInt8.self).baseAddress,
                    namesBuffer.count,
                    &entryCount,
                    &nameCount,
                    &nextCookie,
                    &currentVerifier,
                    &eof
                )
            }
        }
        try check(status)
        entries.removeSubrange(entryCount..<entries.count)
        names.removeSubrange(nameCount..<names.count)
        return RustDirectoryBatch(
            entries: entries,
            names: names,
            verifier: currentVerifier,
            nextCookie: nextCookie,
            eof: eof != 0
        )
    }

    func read(inode: UInt64, offset: UInt64, into buffer: UnsafeMutableRawBufferPointer) throws -> Int {
        var count = 0
        let status = fsb_read(
            handle,
            inode,
            offset,
            buffer.bindMemory(to: UInt8.self).baseAddress,
            buffer.count,
            &count
        )
        try check(status)
        return count
    }

    func write(inode: UInt64, offset: UInt64, data: Data) throws -> Int {
        var count = 0
        let status = data.withUnsafeBytes { bytes in
            fsb_write(
                handle,
                inode,
                offset,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                &count
            )
        }
        try check(status)
        return count
    }

    func create(parent: UInt64, name: Data, kind: UInt8, mode: UInt32) throws -> UInt64 {
        var inode: UInt64 = 0
        let status = name.withUnsafeBytes { bytes in
            fsb_create(
                handle,
                parent,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                kind,
                mode,
                &inode
            )
        }
        try check(status)
        return inode
    }

    func symlink(parent: UInt64, name: Data, target: Data, mode: UInt32) throws -> UInt64 {
        var inode: UInt64 = 0
        let status = name.withUnsafeBytes { nameBytes in
            target.withUnsafeBytes { targetBytes in
                fsb_symlink(
                    handle,
                    parent,
                    nameBytes.bindMemory(to: UInt8.self).baseAddress,
                    nameBytes.count,
                    targetBytes.bindMemory(to: UInt8.self).baseAddress,
                    targetBytes.count,
                    mode,
                    &inode
                )
            }
        }
        try check(status)
        return inode
    }

    func readlink(inode: UInt64, capacity: Int = 4096) throws -> Data {
        var data = Data(count: capacity)
        var count = 0
        let status = data.withUnsafeMutableBytes { bytes in
            fsb_readlink(
                handle,
                inode,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                &count
            )
        }
        try check(status)
        data.removeSubrange(count..<data.count)
        return data
    }

    func remove(parent: UInt64, name: Data, inode: UInt64) throws {
        let status = name.withUnsafeBytes { bytes in
            fsb_remove(
                handle,
                parent,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                inode
            )
        }
        try check(status)
    }

    func rename(
        sourceParent: UInt64,
        sourceName: Data,
        destinationParent: UInt64,
        destinationName: Data,
        replacedInode: UInt64?
    ) throws {
        let status = sourceName.withUnsafeBytes { sourceBytes in
            destinationName.withUnsafeBytes { destinationBytes in
                fsb_rename(
                    handle,
                    sourceParent,
                    sourceBytes.bindMemory(to: UInt8.self).baseAddress,
                    sourceBytes.count,
                    destinationParent,
                    destinationBytes.bindMemory(to: UInt8.self).baseAddress,
                    destinationBytes.count,
                    replacedInode ?? 0
                )
            }
        }
        try check(status)
    }

    func synchronize() throws {
        try check(fsb_sync(handle))
    }

    func open(inode: UInt64, writable: Bool) throws -> RustOpenFile {
        var path = Data(count: Int(MAXPATHLEN))
        var opened = FSBOpenFile()
        let status = path.withUnsafeMutableBytes { bytes in
            fsb_open_file(
                handle,
                inode,
                writable ? 1 : 0,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                &opened
            )
        }
        try check(status)
        let localPath: Data?
        if opened.source == UInt8(FSB_FILE_LOCAL) {
            path.removeSubrange(Int(opened.path_length)..<path.count)
            localPath = path
        } else {
            localPath = nil
        }
        return RustOpenFile(
            lease: opened.lease,
            generation: opened.generation,
            localPath: localPath
        )
    }

    func close(lease: UInt64) throws {
        try check(fsb_close_file(handle, lease))
    }

    private func check(_ status: Int32) throws {
        if status != 0 {
            throw RustBackendError.posix(status)
        }
    }
}
