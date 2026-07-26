import SwiftUI
import Shared

struct ContentView: View {
    @State private var pong = "pinging…"

    var body: some View {
        VStack(spacing: 12) {
            Text(WalletCore.shared.coreVersion())
            Text(pong)
        }
        .padding(24)
        .task {
            do {
                pong = try await WalletCore.shared.pingAsync()
            } catch {
                pong = "ping failed: \(error)"
            }
        }
    }
}
