import AVFoundation
import Shared
import XCTest

@testable import iosApp

/// The Scan screen's pure logic (U20, R13): decode routing against a fake
/// classifier, the toast debounce, the permission state machine's committed
/// contract (over `AVAuthorizationStatus`), and the transcribed camera-error
/// taxonomy — the same fixtures as Android's `ScanLogicTest`.
final class ScanLogicTests: XCTestCase {

    private let validClassifier: (String) -> ClassifiedKind = { _ in .bolt11 }
    private let invalidClassifier: (String) -> ClassifiedKind = { _ in .invalid }

    // MARK: decode routing (fake scanner results)

    func testValidDecodeNavigatesWithTheRawString() {
        let outcome = routeDecode(
            raw: "lnbc1fakeinvoice",
            classify: validClassifier,
            alreadyNavigated: false,
            toastVisible: false
        )
        XCTAssertEqual(.navigate(raw: "lnbc1fakeinvoice"), outcome)
    }

    func testInvalidDecodeShowsTheToast() {
        let outcome = routeDecode(
            raw: "https://example.com/not-a-payment",
            classify: invalidClassifier,
            alreadyNavigated: false,
            toastVisible: false
        )
        XCTAssertEqual(.invalidToast, outcome)
    }

    func testInvalidDecodeIsDebouncedWhileTheToastShows() {
        let outcome = routeDecode(
            raw: "junk",
            classify: invalidClassifier,
            alreadyNavigated: false,
            toastVisible: true
        )
        XCTAssertEqual(.none, outcome)
    }

    func testEmptyFramesDoNothing() {
        XCTAssertEqual(
            .none,
            routeDecode(
                raw: nil,
                classify: validClassifier,
                alreadyNavigated: false,
                toastVisible: false
            )
        )
        XCTAssertEqual(
            .none,
            routeDecode(
                raw: "  ",
                classify: validClassifier,
                alreadyNavigated: false,
                toastVisible: false
            )
        )
    }

    func testDecodesAfterNavigationAreIgnored() {
        let outcome = routeDecode(
            raw: "lnbc1fakeinvoice",
            classify: validClassifier,
            alreadyNavigated: true,
            toastVisible: false
        )
        XCTAssertEqual(.none, outcome)
    }

    func testToastCopyAndDurationMatchThePwa() {
        XCTAssertEqual("Not a valid payment code", invalidScanMessage)
        XCTAssertEqual(3_000, invalidScanToastMs)
        XCTAssertEqual("Position the QR Code in view to activate", scanCaption)
    }

    // MARK: permission state machine (the plan's committed contract)

    func testAuthorizedStatusEnablesTheCamera() {
        XCTAssertEqual(.granted, reduceCameraPermission(.authorized))
    }

    func testNotDeterminedStatusFiresTheSystemRequest() {
        XCTAssertEqual(.requesting, reduceCameraPermission(.notDetermined))
    }

    func testDenialDirectsToOsSettings() {
        // iOS never re-shows the system dialog after a denial, so Android's
        // DENIED_RETRY arm collapses into the Settings deep link.
        XCTAssertEqual(.deniedOpenSettings, reduceCameraPermission(.denied))
    }

    func testRestrictedStatusIsATerminalTaxonomyState() {
        XCTAssertEqual(.restricted, reduceCameraPermission(.restricted))
    }

    // MARK: camera-error taxonomy (PWA Scan.tsx:14-35, transcribed)

    func testTaxonomyCopyIsTranscribedFromThePwa() {
        XCTAssertEqual("No camera found on this device.", cameraErrorMessage(.notFound))
        XCTAssertEqual("Camera is being used by another app.", cameraErrorMessage(.inUse))
        XCTAssertEqual("Could not access camera", cameraErrorMessage(.unknown))
        XCTAssertEqual("Camera access is required to scan QR codes.", cameraRationaleMessage)
        XCTAssertEqual(
            "Camera access is required to scan QR codes. "
                + "Please enable it in your device settings.",
            cameraSettingsMessage
        )
    }

    func testInterruptionReasonsMapToTheTaxonomy() {
        XCTAssertEqual(
            .inUse,
            cameraInterruptionError(.videoDeviceInUseByAnotherClient)
        )
        XCTAssertEqual(
            .inUse,
            cameraInterruptionError(.videoDeviceNotAvailableWithMultipleForegroundApps)
        )
        // Recoverable interruptions are the session's to resume, not
        // terminal states.
        XCTAssertNil(cameraInterruptionError(.videoDeviceNotAvailableInBackground))
        XCTAssertNil(cameraInterruptionError(.videoDeviceNotAvailableDueToSystemPressure))
    }
}
