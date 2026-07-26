import CoreImage.CIFilterBuiltins
import SwiftUI
import UIKit

/// Single wallet screen (R8: rough is acceptable): balance, receive
/// (amount → invoice QR + expiry countdown), send (paste BOLT11), and a
/// status/outcome line. Talks only to WalletModel — no shared-framework
/// types leak into the view.
struct ContentView: View {
    @ObservedObject var model: WalletModel

    @State private var amountSats = ""
    @State private var bolt11Input = ""

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                balanceSection
                if let banner = model.syncBanner {
                    Text(banner)
                        .font(.footnote)
                        .foregroundColor(.orange)
                }
                receiveSection
                sendSection
                statusSection
            }
            .padding(24)
        }
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
                .font(.headline)
            Text("\(model.balanceMsat / 1_000) sats")
                .font(.system(.largeTitle, design: .rounded).weight(.semibold))
            Text("\(model.balanceMsat) msat · node \(model.running ? "running" : "stopped")")
                .font(.footnote)
                .foregroundColor(.secondary)
        }
    }

    // MARK: Receive

    private var receiveSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Receive")
                .font(.headline)
            HStack {
                TextField("Amount (sats)", text: $amountSats)
                    .keyboardType(.numberPad)
                    .textFieldStyle(.roundedBorder)
                Button("Request invoice") {
                    guard let sats = UInt64(amountSats), sats > 0 else { return }
                    model.requestInvoice(amountSats: sats)
                }
                .buttonStyle(.borderedProminent)
                .disabled(!model.running || UInt64(amountSats) == nil)
            }
            if let invoice = model.currentInvoice {
                invoiceView(invoice)
            }
        }
    }

    private func invoiceView(_ invoice: WalletModel.Invoice) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            if let qr = Self.qrImage(for: invoice.bolt11) {
                Image(uiImage: qr)
                    .interpolation(.none)
                    .resizable()
                    .scaledToFit()
                    .frame(maxWidth: 240)
                    .frame(maxWidth: .infinity)
            }
            Text(invoice.bolt11)
                .font(.system(.caption2, design: .monospaced))
                .lineLimit(4)
                .truncationMode(.middle)
                .textSelection(.enabled)
            TimelineView(.periodic(from: .now, by: 1)) { context in
                let remaining = Int(Double(invoice.expiryUnixSecs) - context.date.timeIntervalSince1970)
                if remaining > 0 {
                    Text("Expires in \(remaining / 60)m \(remaining % 60)s")
                        .font(.footnote)
                        .foregroundColor(.secondary)
                } else {
                    Text("Invoice expired")
                        .font(.footnote)
                        .foregroundColor(.red)
                }
            }
        }
    }

    // MARK: Send

    private var sendSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Send")
                .font(.headline)
            TextField("Paste BOLT11 invoice", text: $bolt11Input, axis: .vertical)
                .lineLimit(2...4)
                .font(.system(.caption, design: .monospaced))
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .textFieldStyle(.roundedBorder)
            Button("Pay") {
                model.sendPayment(bolt11: bolt11Input)
            }
            .buttonStyle(.borderedProminent)
            .disabled(!model.running || bolt11Input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
    }

    // MARK: Status

    @ViewBuilder
    private var statusSection: some View {
        if let outcome = model.lastOutcome {
            Text(outcome)
                .font(.footnote)
                .foregroundColor(.secondary)
        }
    }

    // MARK: QR (CoreImage, no dependency)

    private static func qrImage(for text: String) -> UIImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        guard let output = filter.outputImage else { return nil }
        let scaled = output.transformed(by: CGAffineTransform(scaleX: 8, y: 8))
        guard let cgImage = CIContext().createCGImage(scaled, from: scaled.extent) else { return nil }
        return UIImage(cgImage: cgImage)
    }
}
