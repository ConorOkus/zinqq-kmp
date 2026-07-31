import Shared
import XCTest

@testable import iosApp

/// The configuration → network mapping (U7, R2). The Android twin of these
/// assertions lives in `BuildNetworkTest.kt`; both stand in for the guarantee
/// TestFlight rests on — a shipped binary is on mainnet.
final class BuildNetworkTests: XCTestCase {

    func testDebugConfigurationTargetsMutinynet() {
        XCTAssertEqual(.mutinynet, walletNetworkFor("mutinynet"))
    }

    func testReleaseConfigurationTargetsMainnet() {
        XCTAssertEqual(.mainnet, walletNetworkFor("mainnet"))
    }

    /// A missing key means the Info.plist substitution did not happen. That is
    /// a broken build, and it must fail toward mainnet rather than toward a
    /// test network.
    func testAMissingValueFallsBackToMainnet() {
        XCTAssertEqual(.mainnet, walletNetworkFor(nil))
        XCTAssertEqual(.mainnet, walletNetworkFor(""))
    }

    /// Notably including an unsubstituted `$(WALLET_NETWORK)`, which is what a
    /// misconfigured build setting would leave in the plist.
    func testAnUnrecognizedValueFallsBackToMainnet() {
        XCTAssertEqual(.mainnet, walletNetworkFor("$(WALLET_NETWORK)"))
        XCTAssertEqual(.mainnet, walletNetworkFor("regtest"))
    }

    func testTheMappingIsCaseInsensitive() {
        XCTAssertEqual(.mutinynet, walletNetworkFor("MutinyNet"))
        XCTAssertEqual(.mainnet, walletNetworkFor("MAINNET"))
    }

    /// The bundle this test host was built as is Debug, so the end-to-end
    /// plist wiring should resolve Mutinynet. This is the one assertion that
    /// proves the build setting actually reaches Swift.
    func testTheBundleValueResolvesForThisBuild() {
        XCTAssertEqual(.mutinynet, buildWalletNetwork())
    }
}
