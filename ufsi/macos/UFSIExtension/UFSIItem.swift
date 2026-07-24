import Darwin
import Foundation
// FSKit item representation.
import FSKit
import os

private struct OpenState: Sendable {
    let lease: UInt64
    let localDescriptor: Int32?
}

final class UFSIItem: FSItem, @unchecked Sendable {
    let inode: UInt64
    let kind: FSItem.ItemType
    private let openState = OSAllocatedUnfairLock<OpenState?>(initialState: nil)

    init(inode: UInt64, kind: FSItem.ItemType) {
        self.inode = inode
        self.kind = kind
        super.init()
    }

    func install(_ opened: RustOpenFile) throws {
        let descriptor = try opened.localPath.map { path -> Int32 in
            guard let pathString = String(data: path, encoding: .utf8) else {
                throw POSIXError(.EILSEQ)
            }
            let value = Darwin.open(pathString, O_RDONLY)
            guard value >= 0 else {
                throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            }
            return value
        }
        let installed = openState.withLockUnchecked { state -> Bool in
            guard state == nil else {
                return false
            }
            state = OpenState(lease: opened.lease, localDescriptor: descriptor)
            return true
        }
        if !installed {
            if let descriptor {
                Darwin.close(descriptor)
            }
            throw POSIXError(.EBUSY)
        }
    }

    func readLocal(
        offset: off_t,
        length: Int,
        into buffer: UnsafeMutableRawBufferPointer
    ) throws -> Int? {
        try openState.withLockUnchecked { state in
            guard let descriptor = state?.localDescriptor else {
                return nil
            }
            let count = pread(descriptor, buffer.baseAddress, length, offset)
            guard count >= 0 else {
                throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            }
            return count
        }
    }

    func releaseOpenState() -> UInt64? {
        let state = openState.withLockUnchecked { state -> OpenState? in
            let previous = state
            state = nil
            return previous
        }
        if let descriptor = state?.localDescriptor {
            Darwin.close(descriptor)
        }
        return state?.lease
    }
}
