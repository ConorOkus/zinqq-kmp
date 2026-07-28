import SwiftUI

extension View {
    /// Auto-clears `flag` back to `false` `afterMs` milliseconds after it
    /// turns true — the PWA's "Copied!" flash / invalid-scan toast timers.
    /// Each call site keeps its own PWA duration.
    func autoReset(_ flag: Binding<Bool>, afterMs: UInt64) -> some View {
        task(id: flag.wrappedValue) {
            if flag.wrappedValue {
                try? await Task.sleep(nanoseconds: afterMs * 1_000_000)
                flag.wrappedValue = false
            }
        }
    }
}
