import FSKit

final class FSBindItem: FSItem, @unchecked Sendable {
    let inode: UInt64
    let kind: FSItem.ItemType

    init(inode: UInt64, kind: FSItem.ItemType) {
        self.inode = inode
        self.kind = kind
        super.init()
    }
}
