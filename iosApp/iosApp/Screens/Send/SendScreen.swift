import Shared
import SwiftUI
import UIKit

/// The PWA's hard-coded explorer base for oc-success (`onchain/config`).
private let explorerSendTxBase = "https://mempool.space/tx"

/// The Send screen (U20, F1, R5/R7 UI): the PWA's six-step machine rendered
/// from `SendStep` — layout mirroring Android's `SendScreen.kt`. All
/// protocol decisions arrive as core results through `SendController`; this
/// view only places them (R14).
///
/// `scannedInput` is the Scan screen's raw decode (R13) — it runs the exact
/// same classify path as typed/pasted input.
struct SendScreen: View {
    let port: any SendPort
    let scannedInput: String?
    let onDone: () -> Void
    let onBackToHome: () -> Void

    @StateObject private var controller: SendController
    @State private var inputValue = ""

    @Environment(\.zinqqColors) private var colors

    init(
        port: any SendPort,
        scannedInput: String?,
        onDone: @escaping () -> Void,
        onBackToHome: @escaping () -> Void
    ) {
        self.port = port
        self.scannedInput = scannedInput
        self.onDone = onDone
        self.onBackToHome = onBackToHome
        _controller = StateObject(wrappedValue: SendController(port: port))
    }

    var body: some View {
        Group {
            switch controller.step {
            case let .input(current):
                InputStepScreen(
                    value: $inputValue,
                    error: current.error,
                    resolving: current.resolving,
                    onNext: { controller.submitInput(inputValue) },
                    onAbortResolve: { controller.abortResolve() },
                    onPaste: { pasted in
                        inputValue = String(pasted.prefix(sendInputMaxLength))
                        controller.submitInput(pasted)
                    },
                    onBack: onBackToHome
                )

            case let .amount(current):
                AmountStepScreen(
                    step: current,
                    onchainBalanceSats: port.onchainBalanceSats(),
                    unifiedTotal: unifiedTotalSats(
                        onchainBalanceSats: port.onchainBalanceSats(),
                        lightningMsat: port.lightningCapacityMsat()
                    ),
                    onKey: { controller.onNumpadKey($0) },
                    onNext: { controller.submitAmountStep() },
                    onSendMax: { controller.setOnchainSendMax() },
                    onLnAvailable: { controller.setLightningAvailable() },
                    onBack: { controller.backToInput() }
                )

            case let .reviewLightning(current):
                LightningReviewScreen(
                    step: current,
                    onConfirm: { controller.confirmLightning() },
                    onBack: { controller.backFromReview() }
                )

            case let .reviewOnchain(current):
                OnchainReviewScreen(
                    step: current,
                    onConfirm: { controller.confirmOnchain() },
                    onBack: { controller.backFromReview() }
                )

            case let .dispatching(amountMsat):
                DispatchingScreen(amountMsat: amountMsat)

            case let .success(amountSats, txid):
                ResultTemplate(
                    success: true,
                    headline: FormatKt.formatBtc(sats: Int64(bitPattern: amountSats)),
                    onCta: onDone,
                    detail: "sent successfully",
                    fundsAreSafe: false,
                    ctaLabel: "Done"
                ) {
                    if let txid, let url = URL(string: "\(explorerSendTxBase)/\(txid)") {
                        // PWA oc-success "View on explorer" (Send.tsx:871-884).
                        Link("View on explorer", destination: url)
                            .font(ZinqqFont.sans(14))
                            .foregroundColor(colors.onDark)
                            .padding(.horizontal, 24)
                            .padding(.vertical, 12)
                    }
                }

            case let .failure(message, retry):
                ResultTemplate(
                    success: false,
                    headline: "Send Failed",
                    onCta: {
                        if let retry { controller.retry(retry) } else { onDone() }
                    },
                    detail: message,
                    ctaLabel: retry != nil ? "Try Again" : "Done"
                )

            case .timedOut:
                ResultTemplate(
                    success: false,
                    headline: "Payment is taking longer than expected",
                    onCta: onDone,
                    detail: "It may still complete — check Activity for the final status.",
                    fundsAreSafe: false,
                    ctaLabel: "Done"
                )
            }
        }
        .task {
            // The scanned raw string travels like the PWA's location.state
            // (Scan.tsx:60 / Send.tsx:608-620): consumed once on entry and
            // re-classified from scratch — never a parsed object (R13/R14).
            if let scannedInput, !scannedInput.isEmpty {
                inputValue = String(scannedInput.prefix(sendInputMaxLength))
                controller.submitInput(scannedInput)
            }
        }
    }
}

// MARK: - Recipient step (PWA Send.tsx:1146-1202)

private struct InputStepScreen: View {
    @Binding var value: String
    let error: String?
    let resolving: Bool
    let onNext: () -> Void
    let onAbortResolve: () -> Void
    let onPaste: (String) -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Send", onBack: onBack)
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    Text("Recipient")
                        .font(ZinqqFont.sans(14, weight: .medium))
                        .foregroundColor(colors.onDarkMuted)
                    Spacer()
                    Button {
                        if let pasted = UIPasteboard.general.string,
                           !pasted.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                            onPaste(pasted)
                        }
                    } label: {
                        Text("Paste")
                            .font(ZinqqFont.sans(14, weight: .medium))
                            .foregroundColor(colors.onDark)
                            .padding(.horizontal, 12)
                            .padding(.vertical, 8)
                    }
                    .disabled(resolving)
                    .accessibilityLabel("Paste from clipboard")
                }
                ZStack(alignment: .topLeading) {
                    if value.isEmpty {
                        Text("payment request or user@domain")
                            .font(.system(size: 14, design: .monospaced))
                            .foregroundColor(colors.onDarkMuted)
                    }
                    TextField("", text: $value, axis: .vertical)
                        .font(.system(size: 14, design: .monospaced))
                        .foregroundColor(colors.onDark)
                        .tint(colors.hot)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .disabled(resolving)
                        .onChange(of: value) { newValue in
                            if newValue.count > sendInputMaxLength {
                                value = String(newValue.prefix(sendInputMaxLength))
                            }
                        }
                        .accessibilityLabel("Payment request or address")
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(colors.darkElevated)
                .clipShape(RoundedRectangle(cornerRadius: 12))
                .padding(.top, 8)
                if let error, !error.isEmpty {
                    Text(error)
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.danger)
                        .padding(.top, 8)
                }
                Spacer()
            }
            .padding(.horizontal, 24)
            .padding(.top, 24)
            CtaButton(
                label: resolving ? "Resolving..." : "Next",
                enabled: !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    || resolving,
                showSpinner: resolving,
                action: resolving ? onAbortResolve : onNext
            )
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }
}

// MARK: - Amount step (PWA Send.tsx:1095-1144)

private struct AmountStepScreen: View {
    let step: SendAmountStep
    let onchainBalanceSats: UInt64
    let unifiedTotal: UInt64
    let onKey: (NumpadInput) -> Void
    let onNext: () -> Void
    let onSendMax: () -> Void
    let onLnAvailable: () -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    private var isOnchain: Bool { step.target.kind == .onchain }

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Send", onBack: onBack)
            VStack(spacing: 0) {
                Spacer()
                if isOnchain {
                    // "₿X available · Max" pill (Send.tsx:1103-1115).
                    Button(action: onSendMax) {
                        Text(
                            "\(FormatKt.formatBtc(sats: Int64(bitPattern: onchainBalanceSats)))"
                                + " available · Max"
                        )
                        .font(ZinqqFont.sans(14, weight: step.isSendMax ? .semibold : .regular))
                        .foregroundColor(step.isSendMax ? colors.onPill : colors.onDarkMuted)
                        .padding(.horizontal, 16)
                        .padding(.vertical, 6)
                        .background(step.isSendMax ? colors.pill : colors.dark)
                        .clipShape(Capsule())
                    }
                    .accessibilityLabel("Send maximum")
                } else {
                    // "₿X available" (Send.tsx:1116-1123).
                    Button(action: onLnAvailable) {
                        Text("\(FormatKt.formatBtc(sats: Int64(bitPattern: unifiedTotal))) available")
                            .font(ZinqqFont.sans(14))
                            .foregroundColor(colors.onDarkMuted)
                            .padding(.horizontal, 16)
                            .padding(.vertical, 6)
                    }
                }
                Text(FormatKt.formatBtc(sats: Int64(bitPattern: step.amountSats)))
                    .font(ZinqqFont.display(step.digits.count > 5 ? 48 : 72, weight: .bold))
                    .foregroundColor(colors.amount)
                    .padding(.top, 8)
                if step.minSats != nil || step.maxSats != nil {
                    // "Min ₿X · Max ₿X" (Send.tsx:1132-1138).
                    Text(boundsLine)
                        .font(ZinqqFont.sans(12))
                        .foregroundColor(colors.onDarkMuted)
                        .padding(.top, 4)
                }
                if let error = step.error, !error.isEmpty {
                    Text(error)
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.danger)
                        .multilineTextAlignment(.center)
                        .padding(.top, 8)
                        .padding(.horizontal, 24)
                }
                if step.fetchingInvoice {
                    ProgressView()
                        .tint(colors.onDarkMuted)
                        .padding(.top, 12)
                }
                Spacer()
            }
            .frame(maxWidth: .infinity)
            Numpad(
                onKey: onKey,
                onNext: onNext,
                nextEnabled: step.amountSats > 0 && !step.fetchingInvoice
            )
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }

    private var boundsLine: String {
        var parts: [String] = []
        if let min = step.minSats {
            parts.append("Min \(FormatKt.formatBtc(sats: Int64(bitPattern: min)))")
        }
        if let max = step.maxSats {
            parts.append("Max \(FormatKt.formatBtc(sats: Int64(bitPattern: max)))")
        }
        return parts.joined(separator: " · ")
    }
}

// MARK: - Lightning review (PWA Send.tsx:1063-1093)

private struct LightningReviewScreen: View {
    let step: SendLightningReview
    let onConfirm: () -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Review", onBack: onBack)
            VStack(spacing: 24) {
                ReviewRow(label: "To") {
                    Text(step.recipient)
                        .font(ZinqqFont.sans(14, weight: .semibold))
                        .foregroundColor(colors.onDark)
                        .multilineTextAlignment(.trailing)
                }
                ReviewRow(label: "Amount") {
                    Text(
                        FormatKt.formatBtc(
                            sats: FormatKt.msatToSatCeil(msat: Int64(bitPattern: step.amountMsat))
                        )
                    )
                    .font(ZinqqFont.display(30, weight: .bold))
                    .foregroundColor(colors.onDark)
                }
                Spacer()
            }
            .padding(.horizontal, 24)
            .padding(.top, 32)
            CtaButton(label: "Confirm Send", enabled: true, action: onConfirm)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }
}

// MARK: - On-chain review (PWA Send.tsx:988-1061)

private struct OnchainReviewScreen: View {
    let step: SendOnchainReview
    let onConfirm: () -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader(title: "Review", onBack: onBack)
            VStack(alignment: .leading, spacing: 24) {
                if step.amountsUpdated {
                    // R5 drift banner (Send.tsx:995-1002), verbatim.
                    Text("Amounts were updated — conditions changed since your last review.")
                        .font(ZinqqFont.sans(14, weight: .medium))
                        .foregroundColor(colors.warning)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(colors.warning.opacity(0.1))
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                }
                if step.isSendMax {
                    Text("Sending all available onchain funds")
                        .font(ZinqqFont.sans(14, weight: .medium))
                        .foregroundColor(colors.onDarkMuted)
                }
                ReviewRow(label: "To") {
                    Text(onchainRecipientLabel(step.address))
                        .font(.system(size: 14, weight: .semibold, design: .monospaced))
                        .foregroundColor(colors.onDark)
                        .multilineTextAlignment(.trailing)
                }
                ReviewRow(label: "Amount") {
                    Text(FormatKt.formatBtc(sats: Int64(bitPattern: step.amountSats)))
                        .font(ZinqqFont.sans(16, weight: .semibold))
                        .foregroundColor(colors.onDark)
                }
                VStack(alignment: .leading, spacing: 4) {
                    ReviewRow(label: "Network fee (\(step.feeRateSatPerVb) sat/vB)") {
                        Text(FormatKt.formatBtc(sats: Int64(bitPattern: step.feeSats)))
                            .font(ZinqqFont.sans(16, weight: .semibold))
                            .foregroundColor(colors.onDark)
                    }
                    if step.isSendMax && step.reserveSats > 0 {
                        Text("Final fee may vary slightly")
                            .font(ZinqqFont.sans(12))
                            .foregroundColor(colors.onDarkMuted)
                    }
                }
                if step.isSendMax && step.reserveSats > 0 {
                    ReviewRow(label: "Kept for Lightning channel safety") {
                        Text(FormatKt.formatBtc(sats: Int64(bitPattern: step.reserveSats)))
                            .font(ZinqqFont.sans(16, weight: .semibold))
                            .foregroundColor(colors.onDark)
                    }
                }
                Divider().background(colors.darkBorder)
                HStack {
                    Text("Total")
                        .font(ZinqqFont.sans(18, weight: .semibold))
                        .foregroundColor(colors.onDark)
                    Spacer()
                    Text(FormatKt.formatBtc(sats: Int64(bitPattern: step.totalSats)))
                        .font(ZinqqFont.display(30, weight: .bold))
                        .foregroundColor(colors.onDark)
                }
                Spacer()
            }
            .padding(.horizontal, 24)
            .padding(.top, 32)
            CtaButton(
                label: step.broadcasting ? "Sending…" : "Confirm Send",
                enabled: !step.broadcasting,
                showSpinner: step.broadcasting,
                action: onConfirm
            )
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }
}

// MARK: - Dispatching (PWA ln-sending, Send.tsx:946-986; no cancel: the core
// exposes no abandon FFI, so the flow waits for the outcome event)

private struct DispatchingScreen: View {
    let amountMsat: UInt64

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            ProgressView()
                .tint(colors.onDark)
                .scaleEffect(1.4)
            Text("Sending payment...")
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .padding(.top, 16)
            Text(
                FormatKt.formatBtc(
                    sats: FormatKt.msatToSatCeil(msat: Int64(bitPattern: amountMsat))
                )
            )
            .font(ZinqqFont.sans(12))
            .foregroundColor(colors.onDarkMuted)
            .padding(.top, 4)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
    }
}

// MARK: - Shared bits

private struct ReviewRow<Value: View>: View {
    let label: String
    @ViewBuilder var value: Value

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        HStack(alignment: .center) {
            Text(label)
                .font(ZinqqFont.sans(14, weight: .medium))
                .foregroundColor(colors.onDarkMuted)
                .padding(.trailing, 16)
            Spacer()
            value
        }
    }
}

private struct CtaButton: View {
    let label: String
    let enabled: Bool
    var showSpinner: Bool = false
    let action: () -> Void

    @Environment(\.zinqqColors) private var colors

    init(
        label: String,
        enabled: Bool,
        showSpinner: Bool = false,
        action: @escaping () -> Void
    ) {
        self.label = label
        self.enabled = enabled
        self.showSpinner = showSpinner
        self.action = action
    }

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                if showSpinner {
                    ProgressView()
                        .tint(colors.onCta)
                }
                Text(label.uppercased())
                    .font(ZinqqFont.display(18, weight: .bold))
                    .kerning(1)
                    .foregroundColor(colors.onCta)
            }
            .frame(maxWidth: .infinity)
            .frame(height: 56)
            .background(colors.cta)
            .clipShape(RoundedRectangle(cornerRadius: 12))
            .opacity(enabled ? 1 : 0.3)
        }
        .disabled(!enabled)
        .padding(.horizontal, 24)
        .padding(.vertical, 24)
        .accessibilityLabel(label)
    }
}
