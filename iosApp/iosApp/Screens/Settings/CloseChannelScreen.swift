import Shared
import SwiftUI

/// The PWA CloseChannel's step machine (`CloseChannel.tsx:27-30`).
private enum CloseStep: Equatable {
    case confirm(channel: ChannelView, force: Bool)
    case success(force: Bool)
    case error(failure: CloseFailureUi, channel: ChannelView)
}

/// The PWA's CloseChannel (U22, R10 UI; `CloseChannel.tsx`): the confirm
/// screen with the Cooperative / Force Close toggle, the informational
/// estimate that never blocks closing (nullable-safe placeholders), the
/// non-anchor and in-flight warnings, the method-colored CTA, the success
/// screen with "Track Progress" into the close detail, and the coop-failure
/// "Force Close Instead" escalation. Mirrors Android's `CloseChannelScreen`.
struct CloseChannelScreen: View {
    let port: any SettingsPort
    let channelId: String?
    let initialForce: Bool
    let onBack: () -> Void
    let onTrackProgress: (String) -> Void
    let onDone: () -> Void
    let onMissingChannel: () -> Void

    @Environment(\.zinqqColors) private var colors
    @State private var step: CloseStep?
    @State private var isClosing = false
    @State private var estimate: CloseEstimate?
    @State private var estimateLoading = true
    @State private var lookupFailed = false

    var body: some View {
        // Guard: no channel in route state → back to Peers (CloseChannel.tsx:57-62).
        if channelId == nil || lookupFailed {
            Color.clear.onAppear { onMissingChannel() }
        } else {
            content
        }
    }

    @ViewBuilder
    private var content: some View {
        Group {
            switch step {
            case nil:
                VStack(spacing: 0) {
                    ScreenHeader(title: "Close Channel", onBack: onBack, tint: colors.onDark)
                    CenteredSettingsNote("Loading...")
                }
                .background(colors.dark.ignoresSafeArea())
            case let .confirm(channel, force):
                confirmBody(channel: channel, force: force)
            case let .success(force):
                successBody(force: force)
            case let .error(failure, channel):
                errorBody(failure: failure, channel: channel)
            }
        }
        // Channel lookup + best-effort estimate (informational only — a
        // failure leaves placeholders; closing is never blocked,
        // CloseChannel.tsx:49-54).
        .task(id: channelId) {
            guard let channelId else { return }
            let match = (try? await port.listChannels())?
                .first { $0.channelId == channelId }
            guard let match else {
                lookupFailed = true
                return
            }
            step = .confirm(channel: match, force: initialForce)
            estimate = try? await port.estimateClose(channelId: channelId)
            estimateLoading = false
        }
    }

    private func confirm(channel: ChannelView, force: Bool) {
        guard !isClosing else { return }
        isClosing = true
        Task {
            do {
                try await port.closeChannel(channelId: channel.channelId, force: force)
                step = .success(force: force)
            } catch {
                step = .error(
                    failure: closeFailure(error, force: force),
                    channel: channel
                )
            }
            isClosing = false
        }
    }

    private func confirmBody(channel: ChannelView, force: Bool) -> some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Close Channel", onBack: onBack, tint: colors.onDark)
            ScrollView {
                VStack(spacing: 16) {
                    SettingsFactRow(
                        label: "Peer",
                        value: reviewPeerDisplay(channel.counterpartyPubkey),
                        mono: true
                    )
                    SettingsFactRow(
                        label: "Channel Capacity",
                        value: FormatKt.formatBtc(sats: Int64(bitPattern: channel.capacitySats))
                    )
                    SettingsFactRow(
                        label: "Your Balance",
                        value: FormatKt.formatBtc(
                            sats: FormatKt.msatToSatFloor(
                                msat: Int64(bitPattern: channel.outboundMsat)
                            )
                        )
                    )
                    SettingsFactRow(
                        label: "Remote Balance",
                        value: FormatKt.formatBtc(
                            sats: FormatKt.msatToSatFloor(
                                msat: Int64(bitPattern: channel.inboundMsat)
                            )
                        )
                    )

                    Divider().overlay(colors.darkBorder)

                    SettingsFactRow(
                        label: "You Get Back",
                        value: expectedBackLabel(estimate, loading: estimateLoading)
                    )
                    VStack(alignment: .leading, spacing: 4) {
                        SettingsFactRow(
                            label: "Estimated Cost to You",
                            value: closeCostLabel(estimate, force: force, loading: estimateLoading)
                        )
                        if lspPaysCloseFee(estimate, force: force) {
                            note(lspPaysNote)
                        }
                        note(estimateCaveat)
                    }
                    SettingsFactRow(
                        label: "Funds Available",
                        value: closeTimelineLabel(estimate, force: force)
                    )

                    Divider().overlay(colors.darkBorder)

                    // Close-method toggle (CloseChannel.tsx:357-384).
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Close Method")
                            .font(ZinqqFont.sans(14, weight: .medium))
                            .foregroundColor(colors.onDarkMuted)
                        HStack(spacing: 8) {
                            methodButton(
                                label: "Cooperative",
                                selected: !force,
                                selectedBackground: colors.cta,
                                selectedContent: colors.onCta,
                                action: { step = .confirm(channel: channel, force: false) }
                            )
                            methodButton(
                                label: "Force Close",
                                selected: force,
                                selectedBackground: colors.danger.opacity(0.15),
                                selectedContent: colors.danger,
                                action: { step = .confirm(channel: channel, force: true) }
                            )
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)

                    // Info / warning boxes (CloseChannel.tsx:386-413).
                    if force {
                        noteBox(
                            forceCloseInfoText(estimate),
                            background: colors.danger.opacity(0.1),
                            contentColor: colors.danger
                        )
                    } else {
                        noteBox(
                            coopCloseInfo,
                            background: colors.darkElevated,
                            contentColor: colors.onDarkMuted
                        )
                    }
                    if showsNonAnchorWarning(estimate, force: force) {
                        noteBox(
                            nonAnchorWarning,
                            background: colors.warning.opacity(0.1),
                            contentColor: colors.warning
                        )
                    }
                    if let warning = pendingHtlcWarning(estimate) {
                        noteBox(
                            warning,
                            background: colors.warning.opacity(0.1),
                            contentColor: colors.warning
                        )
                    }
                }
                .padding(.horizontal, 24)
                .padding(.top, 16)
            }
            SettingsCta(
                label: closeCtaLabel(force: force, closing: isClosing),
                background: force ? colors.dangerStrong : colors.hot,
                contentColor: force ? .white : colors.onHot,
                action: { confirm(channel: channel, force: force) },
                enabled: !isClosing,
                disabledAlpha: 0.3
            )
            .padding(.horizontal, 24)
            .padding(.vertical, 24)
        }
        .background(colors.dark.ignoresSafeArea())
    }

    private func successBody(force: Bool) -> some View {
        VStack(spacing: 0) {
            Spacer()
            ZStack {
                Circle()
                    .fill(colors.badge)
                    .frame(width: 80, height: 80)
                Image(systemName: "checkmark")
                    .font(.system(size: 36, weight: .semibold))
                    .foregroundColor(colors.onBadge)
            }
            .accessibilityLabel("Success")
            Text("Channel Closing")
                .font(ZinqqFont.display(24, weight: .bold))
                .foregroundColor(colors.onDark)
                .padding(.top, 24)
            Text(closeSuccessDetail(force: force, estimate: estimate))
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .multilineTextAlignment(.center)
                .padding(.top, 8)
            SettingsCta(
                label: "Track Progress",
                background: colors.cta,
                contentColor: colors.onCta,
                action: { if let channelId { onTrackProgress(channelId) } }
            )
            .padding(.top, 32)
            SettingsOutlineCta(
                label: "Done",
                borderColor: colors.darkBorder,
                contentColor: colors.onDark,
                action: onDone
            )
            .padding(.top, 12)
            Spacer()
        }
        .padding(.horizontal, 32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }

    private func errorBody(failure: CloseFailureUi, channel: ChannelView) -> some View {
        VStack(spacing: 0) {
            Spacer()
            ZStack {
                Circle()
                    .fill(colors.danger.opacity(0.15))
                    .frame(width: 80, height: 80)
                Image(systemName: "xmark")
                    .font(.system(size: 36, weight: .semibold))
                    .foregroundColor(colors.danger)
            }
            .accessibilityLabel("Failure")
            Text("Close Failed")
                .font(ZinqqFont.display(24, weight: .bold))
                .foregroundColor(colors.onDark)
                .padding(.top, 24)
            Text(failure.message)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.danger)
                .multilineTextAlignment(.center)
                .padding(.top, 8)
            Text("Your funds are safe.")
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .padding(.top, 4)
            if failure.canForceClose {
                SettingsOutlineCta(
                    label: "Force Close Instead",
                    borderColor: colors.danger,
                    contentColor: colors.danger,
                    action: { step = .confirm(channel: channel, force: true) }
                )
                .padding(.top, 32)
            }
            SettingsCta(
                label: "Try Again",
                background: colors.cta,
                contentColor: colors.onCta,
                action: { step = .confirm(channel: channel, force: false) }
            )
            .padding(.top, failure.canForceClose ? 12 : 32)
            Spacer()
        }
        .padding(.horizontal, 32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }

    private func methodButton(
        label: String,
        selected: Bool,
        selectedBackground: Color,
        selectedContent: Color,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            Text(label)
                .font(ZinqqFont.sans(14, weight: .semibold))
                .foregroundColor(selected ? selectedContent : colors.onDarkMuted)
                .frame(maxWidth: .infinity)
                .frame(height: 44)
                .background(selected ? selectedBackground : colors.darkElevated)
                .clipShape(RoundedRectangle(cornerRadius: 8))
        }
        .accessibilityLabel(label)
        .accessibilityAddTraits(selected ? [.isSelected] : [])
    }

    private func note(_ text: String) -> some View {
        Text(text)
            .font(ZinqqFont.sans(12))
            .foregroundColor(colors.onDarkMuted)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func noteBox(_ text: String, background: Color, contentColor: Color) -> some View {
        Text(text)
            .font(ZinqqFont.sans(14))
            .foregroundColor(contentColor)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
            .background(background)
            .clipShape(RoundedRectangle(cornerRadius: 8))
    }
}
