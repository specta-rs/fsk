import ExtensionFoundation
import FSKit

@main
struct FSBindExtension: UnaryFileSystemExtension {
    let fileSystem = FSBindFileSystem()
}

