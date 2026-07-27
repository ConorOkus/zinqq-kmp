package zinqq.app

import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * Vectors for the close-detail countdown's block humanizer, mirroring the
 * PWA's `humanizeBlocks` (`close-records/estimate.ts:60-66`): 10 minutes a
 * block, minutes under an hour, rounded hours under 48, rounded days after.
 */
class HumanizeBlocksTest {
    @Test
    fun underAnHourRendersMinutes() {
        assertEquals("~10 minutes", humanizeBlocks(1))
        assertEquals("~50 minutes", humanizeBlocks(5))
    }

    @Test
    fun underTwoDaysRendersRoundedHours() {
        assertEquals("~1 hour", humanizeBlocks(6))
        assertEquals("~2 hours", humanizeBlocks(12))
        // 144 blocks is a day of blocks but still under the 48h switch.
        assertEquals("~24 hours", humanizeBlocks(144))
        assertEquals("~47 hours", humanizeBlocks(283))
    }

    @Test
    fun twoDaysAndUpRendersRoundedDays() {
        assertEquals("~2 days", humanizeBlocks(288))
        // The canonical force-close timelock: 2016 blocks ≈ 14 days.
        assertEquals("~14 days", humanizeBlocks(2016))
    }
}
