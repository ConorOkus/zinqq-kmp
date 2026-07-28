package zinqq.app

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue
import uniffi.wallet_core.ActivityDirection
import uniffi.wallet_core.ActivityKind
import uniffi.wallet_core.ActivityStatus
import uniffi.wallet_core.CloseStatusLabel

/**
 * Row-derivation matrix for the Activity list, mirroring the PWA's
 * `Activity.tsx`: titles, Pending/close-status badges (`CLOSE_BADGES`,
 * lines 7-13), signed `formatBtc` amounts with msat floored for Lightning
 * rows, the em-dash for unknown close amounts, muted styling while pending,
 * and the `⚡` glyph on Lightning and close rows.
 */
class ActivityRowPresentationTest {
    @Test
    fun titlesFollowDirectionAndKind() {
        assertEquals("Sent", activityTitle(activityRow(direction = ActivityDirection.SENT)))
        assertEquals("Received", activityTitle(activityRow(direction = ActivityDirection.RECEIVED)))
        assertEquals(
            "Channel close",
            activityTitle(activityRow(kind = ActivityKind.CHANNEL_CLOSE, direction = null)),
        )
    }

    @Test
    fun badgeIsPendingForNonCloseRows() {
        assertEquals("Pending", activityBadge(activityRow(status = ActivityStatus.PENDING)))
        assertNull(activityBadge(activityRow(status = ActivityStatus.CONFIRMED)))
        assertEquals(
            "Pending",
            activityBadge(
                activityRow(kind = ActivityKind.ONCHAIN, status = ActivityStatus.PENDING),
            ),
        )
    }

    @Test
    fun badgeMapsCloseStatusesLikeThePwaCloseBadgesTable() {
        fun closeRow(status: CloseStatusLabel) = activityRow(
            kind = ActivityKind.CHANNEL_CLOSE,
            status = ActivityStatus.PENDING,
            closeStatus = status,
        )
        assertEquals("Closing", activityBadge(closeRow(CloseStatusLabel.CLOSING)))
        assertEquals("Waiting timelock", activityBadge(closeRow(CloseStatusLabel.WAITING_TIMELOCK)))
        assertEquals("Returning to wallet", activityBadge(closeRow(CloseStatusLabel.RETURNING)))
        assertNull(activityBadge(closeRow(CloseStatusLabel.COMPLETE)))
        assertEquals("Resolved", activityBadge(closeRow(CloseStatusLabel.RESOLVED_UNVERIFIED)))
    }

    @Test
    fun lightningAmountsFloorMsatAndSignByDirection() {
        assertEquals(
            "-₿250",
            activityAmountText(
                activityRow(direction = ActivityDirection.SENT, amountMsat = 250_999uL),
            ),
        )
        assertEquals(
            "+₿1,234",
            activityAmountText(
                activityRow(direction = ActivityDirection.RECEIVED, amountMsat = 1_234_000uL),
            ),
        )
    }

    @Test
    fun onchainAmountsUseSatsDirectly() {
        assertEquals(
            "+₿2,000",
            activityAmountText(
                activityRow(
                    kind = ActivityKind.ONCHAIN,
                    direction = ActivityDirection.RECEIVED,
                    amountSats = 2_000uL,
                ),
            ),
        )
    }

    @Test
    fun closeRowsRenderPlusAmountOrEmDashWhenUnknown() {
        assertEquals(
            "+₿5,000",
            activityAmountText(
                activityRow(
                    kind = ActivityKind.CHANNEL_CLOSE,
                    direction = null,
                    amountSats = 5_000uL,
                ),
            ),
        )
        assertEquals(
            "—",
            activityAmountText(
                activityRow(
                    kind = ActivityKind.CHANNEL_CLOSE,
                    direction = null,
                    amountSats = null,
                ),
            ),
        )
    }

    @Test
    fun pendingRowsAreMuted() {
        assertTrue(isAmountMuted(activityRow(status = ActivityStatus.PENDING)))
        assertFalse(isAmountMuted(activityRow(status = ActivityStatus.CONFIRMED)))
    }

    @Test
    fun lightningGlyphShowsOnLightningAndCloseRowsOnly() {
        assertTrue(showsLightningGlyph(activityRow(kind = ActivityKind.LIGHTNING)))
        assertTrue(showsLightningGlyph(activityRow(kind = ActivityKind.CHANNEL_CLOSE)))
        assertFalse(showsLightningGlyph(activityRow(kind = ActivityKind.ONCHAIN)))
    }
}
