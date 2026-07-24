import AppKit
import ExtensionKit
import FSKit
// UFSI's install-once macOS authorization application.
import SwiftUI

private let extensionIdentifier = "dev.specta.ufsi.extension"

@main
struct UFSIApp: App {
    @State private var model = AuthorizationModel()

    init() {
        if CommandLine.arguments.contains("--ufsi-authorization-status") {
            printAuthorizationStatusAndExit()
        }
    }

    var body: some Scene {
        WindowGroup {
            AuthorizationView(model: model)
                .frame(minWidth: 660, idealWidth: 720, minHeight: 570, idealHeight: 640)
                .task {
                    await model.refresh()
                }
        }
        .defaultSize(width: 720, height: 640)
        .windowResizability(.contentMinSize)
    }
}

@MainActor
@Observable
private final class AuthorizationModel {
    enum Status {
        case checking
        case notInstalled
        case needsApproval
        case authorized
        case failed(String)
    }

    var status: Status = .checking

    func refresh() async {
        status = .checking
        do {
            let modules = try await FSClient.shared.installedExtensions
            guard let module = modules.first(where: { $0.bundleIdentifier == extensionIdentifier }) else {
                status = .notInstalled
                return
            }
            status = module.isEnabled ? .authorized : .needsApproval
        } catch {
            status = .failed(error.localizedDescription)
        }
    }

    func openSystemSettings() {
        let url = URL(
            string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"
        )!
        NSWorkspace.shared.open(url)
    }
}

private struct AuthorizationView: View {
    let model: AuthorizationModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                header
                statusCard
                if case .authorized = model.status {
                    readyCard
                } else {
                    permissionCard
                    steps
                }
                footer
            }
            .padding(32)
        }
        .background(
            LinearGradient(
                colors: [Color(nsColor: .windowBackgroundColor), Color.accentColor.opacity(0.05)],
                startPoint: .top,
                endPoint: .bottom
            )
        )
    }

    private var header: some View {
        HStack(spacing: 16) {
            Image(systemName: "externaldrive.connected.to.line.below")
                .font(.system(size: 34, weight: .medium))
                .foregroundStyle(.white)
                .frame(width: 64, height: 64)
                .background(Color.accentColor.gradient, in: RoundedRectangle(cornerRadius: 16))
            VStack(alignment: .leading, spacing: 4) {
                Text("UFSI")
                    .font(.largeTitle.bold())
                Text("One bridge for every Rust filesystem")
                    .font(.title3)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var statusCard: some View {
        HStack(spacing: 14) {
            statusIcon
                .font(.title2)
                .frame(width: 36, height: 36)
            VStack(alignment: .leading, spacing: 3) {
                Text(statusTitle).font(.headline)
                Text(statusDetail).foregroundStyle(.secondary)
            }
            Spacer()
            Button {
                Task { await model.refresh() }
            } label: {
                Label("Check Again", systemImage: "arrow.clockwise")
            }
            .disabled(isChecking)
        }
        .padding(18)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16))
    }

    @ViewBuilder
    private var permissionCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Allow the file system extension")
                .font(.headline)
            Text("Use the macOS control below to enable UFSI. This permission lets the signed bridge receive filesystem requests; your Rust programs and their data stay separate.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            ExtensionBrowser()
                .frame(height: 104)
                .clipShape(RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(.separator))
        }
    }

    private var steps: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("If macOS asks you to use System Settings")
                .font(.headline)
            StepRow(number: 1, text: "Open Login Items & Extensions.")
            StepRow(number: 2, text: "Open File System Extensions and enable UFSI.")
            StepRow(number: 3, text: "Return here and choose Check Again.")
            Button {
                model.openSystemSettings()
            } label: {
                Label("Open System Settings", systemImage: "gear")
            }
            .buttonStyle(.borderedProminent)
        }
    }

    private var readyCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("Ready for mounts", systemImage: "checkmark.seal.fill")
                .font(.title3.bold())
                .foregroundStyle(.green)
            Text("You can close this window. Start any Rust program using `MountSession`; UFSI will connect it to the installed extension for the lifetime of that program.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(18)
        .background(Color.green.opacity(0.09), in: RoundedRectangle(cornerRadius: 16))
    }

    private var footer: some View {
        Text("UFSI uses a private, authenticated Unix socket for each mount. Closing the Rust program unmounts its volume and removes the socket.")
            .font(.caption)
            .foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
    }

    @ViewBuilder
    private var statusIcon: some View {
        switch model.status {
        case .checking:
            ProgressView()
        case .authorized:
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case .needsApproval:
            Image(systemName: "exclamationmark.shield.fill").foregroundStyle(.orange)
        case .notInstalled, .failed:
            Image(systemName: "xmark.circle.fill").foregroundStyle(.red)
        }
    }

    private var statusTitle: String {
        switch model.status {
        case .checking: "Checking permission…"
        case .authorized: "Extension enabled"
        case .needsApproval: "Permission required"
        case .notInstalled: "Extension not installed"
        case .failed: "Couldn’t check permission"
        }
    }

    private var statusDetail: String {
        switch model.status {
        case .checking: "Asking FSKit for the current status."
        case .authorized: "macOS is ready to mount UFSI filesystems."
        case .needsApproval: "Enable UFSI once to use it from any Rust program."
        case .notInstalled: "Move UFSI.app to Applications, then open it again."
        case let .failed(message): message
        }
    }

    private var isChecking: Bool {
        if case .checking = model.status { true } else { false }
    }
}

private struct StepRow: View {
    let number: Int
    let text: String

    var body: some View {
        HStack(spacing: 12) {
            Text("\(number)")
                .font(.caption.bold())
                .foregroundStyle(.white)
                .frame(width: 24, height: 24)
                .background(Color.accentColor, in: Circle())
            Text(text)
        }
    }
}

private struct ExtensionBrowser: NSViewControllerRepresentable {
    func makeNSViewController(context: Context) -> EXAppExtensionBrowserViewController {
        EXAppExtensionBrowserViewController()
    }

    func updateNSViewController(
        _ controller: EXAppExtensionBrowserViewController,
        context: Context
    ) {}
}

private func printAuthorizationStatusAndExit() -> Never {
    FSClient.shared.fetchInstalledExtensions { modules, error in
        if let error {
            FileHandle.standardError.write(Data("\(error.localizedDescription)\n".utf8))
            exit(1)
        }
        guard let module = modules?.first(where: { $0.bundleIdentifier == extensionIdentifier }) else {
            print("not-installed")
            exit(0)
        }
        print(module.isEnabled ? "authorized" : "needs-approval")
        exit(0)
    }
    RunLoop.main.run()
    fatalError("unreachable")
}
