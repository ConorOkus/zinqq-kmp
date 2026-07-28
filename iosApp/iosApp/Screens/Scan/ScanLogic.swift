import AVFoundation
import Shared

/// The pure half of the Scan screen (U20, R13): decode routing, the invalid
/// toast debounce, the camera-error taxonomy, and the camera-permission
/// state machine (the plan's committed contract, shared with Android U15).
/// The SwiftUI screen and VisionKit/AVCapture wiring live in `ScanScreen`;
/// validity itself comes from the core's classifier (R14) via the injected
/// callback.

/// PWA `Scan.tsx:54` — the invalid-scan toast copy, verbatim.
let invalidScanMessage = "Not a valid payment code"

/// PWA `Scan.tsx:79-84` — the invalid toast auto-clears after 3 s.
let invalidScanToastMs: UInt64 = 3_000

/// PWA `Scan.tsx:109` — the viewfinder caption, verbatim.
let scanCaption = "Position the QR Code in view to activate"

// MARK: - Camera-error taxonomy

/// The PWA's camera-error taxonomy (`Scan.tsx:7-35`), with the
/// permission-denied arm replaced by the plan's committed contract (U20).
enum ScanCameraError: Equatable {
    case notFound
    case inUse
    case unknown
}

/// Persistent-error copy (PWA `Scan.tsx:14-27`, transcribed — the same
/// strings as Android's `cameraErrorMessage`).
func cameraErrorMessage(_ error: ScanCameraError) -> String {
    switch error {
    case .notFound: return "No camera found on this device."
    case .inUse: return "Camera is being used by another app."
    case .unknown: return "Could not access camera"
    }
}

/// Map an `AVCaptureSession` interruption reason to the taxonomy; `nil` for
/// recoverable interruptions the session resumes itself (the iOS twin of
/// Android's CameraX `cameraStateError`).
func cameraInterruptionError(
    _ reason: AVCaptureSession.InterruptionReason
) -> ScanCameraError? {
    switch reason {
    case .videoDeviceInUseByAnotherClient,
         .videoDeviceNotAvailableWithMultipleForegroundApps:
        return .inUse
    case .videoDeviceNotAvailableInBackground,
         .audioDeviceInUseByAnotherClient,
         .videoDeviceNotAvailableDueToSystemPressure:
        // Recoverable: the session resumes when the app foregrounds / the
        // pressure clears — not terminal taxonomy states.
        return nil
    @unknown default:
        return .unknown
    }
}

// MARK: - Camera permission (the plan's committed contract, U15/U20)

/// What the permission area of the screen shows.
enum CameraPermissionUi: Equatable {
    /// Waiting for the initial system dialog fired on entry.
    case requesting

    /// Camera usable; the preview and scanner run.
    case granted

    /// Denied — banner directing to OS Settings (deep link). iOS never
    /// re-shows the system dialog after a denial, so Android's DENIED_RETRY
    /// arm collapses into this state; a grant made in Settings is picked up
    /// on return via re-check (the contract's retry path).
    case deniedOpenSettings

    /// Restricted (parental controls / MDM): no one can grant it here —
    /// taxonomy copy only, no Settings affordance.
    case restricted
}

/// Rationale-banner copy (adapted from PWA `Scan.tsx:17`, per the contract).
let cameraRationaleMessage = "Camera access is required to scan QR codes."

/// Denied copy: the PWA's string with settings pointed at the OS.
let cameraSettingsMessage =
    "Camera access is required to scan QR codes. Please enable it in your device settings."

/// The permission state machine's single transition: the outcome of an
/// `AVCaptureDevice.authorizationStatus` check (or a `requestAccess`
/// completion re-check). `notDetermined` → fire the system request;
/// `denied` → Settings deep link; `restricted` → taxonomy copy.
func reduceCameraPermission(_ status: AVAuthorizationStatus) -> CameraPermissionUi {
    switch status {
    case .authorized: return .granted
    case .notDetermined: return .requesting
    case .restricted: return .restricted
    case .denied: return .deniedOpenSettings
    @unknown default: return .deniedOpenSettings
    }
}

// MARK: - Decode routing

/// What a decoded QR frame should do (tested against a fake scanner).
enum DecodeOutcome: Equatable {
    /// Valid payment code: navigate to Send with the RAW string (R13/R14).
    case navigate(raw: String)

    /// Invalid code: show the 3 s "Not a valid payment code" toast.
    case invalidToast

    /// Nothing to do (no code, empty frame, debounced, or already leaving).
    case none
}

/// Route one scanner result (PWA `Scan.tsx:49-61`): the classifier verdict
/// comes from the core; the raw string — never a parsed object — is what
/// navigates. The toast is debounced: while one is showing, further invalid
/// frames are dropped.
func routeDecode(
    raw: String?,
    classify: (String) -> ClassifiedKind,
    alreadyNavigated: Bool,
    toastVisible: Bool
) -> DecodeOutcome {
    if alreadyNavigated { return .none }
    guard let value = raw,
          !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
        return .none
    }
    if classify(value) == .invalid {
        return toastVisible ? .none : .invalidToast
    }
    return .navigate(raw: value)
}
