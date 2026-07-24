import ExtensionFoundation
// FSKit extension entry point.
import FSKit

@main
struct UFSIExtension: UnaryFileSystemExtension {
    let fileSystem = UFSIFileSystem()
}
