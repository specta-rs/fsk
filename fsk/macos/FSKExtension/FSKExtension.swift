import ExtensionFoundation
// FSKit extension entry point.
import FSKit

@main
struct FSKExtension: UnaryFileSystemExtension {
    let fileSystem = FSKFileSystem()
}
