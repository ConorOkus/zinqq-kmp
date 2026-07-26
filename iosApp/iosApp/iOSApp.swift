import SwiftUI

@main
struct iOSApp: App {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var model = WalletModel()

    var body: some Scene {
        WindowGroup {
            ContentView(model: model)
        }
        // KTD-10: foreground-only node lifecycle. Start on active, stop on
        // background; iOS drops sockets on suspend and reconnect on the next
        // start is the core's job — the shell only signals lifecycle.
        .onChange(of: scenePhase) { phase in
            switch phase {
            case .active:
                model.start()
            case .background:
                model.stop()
            default:
                break
            }
        }
    }
}
