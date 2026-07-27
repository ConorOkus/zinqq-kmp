import Shared
import XCTest

@testable import iosApp

/// The Peers screen's pure derivations (U22, R10 UI): the PWA's
/// `parsePeerAddress` matrix (`peer-connection.ts:149-174`), the header count
/// label (`Peers.tsx:199`), per-peer presentation (`Peers.tsx:219-248`), and
/// the nested channel rows (`Peers.tsx:250-305`) — Android's `PeersLogicTest`
/// ported fixture-for-fixture.
final class PeersLogicTests: XCTestCase {

    // MARK: connect-input parse matrix (PWA copy verbatim)

    func testWellFormedAddressParses() {
        let parsed = parsePeerAddress("\(peerPubkey)@203.0.113.9:9735")
        XCTAssertEqual(
            .valid(pubkey: peerPubkey, host: "203.0.113.9", port: 9735), parsed
        )
    }

    func testMissingAtIsRejected() {
        XCTAssertEqual(
            .invalid(message: "Invalid peer address: expected pubkey@host:port"),
            parsePeerAddress("203.0.113.9:9735")
        )
    }

    func testMissingPortIsRejected() {
        XCTAssertEqual(
            .invalid(message: "Invalid peer address: expected host:port after @"),
            parsePeerAddress("\(peerPubkey)@203.0.113.9")
        )
    }

    func testOutOfRangePortIsRejected() {
        XCTAssertEqual(
            .invalid(message: "Invalid peer address: port must be a number between 1 and 65535"),
            parsePeerAddress("\(peerPubkey)@203.0.113.9:70000")
        )
    }

    func testMalformedPubkeyIsRejected() {
        XCTAssertEqual(
            .invalid(message: "Invalid peer address: pubkey must be 66 lowercase hex characters"),
            parsePeerAddress("02ABCD@203.0.113.9:9735")
        )
    }

    func testHostWithForbiddenCharactersIsRejected() {
        XCTAssertEqual(
            .invalid(
                message: "Invalid peer address: host must contain only alphanumeric, dot, "
                    + "hyphen, or underscore"
            ),
            parsePeerAddress("\(peerPubkey)@bad host!:9735")
        )
    }

    // MARK: header counts (Peers.tsx:199)

    func testCountLabelCountsConnectedAndTotal() {
        let peers = [
            peerView(connected: true),
            peerView(pubkey: "03" + String(repeating: "cd", count: 32), connected: false),
            peerView(
                pubkey: "02" + String(repeating: "ef", count: 32),
                connected: true,
                known: false
            ),
        ]
        XCTAssertEqual("Peers (2 connected, 3 saved)", peersCountLabel(peers))
        XCTAssertEqual("Peers (0 connected, 0 saved)", peersCountLabel([]))
    }

    // MARK: per-peer presentation

    func testPeerIdIsMidTruncatedLikeThePwa() {
        XCTAssertEqual(
            String(peerPubkey.prefix(16)) + "..." + String(peerPubkey.suffix(8)),
            peerDisplayId(peerPubkey)
        )
    }

    func testStatusLabelFollowsTheConnectionDot() {
        XCTAssertEqual("Connected", peerStatusLabel(connected: true))
        XCTAssertEqual("Offline", peerStatusLabel(connected: false))
    }

    func testForgetShowsOnlyForKnownPeersAndDisablesWithOpenChannels() {
        XCTAssertTrue(showsForget(peerView(known: true)))
        XCTAssertFalse(showsForget(peerView(known: false)))
        XCTAssertTrue(forgetEnabled(peerView(channelCount: 0)))
        XCTAssertFalse(forgetEnabled(peerView(channelCount: 2)))
    }

    func testForgetWithOpenChannelsMapsToThePwaCopy() {
        XCTAssertEqual(
            "Cannot forget peer with open channels",
            forgetErrorMessage(WalletException.PeerHasOpenChannels())
        )
        XCTAssertEqual("weird", forgetErrorMessage(KotlinRuntimeException(message: "weird")))
    }

    // MARK: nested channel rows (Peers.tsx:255-305)

    func testChannelStateTextMatchesThePwaLabels() {
        XCTAssertEqual("Active", channelStateText(.active))
        XCTAssertEqual("Ready", channelStateText(.ready))
        XCTAssertEqual("Pending", channelStateText(.pending))
        XCTAssertEqual("Closing…", channelStateText(.closing))
    }

    func testCloseActionEscalatesToForceCloseWhileShuttingDown() {
        XCTAssertEqual("Close", channelCloseActionLabel(.active))
        XCTAssertEqual("Close", channelCloseActionLabel(.pending))
        XCTAssertEqual("Force Close", channelCloseActionLabel(.closing))
    }

    func testChannelCapacityAndBalancesRenderInSats() {
        let channel = channelView(
            capacitySats: 100_000,
            outboundMsat: 60_000_999,
            inboundMsat: 39_000_001,
            reserveSats: 1_000
        )
        XCTAssertEqual("₿100,000 capacity", channelCapacityText(channel))
        XCTAssertEqual("Send: ₿60,000", channelSendText(channel))
        XCTAssertEqual("Receive: ₿39,000", channelReceiveText(channel))
        XCTAssertEqual("Reserve: ₿1,000", channelReserveText(channel))
        XCTAssertNil(channelReserveText(channelView(reserveSats: nil)))
    }

    func testChannelsGroupByCounterparty() {
        let a = channelView(
            channelId: String(repeating: "aa", count: 32), counterpartyPubkey: peerPubkey
        )
        let b = channelView(
            channelId: String(repeating: "bb", count: 32), counterpartyPubkey: peerPubkey
        )
        let otherPubkey = "03" + String(repeating: "cd", count: 32)
        let other = channelView(
            channelId: String(repeating: "dd", count: 32), counterpartyPubkey: otherPubkey
        )
        let grouped = channelsByPeer([a, other, b])
        XCTAssertEqual([a, b], grouped[peerPubkey])
        XCTAssertEqual([other], grouped[otherPubkey])
    }
}
