import Darwin
import Foundation
// FSKit item representation.
import FSKit
import os

private struct OpenState: Sendable {
    let lease: UInt64
    let localDescriptor: Int32?
}

final class FSKItem: FSItem, @unchecked Sendable {
    let inode: UInt64
    let kind: FSItem.ItemType
    private let openState = OSAllocatedUnfairLock<OpenState?>(initialState: nil)

    init(inode: UInt64, kind: FSItem.ItemType) {
        self.inode = inode
        self.kind = kind
        super.init()
    }

    /// Installs the open state for `opened`.
    ///
    /// Returns `false` when another open already holds this item's lease; the
    /// caller then owns (and must release) the duplicate lease.
    ///
    /// A local file that can't be opened — because it was evicted, isn't UTF-8,
    /// or the sandbox denies a path outside the extension's container — is
    /// ignored so reads fall back to the backend instead of failing the open.
    @discardableResult
    func install(_ opened: RustOpenFile) -> Bool {
        let descriptor = opened.localPath.flatMap { path -> Int32? in
            guard let pathString = String(data: path, encoding: .utf8) else {
                return nil
            }
            let value = Darwin.open(pathString, O_RDONLY)
            return value >= 0 ? value : nil
        }
        let installed = openState.withLockUnchecked { state -> Bool in
            guard state == nil else {
                return false
            }
            state = OpenState(lease: opened.lease, localDescriptor: descriptor)
            return true
        }
        if !installed, let descriptor {
            Darwin.close(descriptor)
        }
        return installed
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
