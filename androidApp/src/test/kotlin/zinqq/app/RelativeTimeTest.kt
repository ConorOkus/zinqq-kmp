package zinqq.app

import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * Vectors for the Activity list's relative-time buckets, mirroring the PWA's
 * `formatRelativeTime` (`Activity.tsx:15-27`): Just now under a minute, then
 * floor-divided m/h/d/w buckets, and empty for the zero sentinel timestamp.
 */
class RelativeTimeTest {
    private val now = 1_753_500_000_000L

    private fun at(secondsAgo: Long) = formatRelativeTime(now - secondsAgo * 1_000, now)

    @Test
    fun zeroTimestampRendersEmpty() {
        assertEquals("", formatRelativeTime(0L, now))
    }

    @Test
    fun underAMinuteIsJustNow() {
        assertEquals("Just now", at(0))
        assertEquals("Just now", at(5))
        assertEquals("Just now", at(59))
    }

    @Test
    fun minuteBucketUpToAnHour() {
        assertEquals("1m ago", at(60))
        assertEquals("5m ago", at(5 * 60))
        assertEquals("59m ago", at(59 * 60 + 59))
    }

    @Test
    fun hourBucketUpToADay() {
        assertEquals("1h ago", at(60 * 60))
        assertEquals("3h ago", at(3 * 60 * 60))
        assertEquals("23h ago", at(23 * 60 * 60 + 59 * 60))
    }

    @Test
    fun dayBucketUpToAWeek() {
        assertEquals("1d ago", at(24 * 60 * 60))
        assertEquals("2d ago", at(2 * 24 * 60 * 60))
        assertEquals("6d ago", at(6 * 24 * 60 * 60 + 23 * 60 * 60))
    }

    @Test
    fun weekBucketIsOpenEnded() {
        assertEquals("1w ago", at(7 * 24 * 60 * 60))
        assertEquals("3w ago", at(3 * 7 * 24 * 60 * 60))
        assertEquals("52w ago", at(364 * 24 * 60 * 60))
    }
}
