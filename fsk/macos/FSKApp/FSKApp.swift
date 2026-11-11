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
        Window("FSK Setup", id: "setup") {
            AuthorizationView(model: model)
        }
        .defaultSize(width: 460, height: 300)
        .defaultPosition(.center)
        .windowResizability(.contentSize)
    }
}

private final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // FSK is a background setup utility, so LSUIElement keeps it out of the
        // Dock. macOS may refuse a focus request from a newly launched
        // background app for a couple of seconds, so retry until the setup
        // window is actually key instead of giving up after one attempt.
        Task { @MainActor in
            for _ in 0..<12 {
                NSApp.activate(ignoringOtherApps: true)
                NSApp.windows.first?.makeKeyAndOrderFront(nil)
                if NSApp.isActive {
                    return
                }
                try? await Task.sleep(for: .milliseconds(250))
            }
        }
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
    var actionError: String?

    /// Keep the permission row live while the app is open. FSKit does not
    /// publish a change notification when the user flips the System Settings
    /// switch, so a short poll is the reliable way to mirror that state.
    func monitor() async {
        await refresh()
        while !Task.isCancelled {
            do {
                try await Task.sleep(for: .seconds(1))
            } catch {
                return
            }
            await refresh()
        }
    }

    func retry() {
        status = .checking
        Task { await refresh() }
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

    /// Open the exact File System Extensions pane. The macOS 26 SDK does not
    /// expose `FSClient.openFileSystemExtensionsSettings()`, so use the
    /// documented pane identifier that works on both macOS 26 and 27.
    func openSettings() {
        actionError = nil
        let url = URL(
            string: "x-apple.systempreferences:com.apple.ExtensionsPreferences?extensionPointIdentifier=com.apple.fskit.fsmodule"
        )!
        if !NSWorkspace.shared.open(url) {
            actionError = "Could not open System Settings."
        }
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

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            header
            permissionCard
            if let actionError = model.actionError {
                Label(actionError, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
            actions
        }
        .padding(24)
        .frame(width: 460)
        .background(SetupBackground())
        .onAppear {
            NSApp.activate(ignoringOtherApps: true)
            NSApp.windows.first?.makeKeyAndOrderFront(nil)
        }
        .task {
            await model.monitor()
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            Task { await model.refresh() }
        }
        .animation(statusAnimation, value: model.status)
    }

    private var statusAnimation: Animation? {
        reduceMotion ? nil : .snappy(duration: 0.28)
    }

    private var header: some View {
        HStack(spacing: 12) {
            BrandMark()
                .frame(width: 56, height: 56)
                .shadow(color: .black.opacity(0.14), radius: 6, y: 2)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text("Set up FSK")
                    .font(.title2.weight(.semibold))
                Text("Mount virtual filesystems from your apps")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var permissionCard: some View {
        HStack(alignment: .center, spacing: 14) {
            statusIcon
            VStack(alignment: .leading, spacing: 3) {
                Text("File System Extension")
                    .font(.headline)
                statusDetailText
            }
            Spacer(minLength: 12)
            statusBadge
        }
        .padding(16)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .strokeBorder(Color(nsColor: .separatorColor).opacity(0.55))
        }
        .accessibilityElement(children: .combine)
    }

    /// Detail text reserves the two lines most states need so the window keeps
    /// one size as the status changes. Failures keep their full message.
    @ViewBuilder
    private var statusDetailText: some View {
        Group {
            if case .failed = model.status {
                Text(statusDetail)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                Text(statusDetail)
                    .lineLimit(2, reservesSpace: true)
            }
        }
        .font(.callout)
        .foregroundStyle(.secondary)
        .contentTransition(.opacity)
    }

    @ViewBuilder
    private var statusIcon: some View {
        ZStack {
            Circle()
                .fill(statusColor.opacity(0.14))
            if case .checking = model.status {
                ProgressView()
                    .controlSize(.small)
            } else {
                Image(systemName: statusSymbol)
                    .font(.system(size: 16, weight: .semibold))
                    .foregroundStyle(statusColor)
                    .contentTransition(.symbolEffect(.replace))
            }
        }
        .frame(width: 38, height: 38)
        .accessibilityHidden(true)
    }

    private var statusBadge: some View {
        Text(statusLabel)
            .font(.caption.weight(.semibold))
            .foregroundStyle(statusColor)
            .padding(.horizontal, 8)
            .padding(.vertical, 3.5)
            .background(Color.primary.opacity(0.06), in: Capsule())
            .contentTransition(.opacity)
            .accessibilityLabel("Status: \(statusLabel)")
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
        case .needsApproval, .checking:
            Button("Open Settings") {
                model.openSettings()
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
        case .notInstalled:
            Button("Show FSK.app") {
                model.revealApp()
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
        case .failed:
            Button("Try Again") {
                model.retry()
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
        }
    }

    private var actions: some View {
        HStack {
            Spacer(minLength: 0)
            primaryAction
        }
        .controlSize(.large)
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

    private var statusSymbol: String {
        switch model.status {
        case .checking:
            "externaldrive"
        case .authorized:
            "checkmark"
        case .needsApproval:
            "exclamationmark"
        case .notInstalled:
            "questionmark"
        case .failed:
            "xmark"
        }
    }

    private var statusLabel: String {
        switch model.status {
        case .checking:
            "Checking"
        case .authorized:
            "Enabled"
        case .needsApproval:
            "Action Needed"
        case .notInstalled:
            "Not Found"
        case .failed:
            "Error"
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
            "Keep FSK.app in Applications, then open it again."
        case let .failed(message):
            message
        }
    }
}

/// The app icon's artwork, drawn in SwiftUI so the setup window is branded on
/// first launch even before the asset catalog is fully registered.
private struct BrandMark: View {
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 13, style: .continuous)
                .fill(tileColor)
            FSKGlyph(
                top: colorScheme == .dark
                    ? Color(red: 0.310, green: 0.443, blue: 0.588)
                    : Color(red: 0.561, green: 0.690, blue: 0.816),
                middle: colorScheme == .dark
                    ? Color(red: 0.482, green: 0.690, blue: 0.878)
                    : Color(red: 0.169, green: 0.424, blue: 0.690),
                bottom: colorScheme == .dark
                    ? Color(red: 0.910, green: 0.949, blue: 0.988)
                    : Color(red: 0.059, green: 0.180, blue: 0.302)
            )
            .padding(5)
        }
        .overlay {
            RoundedRectangle(cornerRadius: 13, style: .continuous)
                .strokeBorder(.white.opacity(colorScheme == .dark ? 0.10 : 0.55))
        }
    }

    private var tileColor: Color {
        colorScheme == .dark
            ? Color(red: 0.078, green: 0.094, blue: 0.114)
            : Color(red: 0.933, green: 0.953, blue: 0.973)
    }
}

private struct FSKGlyph: View {
    let top: Color
    let middle: Color
    let bottom: Color

    var body: some View {
        Canvas { context, size in
            let scale = size.width / 1024

            func fill(
                _ x: CGFloat,
                _ y: CGFloat,
                _ width: CGFloat,
                _ height: CGFloat,
                radius: CGFloat,
                color: Color
            ) {
                let rect = CGRect(
                    x: x * scale,
                    y: y * scale,
                    width: width * scale,
                    height: height * scale
                )
                context.fill(
                    Path(roundedRect: rect, cornerRadius: radius * scale),
                    with: .color(color)
                )
            }

            fill(204, 259, 616, 143, radius: 38.5, color: top)
            fill(204, 484.5, 500.5, 126.5, radius: 33, color: middle)
            fill(204, 677, 143, 88, radius: 27.5, color: bottom)
            fill(385.5, 677, 143, 88, radius: 27.5, color: bottom)
        }
        .aspectRatio(1, contentMode: .fit)
    }
}

private struct SetupBackground: View {
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        ZStack {
            Color(nsColor: .windowBackgroundColor)
            LinearGradient(
                colors: [
                    Color(red: 0.169, green: 0.424, blue: 0.690)
                        .opacity(colorScheme == .dark ? 0.16 : 0.07),
                    .clear,
                ],
                startPoint: .top,
                endPoint: .bottom
            )
        }
        .ignoresSafeArea()
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
