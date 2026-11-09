import AppKit
import Combine
import FSKit
// FSK's install-once macOS authorization application.
import SwiftUI

private let extensionIdentifier = "dev.specta.fsk.extension"

@main
struct FSKApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var model = AuthorizationModel()

    init() {
        if CommandLine.arguments.contains("--fsk-authorization-status") {
            printAuthorizationStatusAndExit()
        }
    }

    var body: some Scene {
        WindowGroup("FSK Setup") {
            AuthorizationView(model: model)
        }
        .defaultSize(width: 440, height: 330)
        .defaultPosition(.center)
        .windowResizability(.contentSize)
    }
}

private final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // FSK is a background setup utility, so LSUIElement keeps it out of the
        // Dock. The setup window still needs to come forward when it opens.
        NSApp.activate()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

@MainActor
@Observable
private final class AuthorizationModel {
    enum Status: Equatable {
        case checking
        case notInstalled
        case needsApproval
        case authorized
        case failed(String)
    }

    var status: Status = .checking

    /// Keep the permission row live while the app is open. FSKit does not
    /// publish a change notification when the user flips the System Settings
    /// switch, so a short poll is the reliable way to mirror that state.
    func monitor() async {
        await refresh()
        while !Task.isCancelled {
            try? await Task.sleep(for: .seconds(1))
            await refresh()
        }
    }

    func refresh() async {
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

    /// Open the exact File System Extensions pane. FSKit exposes this action
    /// so the app never has to guess at a System Settings URL.
    func openSettings() {
        if #available(macOS 27.0, *), FSClient.shared.openFileSystemExtensionsSettings() {
            return
        }
        let url = URL(
            string: "x-apple.systempreferences:com.apple.ExtensionsPreferences?extensionPointIdentifier=com.apple.fskit.fsmodule"
        )!
        NSWorkspace.shared.open(url)
    }

    func revealApp() {
        NSWorkspace.shared.activateFileViewerSelecting([Bundle.main.bundleURL])
    }

    func quit() {
        NSApplication.shared.terminate(nil)
    }
}

private struct AuthorizationView: View {
    let model: AuthorizationModel

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            header
            permissionCard
            Spacer(minLength: 0)
            HStack(spacing: 10) {
                Button("Quit") {
                    model.quit()
                }
                .keyboardShortcut("q", modifiers: .command)

                if case .notInstalled = model.status {
                    Button("Show FSK.app") {
                        model.revealApp()
                    }
                }

                Spacer()
                primaryAction
            }
            footer
        }
        .padding(24)
        .frame(width: 440)
        .background(Color(nsColor: .windowBackgroundColor))
        .task {
            await model.monitor()
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            Task { await model.refresh() }
        }
    }

    private var header: some View {
        HStack(spacing: 14) {
            Image(systemName: "externaldrive.connected.to.line.below")
                .font(.system(size: 26, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 52, height: 52)
                .background(Color.accentColor.gradient, in: RoundedRectangle(cornerRadius: 13))
            VStack(alignment: .leading, spacing: 3) {
                Text("FSK Setup")
                    .font(.title2.bold())
                Text("One-time permission for virtual filesystems")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var permissionCard: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: "externaldrive.badge.checkmark")
                .font(.title3)
                .foregroundStyle(statusColor)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 3) {
                Text("File System Extension")
                    .font(.headline)
                Text(statusDetail)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            statusBadge
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    @ViewBuilder
    private var statusBadge: some View {
        switch model.status {
        case .checking:
            ProgressView()
                .controlSize(.small)
        case .authorized:
            Text("Enabled")
                .font(.callout.weight(.medium))
                .foregroundStyle(.green)
        case .needsApproval:
            Text("Required")
                .font(.callout.weight(.medium))
                .foregroundStyle(.orange)
        case .notInstalled:
            Text("Missing")
                .font(.callout.weight(.medium))
                .foregroundStyle(.red)
        case .failed:
            Button("Retry") {
                Task { await model.refresh() }
            }
        }
    }

    @ViewBuilder
    private var primaryAction: some View {
        switch model.status {
        case .authorized:
            Button("Done") {
                model.quit()
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
        case .checking:
            Button("Open Settings") {
                model.openSettings()
            }
            .buttonStyle(.borderedProminent)
            .disabled(true)
        default:
            Button("Open Settings") {
                model.openSettings()
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
        }
    }

    private var footer: some View {
        Text("FSK.app only opens for setup. Enable the extension once; every Rust mount reuses it.")
            .font(.caption)
            .foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
    }

    private var statusColor: Color {
        switch model.status {
        case .checking:
            .secondary
        case .authorized:
            .green
        case .needsApproval:
            .orange
        case .notInstalled, .failed:
            .red
        }
    }

    private var statusDetail: String {
        switch model.status {
        case .checking:
            "Checking with macOS…"
        case .authorized:
            "Enabled. FSK can mount filesystems."
        case .needsApproval:
            "Turn on FSK under File System Extensions."
        case .notInstalled:
            "Not registered. Keep FSK.app in Applications, then open it again."
        case let .failed(message):
            message
        }
    }
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
