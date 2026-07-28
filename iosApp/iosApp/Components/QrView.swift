import CoreImage.CIFilterBuiltins
import SwiftUI
import UIKit

/// QR renderer on the PWA's `qr-tile` token (U18, KTD-11): the payload is
/// opaque display data (R14) — a string goes in, pixels come out via
/// CoreImage. The tile stays white-ish in every mode so scanners get
/// contrast.
struct QrView: View {
    let payload: String
    let accessibilityLabel: String

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        Group {
            if let image = QrCache.image(for: payload) {
                Image(uiImage: image)
                    .interpolation(.none)
                    .resizable()
                    .scaledToFit()
            } else {
                Color.clear
            }
        }
        .padding(12)
        .aspectRatio(1, contentMode: .fit)
        .background(colors.qrTile)
        .clipShape(RoundedRectangle(cornerRadius: 16))
        .accessibilityLabel(accessibilityLabel)
    }
}

/// Memoizes the last generated QR so unrelated observable changes (balance,
/// outcome, sync banner) don't re-rasterize an unchanged payload, and shares
/// one CIContext, which is expensive to construct. (Moved verbatim from the
/// spike's ContentView.)
@MainActor
enum QrCache {
    private static let context = CIContext()
    private static var last: (text: String, image: UIImage)?

    static func image(for text: String) -> UIImage? {
        if let last, last.text == text { return last.image }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        guard let output = filter.outputImage else { return nil }
        let scaled = output.transformed(by: CGAffineTransform(scaleX: 8, y: 8))
        guard let cgImage = context.createCGImage(scaled, from: scaled.extent) else { return nil }
        let image = UIImage(cgImage: cgImage)
        last = (text, image)
        return image
    }
}
