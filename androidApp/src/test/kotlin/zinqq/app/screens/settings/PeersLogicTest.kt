package zinqq.app.screens.settings

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue
import uniffi.wallet_core.ChannelStateLabel
import uniffi.wallet_core.WalletException

/**
 * The Peers screen's pure derivations (U17, R10 UI): the PWA's
 * `parsePeerAddress` matrix (`peer-connection.ts:149-174`), the header count
 * label (`Peers.tsx:199`), per-peer presentation (`Peers.tsx:219-248`), and
 * the nested channel rows (`Peers.tsx:250-305`).
 */
class PeersLogicTest {

    // --- connect-input parse matrix (PWA copy verbatim) ---

    @Test
    fun wellFormedAddressParses() {
        val parsed = parsePeerAddress("$PEER_PUBKEY@203.0.113.9:9735")
        assertIs<PeerAddressParse.Valid>(parsed)
        assertEquals(PEER_PUBKEY, parsed.pubkey)
        assertEquals("203.0.113.9", parsed.host)
        assertEquals(9735, parsed.port)
    }

    @Test
    fun missingAtIsRejected() {
        val parsed = parsePeerAddress("203.0.113.9:9735")
        assertIs<PeerAddressParse.Invalid>(parsed)
        assertEquals("Invalid peer address: expected pubkey@host:port", parsed.message)
    }

    @Test
    fun missingPortIsRejected() {
        val parsed = parsePeerAddress("$PEER_PUBKEY@203.0.113.9")
        assertIs<PeerAddressParse.Invalid>(parsed)
        assertEquals("Invalid peer address: expected host:port after @", parsed.message)
    }

    @Test
    fun outOfRangePortIsRejected() {
        val parsed = parsePeerAddress("$PEER_PUBKEY@203.0.113.9:70000")
        assertIs<PeerAddressParse.Invalid>(parsed)
        assertEquals(
            "Invalid peer address: port must be a number between 1 and 65535",
            parsed.message,
        )
    }

    @Test
    fun malformedPubkeyIsRejected() {
        val parsed = parsePeerAddress("02ABCD@203.0.113.9:9735")
        assertIs<PeerAddressParse.Invalid>(parsed)
        assertEquals(
            "Invalid peer address: pubkey must be 66 lowercase hex characters",
            parsed.message,
        )
    }

    @Test
    fun hostWithForbiddenCharactersIsRejected() {
        val parsed = parsePeerAddress("$PEER_PUBKEY@bad host!:9735")
        assertIs<PeerAddressParse.Invalid>(parsed)
        assertEquals(
            "Invalid peer address: host must contain only alphanumeric, dot, hyphen, or underscore",
            parsed.message,
        )
    }

    // --- header counts (Peers.tsx:199) ---

    @Test
    fun countLabelCountsConnectedAndTotal() {
        val peers = listOf(
            peerView(connected = true),
            peerView(pubkey = "03" + "cd".repeat(32), connected = false),
            peerView(pubkey = "02" + "ef".repeat(32), connected = true, known = false),
        )
        assertEquals("Peers (2 connected, 3 saved)", peersCountLabel(peers))
        assertEquals("Peers (0 connected, 0 saved)", peersCountLabel(emptyList()))
    }

    // --- per-peer presentation ---

    @Test
    fun peerIdIsMidTruncatedLikeThePwa() {
        assertEquals(
            PEER_PUBKEY.take(16) + "..." + PEER_PUBKEY.takeLast(8),
            peerDisplayId(PEER_PUBKEY),
        )
    }

    @Test
    fun statusLabelFollowsTheConnectionDot() {
        assertEquals("Connected", peerStatusLabel(connected = true))
        assertEquals("Offline", peerStatusLabel(connected = false))
    }

    @Test
    fun forgetShowsOnlyForKnownPeersAndDisablesWithOpenChannels() {
        assertTrue(showsForget(peerView(known = true)))
        assertFalse(showsForget(peerView(known = false)))
        assertTrue(forgetEnabled(peerView(channelCount = 0u)))
        assertFalse(forgetEnabled(peerView(channelCount = 2u)))
    }

    @Test
    fun forgetWithOpenChannelsMapsToThePwaCopy() {
        assertEquals(
            "Cannot forget peer with open channels",
            forgetErrorMessage(WalletException.PeerHasOpenChannels()),
        )
        assertEquals("weird", forgetErrorMessage(RuntimeException("weird")))
    }

    // --- nested channel rows (Peers.tsx:255-305) ---

    @Test
    fun channelStateTextMatchesThePwaLabels() {
        assertEquals("Active", channelStateText(ChannelStateLabel.ACTIVE))
        assertEquals("Ready", channelStateText(ChannelStateLabel.READY))
        assertEquals("Pending", channelStateText(ChannelStateLabel.PENDING))
        assertEquals("Closing…", channelStateText(ChannelStateLabel.CLOSING))
    }

    @Test
    fun closeActionEscalatesToForceCloseWhileShuttingDown() {
        assertEquals("Close", channelCloseActionLabel(ChannelStateLabel.ACTIVE))
        assertEquals("Close", channelCloseActionLabel(ChannelStateLabel.PENDING))
        assertEquals("Force Close", channelCloseActionLabel(ChannelStateLabel.CLOSING))
    }

    @Test
    fun channelCapacityAndBalancesRenderInSats() {
        val channel = channelView(
            capacitySats = 100_000uL,
            outboundMsat = 60_000_999uL,
            inboundMsat = 39_000_001uL,
            reserveSats = 1_000uL,
        )
        assertEquals("₿100,000 capacity", channelCapacityText(channel))
        assertEquals("Send: ₿60,000", channelSendText(channel))
        assertEquals("Receive: ₿39,000", channelReceiveText(channel))
        assertEquals("Reserve: ₿1,000", channelReserveText(channel))
        assertNull(channelReserveText(channelView(reserveSats = null)))
    }

    @Test
    fun channelsGroupByCounterparty() {
        val a = channelView(channelId = "aa".repeat(32), counterpartyPubkey = PEER_PUBKEY)
        val b = channelView(channelId = "bb".repeat(32), counterpartyPubkey = PEER_PUBKEY)
        val other = channelView(
            channelId = "dd".repeat(32),
            counterpartyPubkey = "03" + "cd".repeat(32),
        )
        val grouped = channelsByPeer(listOf(a, other, b))
        assertEquals(listOf(a, b), grouped[PEER_PUBKEY])
        assertEquals(listOf(other), grouped["03" + "cd".repeat(32)])
    }
}
