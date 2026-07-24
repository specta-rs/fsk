import SwiftUI

@main
struct FSBindApp: App {
    var body: some Scene {
        WindowGroup {
            VStack(spacing: 12) {
                Image(systemName: "externaldrive.connected.to.line.below")
                    .font(.system(size: 44))
                Text("FSBind")
                    .font(.title)
                Text("Enable the file system extension in System Settings, then mount it from Terminal.")
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: 420)
            }
            .padding(40)
        }
        .windowResizability(.contentSize)
    }
}

