import SwiftUI
import XCTest

@testable import iosApp

/// Snapshot-style pins of the three mode tables against `index.css` (U18,
/// KTD-11, R12), mirroring Android's `ZinqqColorsTest` assert-for-assert:
/// the CSS token tables are the spec, these asserts are the transcription
/// check. Spot set per mode: the roles the shell renders first (field/cta/
/// pill families) plus the mode-specific status overrides.
final class ZinqqColorsTests: XCTestCase {
    func testHybridBaseTableMatchesIndexCss() {
        let c = ZinqqColors.hybrid
        XCTAssertEqual(ZinqqColors.rgb(0xE4D7BE), c.accent)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.hot)
        XCTAssertEqual(ZinqqColors.rgb(0x12100C), c.dark)
        XCTAssertEqual(ZinqqColors.rgb(0x1C1913), c.darkSurface)
        XCTAssertEqual(ZinqqColors.rgb(0x231F18), c.darkElevated)
        XCTAssertEqual(ZinqqColors.rgb(0x2E2921), c.darkBorder)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F0E4), c.onDark)
        XCTAssertEqual(ZinqqColors.rgb(0xE4D7BE), c.field)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.onField)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.fieldCta)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.tabActive)
        XCTAssertEqual(ZinqqColors.rgb(0xE4D7BE), c.onTabActive)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F0E4), c.cta)
        XCTAssertEqual(ZinqqColors.rgb(0x12100C), c.onCta)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F0E4), c.amount)
        XCTAssertEqual(ZinqqColors.rgb(0xE4D7BE), c.pill)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.onPill)
        XCTAssertEqual(ZinqqColors.rgb(0xFFFFFF), c.qrTile)
        XCTAssertEqual(ZinqqColors.rgb(0xF87171), c.danger)
        XCTAssertEqual(ZinqqColors.rgb(0xDC2626), c.dangerStrong)
        XCTAssertEqual(ZinqqColors.rgb(0xFBBF24), c.warning)
        XCTAssertEqual(ZinqqColors.rgb(0x4ADE80), c.success)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A, alpha: 0.55), c.onFieldMuted)
    }

    func testDarkOverridesMatchIndexCss() {
        let c = ZinqqColors.dark
        XCTAssertEqual(ZinqqColors.rgb(0x12100C), c.field)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F0E4), c.onField)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.fieldCta)
        XCTAssertEqual(ZinqqColors.rgb(0x231F18), c.tabActive)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F0E4), c.onTabActive)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.cta)
        XCTAssertEqual(ZinqqColors.rgb(0xFFFFFF), c.onCta)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.amount)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.pill)
        XCTAssertEqual(ZinqqColors.rgb(0xFFFFFF), c.onPill)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F0E4, alpha: 0.45), c.onFieldMuted)
        // Not overridden by the dark table: base values cascade through.
        XCTAssertEqual(ZinqqColors.rgb(0x12100C), c.dark)
        XCTAssertEqual(ZinqqColors.rgb(0xFFFFFF), c.qrTile)
        XCTAssertEqual(ZinqqColors.rgb(0xF87171), c.danger)
    }

    func testLightOverridesMatchIndexCss() {
        let c = ZinqqColors.light
        XCTAssertEqual(ZinqqColors.rgb(0xF6F1E5), c.dark)
        XCTAssertEqual(ZinqqColors.rgb(0xEFE8D8), c.darkSurface)
        XCTAssertEqual(ZinqqColors.rgb(0xFCF8F0), c.darkElevated)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.onDark)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F1E5), c.field)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.onField)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.fieldCta)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.tabActive)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F1E5), c.onTabActive)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.cta)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F1E5), c.onCta)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.amount)
        XCTAssertEqual(ZinqqColors.rgb(0xD9481F), c.pill)
        XCTAssertEqual(ZinqqColors.rgb(0xFFFFFF), c.onPill)
        XCTAssertEqual(ZinqqColors.rgb(0x1A140A), c.badge)
        XCTAssertEqual(ZinqqColors.rgb(0xF6F1E5), c.onBadge)
        XCTAssertEqual(ZinqqColors.rgb(0xFCF8F0), c.qrTile)
        XCTAssertEqual(ZinqqColors.rgb(0xB42318), c.danger)
        XCTAssertEqual(ZinqqColors.rgb(0xB45309), c.warning)
        XCTAssertEqual(ZinqqColors.rgb(0x1B7A3D), c.success)
        // dangerStrong has no light override in index.css.
        XCTAssertEqual(ZinqqColors.rgb(0xDC2626), c.dangerStrong)
    }

    func testForModeSelectsTheMatchingTable() {
        XCTAssertEqual(ZinqqColors.hybrid, ZinqqColors.forMode(.hybrid))
        XCTAssertEqual(ZinqqColors.dark, ZinqqColors.forMode(.dark))
        XCTAssertEqual(ZinqqColors.light, ZinqqColors.forMode(.light))
    }

    func testStorageValuesMatchThePwaThemeKey() {
        XCTAssertEqual("hybrid", AppearanceMode.hybrid.rawValue)
        XCTAssertEqual("dark", AppearanceMode.dark.rawValue)
        XCTAssertEqual("light", AppearanceMode.light.rawValue)
        XCTAssertEqual("theme", AppearanceMode.storageKey)
        XCTAssertEqual(AppearanceMode.hybrid, AppearanceMode.fromStorage(nil))
        XCTAssertEqual(AppearanceMode.hybrid, AppearanceMode.fromStorage("solarized"))
        XCTAssertEqual(AppearanceMode.dark, AppearanceMode.fromStorage("dark"))
    }

    func testPersistenceRoundTripsThroughUserDefaults() throws {
        let suiteName = "zinqq-test-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        XCTAssertEqual(.hybrid, AppearanceMode.loadPersisted(from: defaults))
        AppearanceMode.light.persist(to: defaults)
        XCTAssertEqual("light", defaults.string(forKey: AppearanceMode.storageKey))
        XCTAssertEqual(.light, AppearanceMode.loadPersisted(from: defaults))
    }
}
