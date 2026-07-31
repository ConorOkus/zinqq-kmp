import Shared
import SwiftUI
import UIKit

/// The Receive screen (U21, F2, R6 UI, R12): the PWA's `Receive.tsx` overlay
/// as a dedicated route — same machine, layout mirroring Android's
/// `ReceiveScreen.kt`. All liquidity decisions arrive as core results through
/// `ReceiveController`; this view only places them (R14). Platform-sanctioned
/// deviations (R12): TabView page style for the pager, ShareLink for the
/// system share sheet, TimelineView for the countdown, and the idle timer
/// disabled while a QR is displayed (spike behavior preserved).
struct ReceiveScreen: View {
    let onClose: () -> Void

    @StateObject private var controller: ReceiveController
    @State private var showSheet = false
    @State private var page: QrPage = .unified

    @Environment(\.zinqqColors) private var colors

    init(port: any ReceivePort, onClose: @escaping () -> Void) {
        self.onClose = onClose
        _controller = StateObject(wrappedValue: ReceiveController(port: port))
    }

    var body: some View {
        let state = controller.state
        ZStack {
            content(state)
            // Copy bottom sheet (PWA Receive.tsx:1026-1039); 2,000 ms feedback.
            if showSheet {
                CopySheet(
                    title: copySheetTitle(page: page),
                    value: copyValue(page: page, bip321Uri: state.bip321Uri, offer: state.offer, asyncOffer: state.asyncOffer),
                    onClose: { showSheet = false }
                )
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.dark.ignoresSafeArea())
        .task { controller.start() }
    }

    @ViewBuilder
    private func content(_ state: ReceiveUiState) -> some View {
        // Success screen (PWA Receive.tsx:604-637).
        if case let .received(amountSats) = state.step {
            VStack(spacing: 0) {
                ScreenHeader(title: "Request", onBack: onClose)
                ResultTemplate(
                    success: true,
                    headline: "Payment received",
                    onCta: onClose,
                    fundsAreSafe: false,
                    ctaLabel: "Done"
                ) {
                    Text(FormatKt.formatBtc(sats: Int64(bitPattern: amountSats)))
                        .font(ZinqqFont.display(34, weight: .bold))
                        .foregroundColor(colors.onDark)
                }
            }
        } else if state.loading {
            VStack(spacing: 0) {
                ScreenHeader(title: "Request", onBack: onClose)
                Spacer()
                ProgressView()
                    .tint(colors.onDark)
                    .scaleEffect(1.4)
                Spacer()
            }
        } else if let loadError = state.loadError {
            // PWA Receive.tsx:592-602: fatal entry failure.
            VStack(spacing: 0) {
                Spacer()
                Text("Failed to load wallet")
                    .font(ZinqqFont.sans(18, weight: .semibold))
                    .foregroundColor(colors.onDark)
                Text(loadError)
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.danger)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
                    .padding(.horizontal, 24)
                Button(action: onClose) {
                    Text("Close")
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.onDark)
                        .padding(.horizontal, 16)
                        .padding(.vertical, 8)
                }
                .padding(.top, 24)
                .accessibilityLabel("Close")
                Spacer()
            }
        } else {
            VStack(spacing: 0) {
                ScreenHeader(
                    title: "Request",
                    onBack: onClose
                ) {
                    if headerCopyVisible(
                        hasAddress: state.address != nil,
                        editingAmount: state.editingAmount,
                        step: state.step
                    ) {
                        Button {
                            showSheet = true
                        } label: {
                            Image(systemName: "doc.on.doc")
                                .font(.system(size: 18, weight: .semibold))
                                .foregroundColor(colors.onDark)
                                .frame(
                                    width: ZinqqDimens.minTouchTarget,
                                    height: ZinqqDimens.minTouchTarget
                                )
                        }
                        .accessibilityLabel("Copy payment request")
                    }
                }

                stepBody(state)
            }
        }
    }

    @ViewBuilder
    private func stepBody(_ state: ReceiveUiState) -> some View {
        switch state.step {
        case .quoting:
            QuotingSkeleton(amountSats: state.confirmedAmountSats)

        case .jitReview, .jitBelowMinimum:
            JitReviewScreen(
                step: state.step,
                onGenerate: { controller.generateInvoice() },
                onBack: { controller.backFromReview() }
            )

        case .buying:
            CenteredStatus(text: "Generating payment request…")

        case .jitExpired where showExpiredScreen(step: state.step, editingAmount: state.editingAmount):
            ExpiredScreen(
                onRetry: { controller.retryRequest() },
                onBack: { controller.backFromReview() }
            )

        case .jitError:
            JitErrorScreen(
                onRetry: { controller.retryRequest() },
                onBack: { controller.backFromReview() }
            )

        default:
            if state.editingAmount {
                AmountEntry(state: state, controller: controller)
            } else {
                QrDisplay(
                    state: state,
                    page: $page,
                    onEditAmount: { controller.editAmount() }
                )
            }
        }
    }
}

// MARK: - QR display (PWA Receive.tsx:931-1024)

private struct QrDisplay: View {
    let state: ReceiveUiState
    @Binding var page: QrPage
    let onEditAmount: () -> Void

    @Environment(\.zinqqColors) private var colors

    private var pages: [QrPage] {
        receivePages(
            offerExists: state.offerQrValue != nil,
            asyncOfferExists: state.asyncOfferQrValue != nil,
            needsAmount: state.needsAmount
        )
    }

    private var invoicePath: InvoicePath {
        if case let .display(invoicePath) = state.step { return invoicePath }
        return .none
    }

    var body: some View {
        VStack(spacing: 0) {
            VStack(spacing: 0) {
                Spacer()
                if let error = state.invoiceError {
                    Text(error)
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.danger)
                        .padding(.bottom, 16)
                }

                if let address = state.address {
                    if state.confirmedAmountSats > 0 {
                        // Tappable amount above the QR (PWA:939-948).
                        Button(action: onEditAmount) {
                            Text(FormatKt.formatBtc(sats: Int64(bitPattern: state.confirmedAmountSats)))
                                .font(ZinqqFont.display(18, weight: .bold))
                                .foregroundColor(colors.onDark)
                                .padding(.horizontal, 12)
                                .padding(.vertical, 4)
                        }
                        .accessibilityLabel("Edit amount")
                    }

                    // The PWA's snap pager as a paged TabView (R12 deviation).
                    TabView(selection: $page) {
                        QrView(
                            payload: state.qrValue,
                            accessibilityLabel: "QR code for Bitcoin address \(address)"
                        )
                        .padding(.horizontal, 16)
                        .tag(QrPage.unified)
                        if pages.contains(.bolt12) {
                            QrView(
                                payload: state.offerQrValue ?? "",
                                accessibilityLabel: "QR code for BOLT 12 offer"
                            )
                            .padding(.horizontal, 16)
                            .tag(QrPage.bolt12)
                        }
                        if pages.contains(.async) {
                            QrView(
                                payload: state.asyncOfferQrValue ?? "",
                                accessibilityLabel:
                                    "QR code for offline-payable BOLT 12 offer (experimental)"
                            )
                            .padding(.horizontal, 16)
                            .tag(QrPage.async)
                        }
                    }
                    .tabViewStyle(.page(indexDisplayMode: .never))
                    .frame(maxWidth: 300)
                    .aspectRatio(1, contentMode: .fit)
                    .padding(.top, 16)
                    // Reset to the unified page when the current page is
                    // removed (PWA:373-375).
                    .onChange(of: pages) { available in
                        if !available.contains(page) { page = .unified }
                    }

                    // Dot indicators (PWA:980-989) — one per live page, so a
                    // page the pager cannot reach never gets a dot.
                    if pages.count > 1 {
                        HStack(spacing: 8) {
                            ForEach(pages, id: \.self) { dot in
                                Circle()
                                    .fill(dot == page ? colors.onDark : colors.dotIdle)
                                    .frame(width: 8, height: 8)
                            }
                        }
                        .padding(.top, 16)
                    }

                    Text(
                        qrCaption(
                            page: page,
                            invoicePath: invoicePath,
                            openingFeeSats: state.openingFeeSats
                        )
                    )
                    .font(ZinqqFont.sans(12))
                    .foregroundColor(colors.onDarkMuted)
                    .multilineTextAlignment(.center)
                    .padding(.top, 24)

                    // Expiry countdown over a JIT invoice (R6), ticking via
                    // TimelineView; the controller owns the flip to expired.
                    if countdownVisible(
                        step: state.step,
                        editingAmount: state.editingAmount,
                        expiresAtUnix: state.expiresAtUnix
                    ) {
                        let expiresAt = state.expiresAtUnix ?? 0
                        TimelineView(.periodic(from: .now, by: 1)) { context in
                            Text(
                                countdownText(
                                    secondsLeft: countdownSecondsLeft(
                                        expiresAtUnix: expiresAt,
                                        nowUnixSecs: Int64(context.date.timeIntervalSince1970)
                                    )
                                )
                            )
                            .font(ZinqqFont.sans(12))
                            .foregroundColor(colors.onDarkMuted)
                        }
                        .padding(.top, 4)
                    }
                }
                Spacer()
            }
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 32)

            if state.address != nil {
                VStack(spacing: 12) {
                    SecondaryButton(
                        label: state.confirmedAmountSats > 0 ? "Edit amount" : "Add amount",
                        action: onEditAmount
                    )
                    // System share = the PWA's navigator.share (R12: platform
                    // share sheet is a sanctioned deviation).
                    ShareLink(
                        item: copyValue(page: page, bip321Uri: state.bip321Uri, offer: state.offer, asyncOffer: state.asyncOffer)
                    ) {
                        SecondaryButtonLabel(label: "Share")
                    }
                    .accessibilityLabel("Share")
                }
                .padding(.horizontal, 24)
                .padding(.bottom, 24)
            }
        }
        // Spike behavior preserved (U21 packet): the screen must not dim or
        // lock while a payer is scanning the displayed QR.
        .onAppear { UIApplication.shared.isIdleTimerDisabled = true }
        .onDisappear { UIApplication.shared.isIdleTimerDisabled = false }
    }
}

// MARK: - Amount entry (PWA Receive.tsx:893-930)

private struct AmountEntry: View {
    let state: ReceiveUiState
    let controller: ReceiveController

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        let needsJit = editingNeedsJit(
            usableInboundMsat: state.usableInboundMsat,
            amountSats: state.editingAmountSats
        )
        let belowMin = belowJitMinimum(
            needsJit: needsJit,
            amountSats: state.editingAmountSats,
            floorSats: state.floorSats
        )

        VStack(spacing: 0) {
            VStack(spacing: 0) {
                Spacer()
                if !state.needsAmount || state.confirmedAmountSats > 0 {
                    Button(action: { controller.cancelAmount() }) {
                        Text("Cancel")
                            .font(ZinqqFont.sans(14))
                            .foregroundColor(colors.onDarkMuted)
                            .padding(.horizontal, 12)
                            .padding(.vertical, 8)
                    }
                    .accessibilityLabel("Cancel")
                }
                Text(FormatKt.formatBtc(sats: Int64(bitPattern: state.editingAmountSats)))
                    .font(ZinqqFont.display(state.amountDigits.count > 5 ? 48 : 72, weight: .bold))
                    .foregroundColor(colors.amount)
                    .padding(.vertical, 8)
                if state.confirmedAmountSats > 0 {
                    Button(action: { controller.removeAmount() }) {
                        Text("Remove amount")
                            .font(ZinqqFont.sans(14))
                            .foregroundColor(colors.danger)
                            .padding(.horizontal, 12)
                            .padding(.vertical, 8)
                    }
                    .accessibilityLabel("Remove amount")
                }
                if belowMin {
                    // AE4: the below-floor block, PWA copy (Receive.tsx:918-921).
                    Text(minimumAlertText(floorSats: state.floorSats))
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.danger)
                        .padding(.top, 4)
                }
                Spacer()
            }
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 32)

            Numpad(
                onKey: { controller.onNumpadKey($0) },
                onNext: { controller.confirmAmount() },
                nextEnabled: numpadNextEnabled(
                    amountSats: state.editingAmountSats, belowMinimum: belowMin
                ),
                nextLabel: numpadCtaLabel(
                    needsAmount: state.needsAmount,
                    confirmedAmountSats: state.confirmedAmountSats
                )
            )
        }
    }
}

// MARK: - JIT review + skeleton (PWA Receive.tsx:672-806)

private struct QuotingSkeleton: View {
    let amountSats: UInt64

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(spacing: 12) {
                JitRow(label: "Amount") {
                    AmountText(sats: amountSats)
                }
                JitRow(label: "Setup fee") {
                    RoundedRectangle(cornerRadius: 4)
                        .fill(colors.onDark.opacity(0.1))
                        .frame(width: 80, height: 20)
                        .accessibilityLabel("Loading setup fee")
                }
                Divider().background(colors.darkBorder)
                JitRow(label: "You'll receive") {
                    RoundedRectangle(cornerRadius: 4)
                        .fill(colors.onDark.opacity(0.1))
                        .frame(width: 96, height: 20)
                        .accessibilityLabel("Loading net amount")
                }
            }
            .frame(maxWidth: 300)
            .padding(.horizontal, 32)
            ProgressView()
                .tint(colors.onDark)
                .scaleEffect(1.4)
                .padding(.top, 24)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }
}

private struct JitReviewScreen: View {
    let step: ReceiveStep
    let onGenerate: () -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            VStack(spacing: 0) {
                Spacer()
                VStack(spacing: 12) {
                    switch step {
                    case let .jitReview(review):
                        JitRow(label: "Amount") {
                            AmountText(sats: review.amountSats)
                        }
                        JitRow(label: "Setup fee") {
                            AmountText(sats: review.setupFeeSats, prefix: "− ")
                        }
                        Divider().background(colors.darkBorder)
                        JitRow(label: "You'll receive") {
                            AmountText(sats: review.youReceiveSats)
                        }
                        // The PWA's fallback-provider warning slot
                        // (Receive.tsx:753-761) is intentionally absent: the
                        // core configures a single LSP, so quotes have no
                        // fallback role to disclose.

                    case let .jitBelowMinimum(amountSats, displayMinSats):
                        JitRow(label: "Amount") {
                            AmountText(sats: amountSats)
                        }
                        Divider().background(colors.darkBorder)
                        Text(
                            "Minimum receive: "
                                + FormatKt.formatBtc(sats: Int64(bitPattern: displayMinSats))
                        )
                        .font(ZinqqFont.sans(14))
                        .foregroundColor(colors.onDarkMuted)

                    default:
                        EmptyView()
                    }
                }
                .frame(maxWidth: 300)
                .padding(.horizontal, 32)
                Spacer()
            }
            .frame(maxWidth: .infinity)

            BottomActions(
                primaryLabel: "Generate Payment Request",
                primaryEnabled: {
                    if case .jitReview = step { return true }
                    return false
                }(),
                onPrimary: onGenerate,
                onBack: onBack
            )
        }
    }
}

// MARK: - Expired / error (PWA Receive.tsx:814-892)

private struct ExpiredScreen: View {
    let onRetry: () -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            VStack(spacing: 0) {
                Spacer()
                StatusBadge(tint: colors.warning, systemName: "clock")
                Text("Payment request expired")
                    .font(ZinqqFont.sans(16, weight: .semibold))
                    .foregroundColor(colors.onDark)
                    .padding(.top, 24)
                Text("This request is no longer payable. Generate a new one to keep receiving.")
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(colors.onDarkMuted)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
                    .padding(.horizontal, 16)
                Spacer()
            }
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 32)
            BottomActions(
                primaryLabel: "Generate new request",
                primaryEnabled: true,
                onPrimary: onRetry,
                onBack: onBack
            )
        }
    }
}

private struct JitErrorScreen: View {
    let onRetry: () -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            VStack(spacing: 0) {
                Spacer()
                StatusBadge(tint: colors.danger, systemName: "xmark")
                Text("Could not generate payment request")
                    .font(ZinqqFont.sans(16, weight: .semibold))
                    .foregroundColor(colors.onDark)
                    .multilineTextAlignment(.center)
                    .padding(.top, 24)
                Spacer()
            }
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 32)
            BottomActions(
                primaryLabel: "Try again",
                primaryEnabled: true,
                onPrimary: onRetry,
                onBack: onBack
            )
        }
    }
}

// MARK: - Copy sheet

private struct CopySheet: View {
    let title: String
    let value: String
    let onClose: () -> Void

    @State private var copied = false

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        BottomSheetView(open: true, onClose: onClose) {
            Text(title)
                .font(ZinqqFont.sans(14, weight: .semibold))
                .foregroundColor(colors.onDark)
            Text(value)
                .font(.system(size: 12, design: .monospaced))
                .foregroundColor(colors.onDarkMuted)
                .padding(.top, 12)
            Button {
                UIPasteboard.general.string = value
                copied = true
            } label: {
                Text(copied ? "Copied!" : "Copy")
                    .font(ZinqqFont.sans(14, weight: .semibold))
                    .foregroundColor(colors.onPill)
                    .frame(maxWidth: .infinity)
                    .frame(height: 48)
                    .background(colors.pill)
                    .clipShape(RoundedRectangle(cornerRadius: 12))
            }
            .padding(.top, 16)
            .accessibilityLabel("Copy")
            .autoReset($copied, afterMs: copyFeedbackMs)
        }
    }
}

// MARK: - Shared bits

private struct CenteredStatus: View {
    let text: String

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            Text(text)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
            ProgressView()
                .tint(colors.onDark)
                .scaleEffect(1.4)
                .padding(.top, 24)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }
}

private struct StatusBadge: View {
    let tint: Color
    let systemName: String

    var body: some View {
        ZStack {
            Circle()
                .fill(tint.opacity(0.15))
                .frame(width: 64, height: 64)
            Image(systemName: systemName)
                .font(.system(size: 28, weight: .semibold))
                .foregroundColor(tint)
        }
    }
}

private struct JitRow<Value: View>: View {
    let label: String
    @ViewBuilder var value: Value

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        HStack(alignment: .center) {
            Text(label)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .padding(.trailing, 16)
            Spacer()
            value
        }
    }
}

private struct AmountText: View {
    let sats: UInt64
    var prefix: String = ""

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        Text("\(prefix)\(FormatKt.formatBtc(sats: Int64(bitPattern: sats)))")
            .font(ZinqqFont.display(18, weight: .bold))
            .foregroundColor(colors.onDark)
    }
}

private struct SecondaryButtonLabel: View {
    let label: String

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        Text(label)
            .font(ZinqqFont.sans(14, weight: .semibold))
            .foregroundColor(colors.onDark)
            .frame(maxWidth: .infinity)
            .frame(height: 56)
            .background(colors.darkElevated)
            .clipShape(RoundedRectangle(cornerRadius: 12))
    }
}

private struct SecondaryButton: View {
    let label: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            SecondaryButtonLabel(label: label)
        }
        .accessibilityLabel(label)
    }
}

private struct BottomActions: View {
    let primaryLabel: String
    let primaryEnabled: Bool
    let onPrimary: () -> Void
    let onBack: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 12) {
            Button(action: onPrimary) {
                Text(primaryLabel)
                    .font(ZinqqFont.display(18, weight: .bold))
                    .foregroundColor(colors.onCta)
                    .frame(maxWidth: .infinity)
                    .frame(height: 56)
                    .background(colors.cta)
                    .clipShape(RoundedRectangle(cornerRadius: 12))
                    .opacity(primaryEnabled ? 1 : 0.7)
            }
            .disabled(!primaryEnabled)
            .accessibilityLabel(primaryLabel)
            SecondaryButton(label: "Back", action: onBack)
        }
        .padding(.horizontal, 24)
        .padding(.top, 16)
        .padding(.bottom, 24)
    }
}
