package zinqq.app.screens.scan

import uniffi.wallet_core.ClassifiedKind

/**
 * The pure half of the Scan screen (U15, R13): decode routing, the invalid
 * toast debounce, the camera-error taxonomy, and the camera-permission state
 * machine (the plan's committed contract, shared with iOS U20). The Compose
 * screen and CameraX wiring live in [ScanScreen]; validity itself comes from
 * the core's classifier (R14) via the injected callback.
 */

/** PWA `Scan.tsx:54` — the invalid-scan toast copy, verbatim. */
const val INVALID_SCAN_MESSAGE = "Not a valid payment code"

/** PWA `Scan.tsx:79-84` — the invalid toast auto-clears after 3 s. */
const val INVALID_SCAN_TOAST_MS = 3_000L

/** PWA `Scan.tsx:109` — the viewfinder caption, verbatim. */
const val SCAN_CAPTION = "Position the QR Code in view to activate"

/**
 * The PWA's camera-error taxonomy (`Scan.tsx:7-35`), with the
 * permission-denied arm replaced by the plan's committed Android contract
 * (U15): denied → inline rationale + retry; permanently denied → banner
 * deep-linking to OS Settings.
 */
enum class ScanCameraError { NOT_FOUND, IN_USE, UNKNOWN }

/** Persistent-error copy (PWA `Scan.tsx:14-27`, transcribed). */
fun cameraErrorMessage(error: ScanCameraError): String = when (error) {
    ScanCameraError.NOT_FOUND -> "No camera found on this device."
    ScanCameraError.IN_USE -> "Camera is being used by another app."
    ScanCameraError.UNKNOWN -> "Could not access camera"
}

/**
 * Map a CameraX `CameraState` error code to the taxonomy; `null` for
 * recoverable errors CameraX retries itself. The codes are compile-time
 * constants mirrored from `androidx.camera.core.CameraState` so this stays
 * JVM-unit-testable.
 */
fun cameraStateError(code: Int): ScanCameraError? = when (code) {
    CAMERA_STATE_ERROR_MAX_CAMERAS_IN_USE,
    CAMERA_STATE_ERROR_CAMERA_IN_USE,
    -> ScanCameraError.IN_USE
    CAMERA_STATE_ERROR_OTHER_RECOVERABLE_ERROR -> null
    else -> ScanCameraError.UNKNOWN
}

// androidx.camera.core.CameraState error codes (stable public API values).
const val CAMERA_STATE_ERROR_MAX_CAMERAS_IN_USE = 1
const val CAMERA_STATE_ERROR_CAMERA_IN_USE = 2
const val CAMERA_STATE_ERROR_OTHER_RECOVERABLE_ERROR = 3

// --- Camera permission (the plan's committed contract, U15/U20) ---

/** What the permission area of the screen shows. */
enum class CameraPermissionUi {
    /** Waiting for the initial system dialog fired on entry. */
    REQUESTING,

    /** Camera usable; the preview and analyzer run. */
    GRANTED,

    /** Denied once — inline rationale banner with a Retry button. */
    DENIED_RETRY,

    /** Permanently denied — banner directing to OS Settings (deep link). */
    DENIED_OPEN_SETTINGS,
}

/** Rationale-banner copy (adapted from PWA `Scan.tsx:17`, per the contract). */
const val CAMERA_RATIONALE_MESSAGE = "Camera access is required to scan QR codes."

/** Permanently-denied copy: the PWA's string with settings pointed at the OS. */
const val CAMERA_SETTINGS_MESSAGE =
    "Camera access is required to scan QR codes. Please enable it in your device settings."

/**
 * The permission state machine's single transition: the outcome of a
 * permission check or request. `shouldShowRationale == true` after a denial
 * means the OS will show the dialog again (retry is useful); `false` means
 * "don't ask again" — only Settings can grant it now.
 */
fun reduceCameraPermission(granted: Boolean, shouldShowRationale: Boolean): CameraPermissionUi =
    when {
        granted -> CameraPermissionUi.GRANTED
        shouldShowRationale -> CameraPermissionUi.DENIED_RETRY
        else -> CameraPermissionUi.DENIED_OPEN_SETTINGS
    }

// --- Decode routing ---

/** What a decoded QR frame should do (tested against a fake analyzer). */
sealed interface DecodeOutcome {
    /** Valid payment code: navigate to Send with the RAW string (R13/R14). */
    data class Navigate(val raw: String) : DecodeOutcome

    /** Invalid code: show the 3 s "Not a valid payment code" toast. */
    data object InvalidToast : DecodeOutcome

    /** Nothing to do (no code, empty frame, debounced, or already leaving). */
    data object None : DecodeOutcome
}

/**
 * Route one analyzer result (PWA `Scan.tsx:49-61`): the classifier verdict
 * comes from the core; the raw string — never a parsed object — is what
 * navigates. The toast is debounced: while one is showing, further invalid
 * frames are dropped.
 */
fun routeDecode(
    raw: String?,
    classify: (String) -> ClassifiedKind,
    alreadyNavigated: Boolean,
    toastVisible: Boolean,
): DecodeOutcome {
    if (alreadyNavigated) return DecodeOutcome.None
    val value = raw?.takeIf { it.isNotBlank() } ?: return DecodeOutcome.None
    return if (classify(value) == ClassifiedKind.INVALID) {
        if (toastVisible) DecodeOutcome.None else DecodeOutcome.InvalidToast
    } else {
        DecodeOutcome.Navigate(value)
    }
}
