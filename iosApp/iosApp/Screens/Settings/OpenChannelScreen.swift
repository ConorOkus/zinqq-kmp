import Shared
import SwiftUI

/// The PWA OpenChannel's step machine (`OpenChannel.tsx:22-27`).
private enum OpenStep: Equatable {
    case amount
    case reviewing(amountSats: UInt64, fee: OpenFeeEstimate)
    case opening
    case success
    case failed(message: String)
}

/// The PWA's OpenChannel (U22, R10 UI; `OpenChannel.tsx`): numpad amount step
/// with the 20,000–16,777,215 bounds and balance gate, the Peer / Channel
/// Size / Est. fee / Total review, "Connect & Open Channel" (the core's
/// `openChannel` connects if needed), and the Channel Opening / failure
/// results. `peerAddress` arrives from the Peers connect form like the PWA's
/// `location.state`; missing state redirects back to Peers. Mirrors Android's
/// `OpenChannelScreen`.
struct OpenChannelScreen: View {
    let port: any SettingsPort
    let peerAddress: String?
    let onBack: () -> Void
    let onDone: () -> Void
    let onMissingPeer: () -> Void

    @Environment(\.zinqqColors) private var colors
    @State private var step: OpenStep = .amount
    @State private var digits = ""
    @State private var amountError: String?
    @State private var fee: OpenFeeEstimate?

    private var validPeerAddress: String? {
        guard let peerAddress, case .valid = parsePeerAddress(peerAddress) else { return nil }
        return peerAddress
    }

    var body: some View {
        // Guard: no peer in route state → back to Peers (OpenChannel.tsx:57-62).
        if let peerAddress = validPeerAddress {
            content(peerAddress: peerAddress)
        } else {
            Color.clear.onAppear { onMissingPeer() }
        }
    }

    @ViewBuilder
    private func content(peerAddress: String) -> some View {
        let peerPubkey = String(peerAddress.prefix(while: { $0 != "@" }))
        let amountSats = UInt64(digits) ?? 0
        let balanceSats = port.onchainBalanceSats()

        Group {
            switch step {
            case .amount:
                amountBody(amountSats: amountSats, balanceSats: balanceSats)
            case let .reviewing(amountSats, fee):
                reviewBody(peerAddress: peerAddress, peerPubkey: peerPubkey,
                           amountSats: amountSats, fee: fee)
            case .opening:
                openingBody
            case .success:
                ResultTemplate(
                    success: true,
                    headline: "Channel Opening",
                    onCta: onDone,
                    detail: "Your channel is being set up. It will be ready once the funding "
                        + "transaction confirms on-chain.",
                    fundsAreSafe: false,
                    ctaLabel: "Done"
                )
            case let .failed(message):
                ResultTemplate(
                    success: false,
                    headline: "Channel Open Failed",
                    onCta: {
                        digits = ""
                        amountError = nil
                        step = .amount
                    },
                    detail: message,
                    ctaLabel: "Try Again"
                )
            }
        }
        // Fee estimate on entry; failures fall back like the PWA's getFeeRate
        // catch (1 sat/vB × 140 vB).
        .task {
            do {
                fee = try await port.estimateOpenFee()
            } catch {
                fee = fallbackOpenFee()
            }
        }
    }

    private func amountBody(amountSats: UInt64, balanceSats: UInt64) -> some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Channel Size", onBack: onBack, tint: colors.onDark)
            VStack(spacing: 8) {
                Text("\(FormatKt.formatBtc(sats: Int64(bitPattern: balanceSats))) available")
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.onDarkMuted)
                Text(FormatKt.formatBtc(sats: Int64(bitPattern: amountSats)))
                    .font(ZinqqFont.display(digits.count > 5 ? 48 : 72, weight: .bold))
                    .foregroundColor(colors.amount)
                if let amountError {
                    Text(amountError)
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.danger)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            Numpad(
                onKey: { key in
                    amountError = nil
                    digits = NumpadReducer.reduce(digits, key, maxDigits: openAmountMaxDigits)
                },
                onNext: {
                    let estimate = fee ?? fallbackOpenFee()
                    if let error = validateOpenAmount(
                        amountSats: amountSats,
                        estimatedFeeSats: estimate.estimatedFeeSats,
                        balanceSats: balanceSats
                    ) {
                        amountError = error
                    } else {
                        step = .reviewing(amountSats: amountSats, fee: estimate)
                    }
                },
                nextEnabled: amountSats > 0
            )
        }
        .background(colors.dark.ignoresSafeArea())
    }

    private func reviewBody(
        peerAddress: String,
        peerPubkey: String,
        amountSats: UInt64,
        fee: OpenFeeEstimate
    ) -> some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Review", onBack: { step = .amount }, tint: colors.onDark)
            VStack(spacing: 24) {
                SettingsFactRow(label: "Peer", value: reviewPeerDisplay(peerPubkey), mono: true)
                SettingsFactRow(
                    label: "Channel Size",
                    value: FormatKt.formatBtc(sats: Int64(bitPattern: amountSats))
                )
                SettingsFactRow(
                    label: openFeeRateLabel(fee.feeRateSatPerVb),
                    value: "≈ \(FormatKt.formatBtc(sats: Int64(bitPattern: fee.estimatedFeeSats)))"
                )
                Divider().overlay(colors.darkBorder)
                HStack {
                    Text("Total")
                        .font(ZinqqFont.sans(18, weight: .semibold))
                        .foregroundColor(colors.onDark)
                    Spacer()
                    Text(
                        "≈ " + FormatKt.formatBtc(
                            sats: Int64(bitPattern: openTotalSats(
                                amountSats: amountSats,
                                estimatedFeeSats: fee.estimatedFeeSats
                            ))
                        )
                    )
                    .font(ZinqqFont.display(30, weight: .bold))
                    .foregroundColor(colors.onDark)
                }
            }
            .padding(.horizontal, 24)
            .padding(.top, 32)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            SettingsCta(
                label: "Connect & Open Channel",
                background: colors.cta,
                contentColor: colors.onCta,
                action: {
                    step = .opening
                    Task {
                        do {
                            _ = try await port.openChannel(
                                peerAddress: peerAddress, amountSats: amountSats
                            )
                            step = .success
                        } catch {
                            step = .failed(message: openChannelErrorMessage(error))
                        }
                    }
                }
            )
            .padding(.horizontal, 24)
            .padding(.vertical, 24)
        }
        .background(colors.dark.ignoresSafeArea())
    }

    private var openingBody: some View {
        VStack(spacing: 16) {
            ProgressView()
                .controlSize(.large)
                .tint(colors.onDark)
            Text("Connecting to peer & opening channel...")
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .multilineTextAlignment(.center)
        }
        .padding(.horizontal, 32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }
}
