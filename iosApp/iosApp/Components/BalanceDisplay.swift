import Shared
import SwiftUI

/// The PWA's `BalanceDisplay` (U18, KTD-11, R12): unified total as a BIP177
/// `₿` amount in the display font, a `+₿X pending` line, and a hide/show
/// toggle. Visibility is persisted by the caller under the PWA's
/// `balance-visible` key; hidden renders six dots. The readout scales down
/// past 5 digits (the PWA's clamp equivalent: text-7xl → text-5xl).
///
/// `FormatKt` is one of the two UI-safe pure helpers views may consume from
/// the shared framework directly (R14 carve-out) — identical formatting on
/// both clients comes from the same commonMain code.
struct BalanceDisplay: View {
    let balanceSats: Int64
    let visible: Bool
    let onToggleVisible: () -> Void
    var pendingSats: Int64?
    var breakdown: String?
    var loading: Bool = false

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if loading {
                ProgressView()
                    .tint(colors.onField)
                    .frame(width: 32, height: 32)
                    .accessibilityLabel("Loading balance")
            } else {
                readout
                if let pendingSats, pendingSats > 0, visible {
                    Text("+\(FormatKt.formatBtc(sats: pendingSats)) pending")
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.onFieldMuted)
                        .padding(.top, 4)
                }
                if let breakdown, visible {
                    Text(breakdown)
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.onFieldMuted)
                        .padding(.top, 4)
                }
                toggle
            }
        }
    }

    @ViewBuilder
    private var readout: some View {
        if visible {
            let formatted = FormatKt.formatBtc(sats: balanceSats)
            // Digit-count breakpoint replaces the PWA's vw clamp: 5 digits or
            // fewer read at 72pt, longer amounts drop to 48pt.
            let digits = formatted.filter(\.isNumber).count
            Text(formatted)
                .font(ZinqqFont.display(digits > 5 ? 48 : 72, weight: .bold))
                .kerning(-1)
                .foregroundColor(colors.onField)
                .lineLimit(1)
                .accessibilityLabel("Balance \(formatted)")
        } else {
            Text("••••••")
                .font(ZinqqFont.display(36, weight: .bold))
                .kerning(4)
                .foregroundColor(colors.onField)
                .accessibilityLabel("Balance hidden")
        }
    }

    private var toggle: some View {
        Button(action: onToggleVisible) {
            HStack(spacing: 8) {
                Image(systemName: visible ? "eye.slash" : "eye")
                    .font(.system(size: 16))
                Text(visible ? "Hide balance" : "Show balance")
                    .font(ZinqqFont.sans(14, weight: .medium))
            }
            .foregroundColor(colors.onFieldMuted)
            .frame(minHeight: ZinqqDimens.minTouchTarget)
        }
        .padding(.top, 4)
        .accessibilityLabel(visible ? "Hide balance" : "Show balance")
    }
}
