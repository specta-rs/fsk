import ExtensionFoundation
// FSKit extension entry point.
import FSKit

@main
struct FSBindExtension: UnaryFileSystemExtension {
    let fileSystem = FSBindFileSystem()
}
