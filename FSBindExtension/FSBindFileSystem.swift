import Foundation
import FSKit

@objc
final class FSBindFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        guard resource is FSPathURLResource else {
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
        guard resource is FSPathURLResource else {
            replyHandler(nil, POSIXError(.EINVAL))
            return
        }
        if options.taskOptions.contains(where: { $0.contains("-f") }) {
            replyHandler(nil, POSIXError(.ENOTSUP))
            return
        }
        containerStatus = .ready
        replyHandler(FSBindVolume(backend: RustBackend()), nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        replyHandler(nil)
    }
}

