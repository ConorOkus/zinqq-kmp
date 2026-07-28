import Shared
import SwiftUI

/// The PWA's Peers (U22, R10 UI; `Peers.tsx`): the connect input — parsed
/// client-side like the PWA, then handed to OpenChannel where the core's
/// `openChannel` connects if needed — the `(N connected, M saved)` list with
/// Refresh, per-peer status dots and channel-guarded Forget, and the nested
/// channel rows with Close / Force Close actions. Mirrors Android's
/// `PeersScreen`.
struct PeersScreen: View {
    let port: any SettingsPort
    var onBack: (() -> Void)?
    let onOpenChannel: (String) -> Void
    let onCloseChannel: (_ channelId: String, _ force: Bool) -> Void

    @Environment(\.zinqqColors) private var colors
    @State private var peerAddress = ""
    @State private var connectError: String?
    @State private var peers: [PeerView]?
    @State private var channels: [String: [ChannelView]] = [:]
    @State private var loadError: String?
    @State private var forgetError: String?

    var body: some View {
        SettingsScaffold(title: "Peers", onBack: onBack) {
            if peers == nil && loadError == nil {
                CenteredSettingsNote("Loading Lightning node...")
            } else if let loadError, peers == nil {
                loadErrorBody(loadError)
            } else {
                content
            }
        }
        .task { await refresh() }
    }

    private func refresh() async {
        do {
            let fetchedPeers = try await port.listPeers()
            channels = channelsByPeer(try await port.listChannels())
            peers = fetchedPeers
            loadError = nil
        } catch {
            loadError = kotlinThrowable(error)?.message
                ?? (error as NSError).localizedDescription
        }
    }

    private func connect() {
        connectError = nil
        let trimmed = peerAddress.trimmingCharacters(in: .whitespacesAndNewlines)
        switch parsePeerAddress(trimmed) {
        case let .invalid(message):
            connectError = message
        case .valid:
            onOpenChannel(trimmed)
        }
    }

    private func loadErrorBody(_ message: String) -> some View {
        VStack(spacing: 8) {
            Text("Lightning node error")
                .font(ZinqqFont.sans(16, weight: .semibold))
                .foregroundColor(colors.onDark)
            Text(message)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.danger)
                .multilineTextAlignment(.center)
        }
        .padding(.horizontal, 24)
        .padding(.top, 64)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
    }

    private var content: some View {
        ScrollView {
            VStack(spacing: 20) {
                connectForm
                peerList
            }
            .padding(.horizontal, 24)
            .padding(.top, 8)
            .padding(.bottom, 32)
        }
    }

    /// Connect form (Peers.tsx:167-193).
    private var connectForm: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Connect & Open Channel")
                .font(ZinqqFont.sans(14, weight: .medium))
                .foregroundColor(colors.onDarkMuted)
            TextField(
                "",
                text: $peerAddress,
                prompt: Text("pubkey@host:port")
                    .font(.system(size: 14, design: .monospaced))
                    .foregroundColor(colors.onDarkMuted)
            )
            .font(.system(size: 14, design: .monospaced))
            .foregroundColor(colors.onDark)
            .tint(colors.hot)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled(true)
            .keyboardType(.asciiCapable)
            .padding(.horizontal, 16)
            .padding(.vertical, 14)
            .background(colors.darkElevated)
            .clipShape(RoundedRectangle(cornerRadius: 12))
            .accessibilityLabel("Peer address")
            if let connectError {
                Text(connectError)
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.danger)
            }
            SettingsCta(
                label: "Next",
                background: colors.cta,
                contentColor: colors.onCta,
                action: connect,
                enabled: !peerAddress.trimmingCharacters(in: .whitespaces).isEmpty,
                disabledAlpha: 0.3
            )
        }
    }

    /// Peer list (Peers.tsx:196-310).
    private var peerList: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(peersCountLabel(peers ?? []))
                    .font(ZinqqFont.sans(14, weight: .medium))
                    .foregroundColor(colors.onDarkMuted)
                Spacer()
                Button {
                    Task { await refresh() }
                } label: {
                    Text("Refresh")
                        .font(ZinqqFont.sans(12))
                        .underline()
                        .foregroundColor(colors.onDark)
                        .padding(8)
                }
                .accessibilityLabel("Refresh")
            }
            if let forgetError {
                Text(forgetError)
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.danger)
            }
            if let peers, !peers.isEmpty {
                VStack(spacing: 8) {
                    ForEach(peers, id: \.pubkey) { peer in
                        PeerCard(
                            peer: peer,
                            channels: channels[peer.pubkey] ?? [],
                            onForget: {
                                forgetError = nil
                                Task {
                                    do {
                                        try await port.forgetPeer(pubkey: peer.pubkey)
                                        await refresh()
                                    } catch {
                                        forgetError = forgetErrorMessage(error)
                                    }
                                }
                            },
                            onCloseChannel: onCloseChannel
                        )
                    }
                }
            } else {
                Text("No peers connected")
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.onDarkMuted)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 16)
            }
        }
    }
}

private struct PeerCard: View {
    let peer: PeerView
    let channels: [ChannelView]
    let onForget: () -> Void
    let onCloseChannel: (_ channelId: String, _ force: Bool) -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 12) {
                Circle()
                    .fill(peer.connected ? colors.success : colors.onDarkMuted)
                    .frame(width: 10, height: 10)
                Text(peerDisplayId(peer.pubkey))
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundColor(colors.onDark)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer()
                Text(peerStatusLabel(connected: peer.connected))
                    .font(ZinqqFont.sans(12, weight: .semibold))
                    .foregroundColor(peer.connected ? colors.success : colors.onDarkMuted)
                if showsForget(peer) {
                    let enabled = forgetEnabled(peer)
                    Button(action: onForget) {
                        Text("Forget")
                            .font(ZinqqFont.sans(12))
                            .foregroundColor(colors.danger)
                            .padding(4)
                            .opacity(enabled ? 1 : 0.3)
                    }
                    .disabled(!enabled)
                    .accessibilityLabel("Forget peer")
                }
            }
            ForEach(channels, id: \.channelId) { channel in
                ChannelRow(channel: channel, onCloseChannel: onCloseChannel)
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(colors.darkElevated)
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }
}

private struct ChannelRow: View {
    let channel: ChannelView
    let onCloseChannel: (_ channelId: String, _ force: Bool) -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        let closing = channel.state == .closing
        // `ml-5 border-l pl-3` (Peers.tsx:253): indented with a left rule.
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(channelStateText(channel.state))
                    .font(ZinqqFont.sans(12, weight: closing ? .semibold : .regular))
                    .foregroundColor(closing ? colors.warning : colors.onDarkMuted)
                Spacer()
                Text(channelCapacityText(channel))
                    .font(ZinqqFont.sans(12, weight: .semibold))
                    .foregroundColor(colors.onDark)
            }
            if closing {
                Text(closingInProgressNote)
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onDarkMuted)
            }
            HStack(spacing: 12) {
                Text(channelSendText(channel))
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onDarkMuted)
                Text(channelReceiveText(channel))
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onDarkMuted)
                if let reserve = channelReserveText(channel) {
                    Text(reserve)
                        .font(ZinqqFont.sans(12))
                        .foregroundColor(colors.onDarkMuted)
                }
                Spacer()
                Button {
                    onCloseChannel(channel.channelId, closing)
                } label: {
                    Text(channelCloseActionLabel(channel.state))
                        .font(ZinqqFont.sans(12, weight: .semibold))
                        .foregroundColor(colors.danger)
                        .padding(4)
                }
                .accessibilityLabel(channelCloseActionLabel(channel.state))
            }
        }
        .padding(.leading, 12)
        .overlay(alignment: .leading) {
            Rectangle()
                .fill(colors.darkBorder)
                .frame(width: 1)
        }
        .padding(.leading, 20)
    }
}
