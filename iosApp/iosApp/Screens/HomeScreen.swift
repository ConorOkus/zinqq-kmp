import SwiftUI
import UIKit

/// Home temporarily hosts the spike's single wallet screen (U18): the proven
/// receive/send flow moved here from `ContentView` unchanged so the shell
/// keeps paying while U19 builds the real Home. Only the QR rendering moved
/// into the shared `QrView` component and the hardcoded colors onto the field
/// tokens; everything else is the spike content verbatim. Talks only to
/// WalletModel — no shared-framework types leak into the view (R14).
struct HomeScreen: View {
    @ObservedObject var model: WalletModel

    @State private var amountSats = ""
    @State private var bolt11Input = ""

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                balanceSection
                if let banner = model.syncBanner {
                    Text(banner)
                        .font(ZinqqFont.sans(13))
                        .foregroundColor(colors.warning)
                }
                receiveSection
                sendSection
                statusSection
            }
            .padding(24)
        }
        .background(colors.field.ignoresSafeArea())
        .onChange(of: model.currentInvoice) { invoice in
            // Screen must not sleep mid-payment while an invoice is showing.
            UIApplication.shared.isIdleTimerDisabled = invoice != nil
        }
        .onDisappear {
            UIApplication.shared.isIdleTimerDisabled = false
        }
    }

    // MARK: Balance

    private var balanceSection: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Balance")
                .font(ZinqqFont.sans(16, weight: .semibold))
                .foregroundColor(colors.onField)
            Text("\(model.balanceMsat / 1_000) sats")
                .font(ZinqqFont.display(34, weight: .semibold))
                .foregroundColor(colors.onField)
                .accessibilityLabel("Balance \(model.balanceMsat / 1_000) sats")
            Text("\(model.balanceMsat) msat · node \(model.running ? "running" : "stopped")")
                .font(ZinqqFont.sans(13))
                .foregroundColor(colors.onFieldMuted)
        }
    }

    // MARK: Receive

    private var receiveSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Receive")
                .font(ZinqqFont.sans(16, weight: .semibold))
                .foregroundColor(colors.onField)
            HStack {
                TextField("Amount (sats)", text: $amountSats)
                    .keyboardType(.numberPad)
                    .textFieldStyle(.roundedBorder)
                    .accessibilityLabel("Amount in sats")
                let parsedSats = UInt64(amountSats)
                Button("Request invoice") {
                    guard let sats = parsedSats, sats > 0 else { return }
                    model.requestInvoice(amountSats: sats)
                }
                .buttonStyle(.borderedProminent)
                .tint(colors.fieldCta)
                .disabled(!model.running || model.busy || parsedSats == nil)
                .accessibilityLabel("Request invoice")
            }
            if let invoice = model.currentInvoice {
                invoiceView(invoice)
            }
        }
    }

    private func invoiceView(_ invoice: WalletModel.Invoice) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            QrView(payload: invoice.bolt11, accessibilityLabel: "Invoice QR code")
                .frame(maxWidth: 240)
                .frame(maxWidth: .infinity)
            Text(invoice.bolt11)
                .font(.system(.caption2, design: .monospaced))
                .foregroundColor(colors.onField)
                .lineLimit(4)
                .truncationMode(.middle)
                .textSelection(.enabled)
            TimelineView(.periodic(from: .now, by: 1)) { context in
                let remaining = Int(Double(invoice.expiryUnixSecs) - context.date.timeIntervalSince1970)
                if remaining > 0 {
                    Text("Expires in \(remaining / 60)m \(remaining % 60)s")
                        .font(ZinqqFont.sans(13))
                        .foregroundColor(colors.onFieldMuted)
                } else {
                    Text("Invoice expired")
                        .font(ZinqqFont.sans(13))
                        .foregroundColor(colors.dangerStrong)
                }
            }
        }
    }

    // MARK: Send

    private var sendSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Send")
                .font(ZinqqFont.sans(16, weight: .semibold))
                .foregroundColor(colors.onField)
            TextField("Paste BOLT11 invoice", text: $bolt11Input, axis: .vertical)
                .lineLimit(2...4)
                .font(.system(.caption, design: .monospaced))
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .textFieldStyle(.roundedBorder)
                .accessibilityLabel("BOLT11 invoice")
            Button("Pay") {
                model.sendPayment(bolt11: bolt11Input)
            }
            .buttonStyle(.borderedProminent)
            .tint(colors.fieldCta)
            .disabled(
                !model.running || model.busy
                    || bolt11Input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            )
            .accessibilityLabel("Pay")
        }
    }

    // MARK: Status

    @ViewBuilder
    private var statusSection: some View {
        if let outcome = model.lastOutcome {
            Text(outcome)
                .font(ZinqqFont.sans(13))
                .foregroundColor(colors.onFieldMuted)
        }
    }
}
