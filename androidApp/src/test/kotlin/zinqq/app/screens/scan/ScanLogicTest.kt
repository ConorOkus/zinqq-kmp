package zinqq.app.screens.scan

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import uniffi.wallet_core.ClassifiedKind

/**
 * The Scan screen's pure logic (U15, R13): decode routing against a fake
 * classifier, the toast debounce, the permission state machine's committed
 * contract, and the transcribed camera-error taxonomy.
 */
class ScanLogicTest {

    private val validClassifier: (String) -> ClassifiedKind = { ClassifiedKind.BOLT11 }
    private val invalidClassifier: (String) -> ClassifiedKind = { ClassifiedKind.INVALID }

    // --- decode routing (fake analyzer results) ---

    @Test
    fun validDecodeNavigatesWithTheRawString() {
        val outcome = routeDecode(
            raw = "lnbc1fakeinvoice",
            classify = validClassifier,
            alreadyNavigated = false,
            toastVisible = false,
        )
        assertEquals(DecodeOutcome.Navigate("lnbc1fakeinvoice"), outcome)
    }

    @Test
    fun invalidDecodeShowsTheToast() {
        val outcome = routeDecode(
            raw = "https://example.com/not-a-payment",
            classify = invalidClassifier,
            alreadyNavigated = false,
            toastVisible = false,
        )
        assertEquals(DecodeOutcome.InvalidToast, outcome)
    }

    @Test
    fun invalidDecodeIsDebouncedWhileTheToastShows() {
        val outcome = routeDecode(
            raw = "junk",
            classify = invalidClassifier,
            alreadyNavigated = false,
            toastVisible = true,
        )
        assertEquals(DecodeOutcome.None, outcome)
    }

    @Test
    fun emptyFramesDoNothing() {
        assertEquals(
            DecodeOutcome.None,
            routeDecode(null, validClassifier, alreadyNavigated = false, toastVisible = false),
        )
        assertEquals(
            DecodeOutcome.None,
            routeDecode("  ", validClassifier, alreadyNavigated = false, toastVisible = false),
        )
    }

    @Test
    fun decodesAfterNavigationAreIgnored() {
        val outcome = routeDecode(
            raw = "lnbc1fakeinvoice",
            classify = validClassifier,
            alreadyNavigated = true,
            toastVisible = false,
        )
        assertEquals(DecodeOutcome.None, outcome)
    }

    @Test
    fun toastCopyAndDurationMatchThePwa() {
        assertEquals("Not a valid payment code", INVALID_SCAN_MESSAGE)
        assertEquals(3_000L, INVALID_SCAN_TOAST_MS)
        assertEquals("Position the QR Code in view to activate", SCAN_CAPTION)
    }

    // --- permission state machine (the plan's committed contract) ---

    @Test
    fun grantResultEnablesTheCamera() {
        assertEquals(
            CameraPermissionUi.GRANTED,
            reduceCameraPermission(granted = true, shouldShowRationale = false),
        )
        assertEquals(
            CameraPermissionUi.GRANTED,
            reduceCameraPermission(granted = true, shouldShowRationale = true),
        )
    }

    @Test
    fun denialWithRationaleOffersRetry() {
        assertEquals(
            CameraPermissionUi.DENIED_RETRY,
            reduceCameraPermission(granted = false, shouldShowRationale = true),
        )
    }

    @Test
    fun permanentDenialDirectsToOsSettings() {
        assertEquals(
            CameraPermissionUi.DENIED_OPEN_SETTINGS,
            reduceCameraPermission(granted = false, shouldShowRationale = false),
        )
    }

    // --- camera-error taxonomy (PWA Scan.tsx:14-35, transcribed) ---

    @Test
    fun taxonomyCopyIsTranscribedFromThePwa() {
        assertEquals(
            "No camera found on this device.",
            cameraErrorMessage(ScanCameraError.NOT_FOUND),
        )
        assertEquals(
            "Camera is being used by another app.",
            cameraErrorMessage(ScanCameraError.IN_USE),
        )
        assertEquals("Could not access camera", cameraErrorMessage(ScanCameraError.UNKNOWN))
        assertEquals("Camera access is required to scan QR codes.", CAMERA_RATIONALE_MESSAGE)
        assertEquals(
            "Camera access is required to scan QR codes. " +
                "Please enable it in your device settings.",
            CAMERA_SETTINGS_MESSAGE,
        )
    }

    @Test
    fun cameraStateCodesMapToTheTaxonomy() {
        assertEquals(ScanCameraError.IN_USE, cameraStateError(CAMERA_STATE_ERROR_CAMERA_IN_USE))
        assertEquals(
            ScanCameraError.IN_USE,
            cameraStateError(CAMERA_STATE_ERROR_MAX_CAMERAS_IN_USE),
        )
        // Recoverable errors are CameraX's to retry, not terminal states.
        assertNull(cameraStateError(CAMERA_STATE_ERROR_OTHER_RECOVERABLE_ERROR))
        assertEquals(ScanCameraError.UNKNOWN, cameraStateError(6))
    }
}
