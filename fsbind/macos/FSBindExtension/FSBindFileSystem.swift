import Foundation
// FSKit filesystem implementation backed by a Rust process.
import FSKit

@objc
final class FSBindFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        guard let resource = resource as? FSGenericURLResource,
              resource.url.scheme?.lowercased() == "fsbind"
        else {
            replyHandler(nil, POSIXError(.ENODEV))
            return
        }
        let identifier = FSContainerIdentifier(uuid: UUID())
        replyHandler(.usable(name: "FSBind", containerID: identifier), nil)
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        let resourceURL: URL
        if let genericResource = resource as? FSGenericURLResource {
            resourceURL = genericResource.url
        } else {
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        do {
            let backend = try RustBackend(resourceURL: resourceURL)
            containerStatus = .ready
            replyHandler(FSBindVolume(backend: backend), nil)
        } catch let error as RustBackendError {
            replyHandler(nil, error.asPOSIXError)
        } catch {
            replyHandler(nil, error)
        }
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        replyHandler(nil)
    }
}
