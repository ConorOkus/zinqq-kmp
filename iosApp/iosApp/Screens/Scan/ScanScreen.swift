import AVFoundation
import Shared
import SwiftUI
import UIKit
import VisionKit

/// The Scan screen (U20, R13): VisionKit `DataScannerViewController`
/// (QR symbology, gated on `isSupported`/`isAvailable`) with an
/// `AVCaptureSession` + `AVCaptureMetadataOutput` fallback, over the PWA's
/// layout (`Scan.tsx`) — 256pt viewfinder frame, caption, 3 s invalid toast,
/// camera-error taxonomy — plus the plan's committed camera permission
/// contract. A valid decode navigates to Send with the RAW string; validity
/// itself is the core classifier's verdict via `SendPort` (R14).
struct ScanScreen: View {
    let port: any SendPort
    let onScanned: (String) -> Void
    let onClose: () -> Void

    @State private var permission: CameraPermissionUi =
        reduceCameraPermission(AVCaptureDevice.authorizationStatus(for: .video))
    @State private var cameraError: ScanCameraError?
    @State private var toastVisible = false
    @State private var navigated = false

    @Environment(\.zinqqColors) private var colors
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        ZStack {
            if permission == .granted && cameraError == nil {
                ScannerContainer(
                    onCode: handleCode,
                    onError: { cameraError = $0 }
                )
                .ignoresSafeArea()
            }

            VStack(spacing: 0) {
                ScreenHeader(title: "Scan", onClose: onClose, tint: .white)
                ZStack {
                    centerContent
                    if toastVisible {
                        // Transient invalid toast (PWA Scan.tsx:120-125).
                        VStack {
                            Spacer()
                            Text(invalidScanMessage)
                                .font(ZinqqFont.sans(14, weight: .medium))
                                .foregroundColor(.white)
                                .multilineTextAlignment(.center)
                                .padding(.horizontal, 16)
                                .padding(.vertical, 12)
                                .frame(maxWidth: .infinity)
                                .background(colors.dangerStrong.opacity(0.9))
                                .clipShape(RoundedRectangle(cornerRadius: 12))
                                .padding(.horizontal, 16)
                                .padding(.bottom, 32)
                        }
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black.ignoresSafeArea())
        .onAppear {
            // Committed contract: the initial request fires on entry.
            if permission == .requesting {
                AVCaptureDevice.requestAccess(for: .video) { _ in
                    Task { @MainActor in
                        permission = reduceCameraPermission(
                            AVCaptureDevice.authorizationStatus(for: .video)
                        )
                    }
                }
            }
            // The taxonomy's not-found arm is knowable up front.
            if AVCaptureDevice.default(for: .video) == nil {
                cameraError = .notFound
            }
        }
        // Returning from OS Settings must pick up a grant without re-entering.
        .onChange(of: scenePhase) { phase in
            if phase == .active,
               AVCaptureDevice.authorizationStatus(for: .video) == .authorized {
                permission = .granted
            }
        }
        // Invalid toast auto-clears after 3 s (PWA Scan.tsx:79-84).
        .task(id: toastVisible) {
            if toastVisible {
                try? await Task.sleep(nanoseconds: invalidScanToastMs * 1_000_000)
                toastVisible = false
            }
        }
    }

    /// Same precedence as Android's `when` block: permission banners first,
    /// then the camera-error taxonomy, then the viewfinder.
    @ViewBuilder
    private var centerContent: some View {
        if permission == .deniedOpenSettings {
            PermissionBanner(
                message: cameraSettingsMessage,
                actionLabel: "Open Settings",
                onAction: {
                    if let url = URL(string: UIApplication.openSettingsURLString) {
                        UIApplication.shared.open(url)
                    }
                }
            )
        } else if permission == .restricted {
            Text(cameraRationaleMessage)
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 32)
        } else if let cameraError {
            Text(cameraErrorMessage(cameraError))
                .font(ZinqqFont.sans(14))
                .foregroundColor(colors.onDarkMuted)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 32)
        } else if permission == .granted {
            // Viewfinder frame + caption (PWA Scan.tsx:104-111).
            VStack(spacing: 0) {
                RoundedRectangle(cornerRadius: 16)
                    .strokeBorder(Color.white.opacity(0.6), lineWidth: 2)
                    .frame(width: 256, height: 256)
                    .accessibilityLabel("QR viewfinder")
                Text(scanCaption)
                    .font(ZinqqFont.sans(14))
                    .foregroundColor(.white.opacity(0.7))
                    .padding(.top, 24)
            }
        }
    }

    /// One decoded QR string from either scanner backend: the core is the
    /// only classifier (R14), and the raw string is what navigates (R13).
    private func handleCode(_ raw: String) {
        guard !navigated else { return }
        Task { @MainActor in
            let kind = (try? await port.classify(raw))?.kind ?? ClassifiedKind.invalid
            let outcome = routeDecode(
                raw: raw,
                classify: { _ in kind },
                alreadyNavigated: navigated,
                toastVisible: toastVisible
            )
            switch outcome {
            case let .navigate(value):
                if !navigated {
                    navigated = true
                    onScanned(value)
                }
            case .invalidToast:
                toastVisible = true
            case .none:
                break
            }
        }
    }
}

// MARK: - Scanner backends

/// Picks the plan-mandated VisionKit scanner when the device supports it,
/// falling back to AVCapture metadata scanning otherwise (older/unsupported
/// hardware). Both feed the same `onCode` path.
private struct ScannerContainer: View {
    let onCode: (String) -> Void
    let onError: (ScanCameraError) -> Void

    var body: some View {
        if DataScannerViewController.isSupported && DataScannerViewController.isAvailable {
            DataScannerView(onCode: onCode)
        } else {
            MetadataScannerView(onCode: onCode, onError: onError)
        }
    }
}

/// VisionKit `DataScannerViewController` wrapped for SwiftUI: QR symbology
/// only, continuous recognition, no built-in highlighting (the viewfinder
/// overlay is ours).
private struct DataScannerView: UIViewControllerRepresentable {
    let onCode: (String) -> Void

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let controller = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            isHighlightingEnabled: false
        )
        controller.delegate = context.coordinator
        return controller
    }

    func updateUIViewController(_ controller: DataScannerViewController, context: Context) {
        if !controller.isScanning {
            try? controller.startScanning()
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onCode: onCode)
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onCode: (String) -> Void

        init(onCode: @escaping (String) -> Void) {
            self.onCode = onCode
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController,
            didAdd addedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            for item in addedItems {
                if case let .barcode(barcode) = item,
                   let value = barcode.payloadStringValue {
                    onCode(value)
                }
            }
        }
    }
}

/// AVCapture fallback for devices VisionKit doesn't support:
/// `AVCaptureMetadataOutput` restricted to QR, with session interruptions
/// mapped through the taxonomy.
private struct MetadataScannerView: UIViewControllerRepresentable {
    let onCode: (String) -> Void
    let onError: (ScanCameraError) -> Void

    func makeUIViewController(context: Context) -> MetadataScannerController {
        let controller = MetadataScannerController()
        controller.onCode = onCode
        controller.onError = onError
        return controller
    }

    func updateUIViewController(_ controller: MetadataScannerController, context: Context) {}
}

final class MetadataScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    var onCode: ((String) -> Void)?
    var onError: ((ScanCameraError) -> Void)?

    private let session = AVCaptureSession()
    private var previewLayer: AVCaptureVideoPreviewLayer?

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black

        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device) else {
            onError?(.notFound)
            return
        }
        guard session.canAddInput(input) else {
            onError?(.inUse)
            return
        }
        session.addInput(input)

        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else {
            onError?(.unknown)
            return
        }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        guard output.availableMetadataObjectTypes.contains(.qr) else {
            onError?(.unknown)
            return
        }
        output.metadataObjectTypes = [.qr]

        let layer = AVCaptureVideoPreviewLayer(session: session)
        layer.videoGravity = .resizeAspectFill
        view.layer.addSublayer(layer)
        previewLayer = layer

        // Session interruptions feed the taxonomy (in-use etc.).
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(sessionInterrupted(_:)),
            name: .AVCaptureSessionWasInterrupted,
            object: session
        )

        let session = self.session
        DispatchQueue.global(qos: .userInitiated).async {
            session.startRunning()
        }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        previewLayer?.frame = view.bounds
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        let session = self.session
        DispatchQueue.global(qos: .userInitiated).async {
            session.stopRunning()
        }
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        for object in metadataObjects {
            if let code = object as? AVMetadataMachineReadableCodeObject,
               let value = code.stringValue {
                onCode?(value)
            }
        }
    }

    @objc private func sessionInterrupted(_ notification: Notification) {
        guard let rawReason = notification
            .userInfo?[AVCaptureSessionInterruptionReasonKey] as? Int,
            let reason = AVCaptureSession.InterruptionReason(rawValue: rawReason),
            let error = cameraInterruptionError(reason) else { return }
        onError?(error)
    }
}

// MARK: - Permission banner

private struct PermissionBanner: View {
    let message: String
    let actionLabel: String
    let onAction: () -> Void

    @Environment(\.zinqqColors) private var colors

    var body: some View {
        VStack(spacing: 16) {
            Text(message)
                .font(ZinqqFont.sans(14))
                .foregroundColor(.white)
                .multilineTextAlignment(.center)
            Button(action: onAction) {
                Text(actionLabel)
                    .font(ZinqqFont.display(15, weight: .bold))
                    .foregroundColor(colors.onCta)
                    .padding(.horizontal, 20)
                    .padding(.vertical, 10)
                    .background(colors.cta)
                    .clipShape(RoundedRectangle(cornerRadius: 10))
            }
            .accessibilityLabel(actionLabel)
        }
        .padding(20)
        .frame(maxWidth: .infinity)
        .background(Color.white.opacity(0.1))
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .padding(.horizontal, 24)
    }
}
