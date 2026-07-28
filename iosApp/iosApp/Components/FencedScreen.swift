import SwiftUI

/// The blocking fenced screen (U18; plan "System-Wide Impact", KTD-3):
/// another client wrote divergent state into this seed's VSS namespace, the
/// core fenced itself durably, and no automatic un-fence exists. Rendered
/// above ALL destinations (top of the z-ladder) whenever the shell's fenced
/// flag is set — the two exits are user-owned: take over here (the U4
/// wipe-and-restore flow behind Settings → Restore) or quit and keep using
/// the other client. Copy matches Android's `FencedScreen` verbatim.
struct FencedScreen: View {
    let onRestore: () -> Void
    let onQuit: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            ZStack {
                Circle()
                    .fill(colors.warning.opacity(0.15))
                    .frame(width: 80, height: 80)
                Image(systemName: "exclamationmark.triangle")
                    .font(.system(size: 34, weight: .semibold))
                    .foregroundColor(colors.warning)
            }

            Text("This wallet is active on another device")
                .font(ZinqqFont.display(24, weight: .bold))
                .foregroundColor(colors.onDark)
                .multilineTextAlignment(.center)
                .padding(.top, 24)

            Text(
                "Another device took over this wallet's cloud backup. To keep "
                    + "your funds safe, this device stopped. Restore from backup to take "
                    + "over here, or quit and keep using the other device."
            )
            .font(ZinqqFont.sans(14))
            .foregroundColor(colors.onDarkMuted)
            .multilineTextAlignment(.center)
            .padding(.top, 12)

            Button(action: onRestore) {
                Text("Restore from backup")
                    .font(ZinqqFont.display(18, weight: .bold))
                    .foregroundColor(colors.onCta)
                    .frame(maxWidth: .infinity)
                    .frame(height: 56)
                    .background(colors.cta)
                    .clipShape(RoundedRectangle(cornerRadius: 12))
            }
            .padding(.top, 32)
            .accessibilityLabel("Restore from backup")

            Button(action: onQuit) {
                Text("Quit")
                    .font(ZinqqFont.sans(16, weight: .medium))
                    .foregroundColor(colors.onDark)
                    .frame(maxWidth: .infinity)
                    .frame(height: 56)
            }
            .padding(.top, 12)
            .accessibilityLabel("Quit")
            Spacer()
        }
        .padding(.horizontal, 32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // Swallow all input: nothing below this screen is interactive.
        .background(colors.dark.ignoresSafeArea())
        .contentShape(Rectangle())
    }
}
