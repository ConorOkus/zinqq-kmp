package zinqq.app

import java.time.ZoneOffset
import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * en-GB date/time stability, pinned to what the PWA's
 * `Intl.DateTimeFormat('en-GB', …)` produces (verified against V8):
 * - TransactionDetail (`TransactionDetail.tsx:9-27`): "Sun, 26 July 2026" /
 *   "14:05:09", both "Pending" for the zero sentinel.
 * - ChannelCloseDetail (`ChannelCloseDetail.tsx:32-40`):
 *   "26 July 2026 at 14:05".
 * Tests pass UTC explicitly; screens use the device zone like the PWA.
 */
class DateFormatTest {
    // 2026-07-26T14:05:09Z
    private val ts = 1_785_074_709_000L

    @Test
    fun transactionDetailDate() {
        assertEquals("Sun, 26 July 2026", formatDetailDate(ts, ZoneOffset.UTC))
    }

    @Test
    fun transactionDetailTimeIs24HourWithSeconds() {
        assertEquals("14:05:09", formatDetailTime(ts, ZoneOffset.UTC))
    }

    @Test
    fun zeroTimestampRendersPending() {
        assertEquals("Pending", formatDetailDate(0L, ZoneOffset.UTC))
        assertEquals("Pending", formatDetailTime(0L, ZoneOffset.UTC))
    }

    @Test
    fun closeDetailDateIncludesTheTime() {
        assertEquals("26 July 2026 at 14:05", formatCloseDate(ts, ZoneOffset.UTC))
    }

    @Test
    fun closeDetailDateZeroPadsAndDropsLeadingDayZero() {
        // 2026-01-05T09:03:00Z — single-digit day stays bare, time zero-pads.
        assertEquals("5 January 2026 at 09:03", formatCloseDate(1_767_603_780_000L, ZoneOffset.UTC))
    }
}
