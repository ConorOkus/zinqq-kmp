import SwiftUI

/// The PWA's Home (U19, R12; `Home.tsx`), mirroring Android's `HomeScreen`:
/// field screen with a refresh icon (re-queries wallet data — no page-reload
/// concept natively; the PWA install button is omitted on native), the
/// unified BalanceDisplay, RecoveryBanner / PendingSweepBanner, and the two
/// 88pt CTAs. A fatal start failure replaces the content with "Something went
/// wrong" (`Home.tsx:29-42`). Every derivation comes from the pure helpers in
/// `WalletPresentation.swift` (R14) — this view only places results.
struct HomeScreen: View {
    @ObservedObject var model: WalletModel
    let onNavigate: (Route) -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        Group {
            if let startError = model.startError {
                errorState(startError)
            } else {
                content
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.field.ignoresSafeArea())
    }

    /// The PWA's fatal error state (`Home.tsx:29-42`).
    private func errorState(_ detail: String) -> some View {
        VStack(spacing: 8) {
            Text("Something went wrong")
                .font(ZinqqFont.sans(18, weight: .semibold))
                .foregroundColor(colors.onField)
            Text(detail)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onFieldMuted)
                .multilineTextAlignment(.center)
        }
        .padding(.horizontal, 24)
    }

    private var content: some View {
        VStack(spacing: 0) {
            // Top bar: install slot omitted on native (left spacer keeps the
            // refresh pinned right, like the PWA's placeholder div).
            HStack {
                Spacer()
                Button(action: { model.refreshWalletData() }) {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 18, weight: .medium))
                        .foregroundColor(colors.onField)
                        .frame(
                            width: ZinqqDimens.minTouchTarget,
                            height: ZinqqDimens.minTouchTarget
                        )
                }
                .accessibilityLabel("Refresh")
            }

            // Balance centered in the flexible middle, like the PWA's
            // justify-between column.
            HStack {
                let balance = model.balances.map(homeBalance)
                BalanceDisplay(
                    balanceSats: balance?.totalSats ?? 0,
                    visible: model.balanceVisible,
                    onToggleVisible: { model.balanceVisible.toggle() },
                    pendingSats: balance?.pendingSats,
                    loading: balance == nil
                )
                Spacer()
            }
            .frame(maxHeight: .infinity)

            if let banner = recoveryBanner(
                model.recoveryState, dismissed: model.recoveryBannerDismissed
            ) {
                BannerView(
                    icon: banner.dismissible ? .check : .warningHot,
                    title: banner.title,
                    subtitle: banner.subtitle,
                    onTap: banner.navigatesToRecover ? { onNavigate(.recover) } : nil,
                    onDismiss: banner.dismissible ? { model.dismissRecoveryBanner() } : nil
                )
                .padding(.bottom, 12)
            }

            if let banner = sweepBanner(model.pendingSweep) {
                BannerView(
                    icon: banner.navigatesToReceive ? .warningHot : .warning,
                    title: banner.heading,
                    subtitle: banner.subtitle,
                    onTap: banner.navigatesToReceive ? { onNavigate(.receive) } : nil
                )
                .padding(.bottom, 12)
            }

            // The two 88pt CTAs: Send filled (the field screen's hot moment),
            // Request outlined (Home.tsx:92-107).
            HStack(spacing: 12) {
                HomeCta(
                    label: "Send",
                    systemImage: "arrow.up.right",
                    background: colors.fieldCta,
                    contentColor: colors.onFieldCta,
                    outline: nil,
                    action: { onNavigate(.send) }
                )
                HomeCta(
                    label: "Request",
                    systemImage: "arrow.down.left",
                    background: .clear,
                    contentColor: colors.onField,
                    outline: colors.fieldOutline,
                    action: { onNavigate(.receive) }
                )
            }
            .padding(.bottom, 12)
        }
        .padding(.horizontal, 24)
        .padding(.top, 16)
    }
}

private struct HomeCta: View {
    let label: String
    let systemImage: String
    let background: Color
    let contentColor: Color
    let outline: Color?
    let action: () -> Void

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 16)
        Button(action: action) {
            HStack(spacing: 12) {
                Text(label.uppercased())
                    .font(ZinqqFont.display(20, weight: .bold))
                    .kerning(1)
                Image(systemName: systemImage)
                    .font(.system(size: 22, weight: .semibold))
            }
            .foregroundColor(contentColor)
            .frame(maxWidth: .infinity)
            .frame(height: 88)
            .background(background)
            .overlay {
                if let outline {
                    shape.strokeBorder(outline, lineWidth: 2)
                }
            }
            .clipShape(shape)
        }
        .accessibilityLabel(label)
    }
}
