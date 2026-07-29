package zinqq.main

import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * Vectors ported from the PWA's `format-btc.test.ts` and `msat.test.ts`
 * (U13, R12): identical inputs must render identically on both clients.
 */
class FormatTest {
    @Test
    fun formatsZero() {
        assertEquals("₿0", formatBtc(0))
    }

    @Test
    fun formatsSmallAmountsWithoutCommas() {
        assertEquals("₿1", formatBtc(1))
        assertEquals("₿999", formatBtc(999))
    }

    @Test
    fun formatsAmountsWithCommaSeparation() {
        assertEquals("₿1,000", formatBtc(1_000))
        assertEquals("₿50,000", formatBtc(50_000))
        assertEquals("₿1,234,567", formatBtc(1_234_567))
        assertEquals("₿100,000,000", formatBtc(100_000_000))
    }

    @Test
    fun handlesLargeValues() {
        assertEquals("₿2,100,000,000,000,000", formatBtc(2_100_000_000_000_000))
    }

    @Test
    fun handlesNegativeAmounts() {
        assertEquals("-₿50,000", formatBtc(-50_000))
        assertEquals("-₿1", formatBtc(-1))
    }

    @Test
    fun floorConvertsExactMultiples() {
        assertEquals(5, msatToSatFloor(5_000))
        assertEquals(1, msatToSatFloor(1_000))
        assertEquals(0, msatToSatFloor(0))
    }

    @Test
    fun floorDropsSubSatRemainders() {
        assertEquals(1, msatToSatFloor(1_999))
        assertEquals(1, msatToSatFloor(1_001))
        assertEquals(0, msatToSatFloor(999))
        assertEquals(0, msatToSatFloor(1))
    }

    @Test
    fun floorHandlesLargeValues() {
        assertEquals(100_000_000, msatToSatFloor(100_000_000_000))
        assertEquals(100_000_000, msatToSatFloor(100_000_000_999))
    }

    @Test
    fun ceilConvertsExactMultiples() {
        assertEquals(5, msatToSatCeil(5_000))
        assertEquals(1, msatToSatCeil(1_000))
        assertEquals(0, msatToSatCeil(0))
    }

    @Test
    fun ceilRoundsUpSubSatRemainders() {
        assertEquals(2, msatToSatCeil(1_999))
        assertEquals(2, msatToSatCeil(1_001))
        assertEquals(1, msatToSatCeil(999))
        assertEquals(1, msatToSatCeil(1))
    }

    @Test
    fun ceilHandlesLargeValues() {
        assertEquals(100_000_000, msatToSatCeil(100_000_000_000))
        assertEquals(100_000_001, msatToSatCeil(100_000_000_001))
    }

    @Test
    fun satsToBtcStringUsesEightDecimalPlaces() {
        assertEquals("0.00000000", satsToBtcString(0))
        assertEquals("0.00050000", satsToBtcString(50_000))
        assertEquals("1.00000000", satsToBtcString(100_000_000))
        assertEquals("1.23456789", satsToBtcString(123_456_789))
        assertEquals("21000000.00000000", satsToBtcString(2_100_000_000_000_000))
        assertEquals("-0.00000001", satsToBtcString(-1))
    }
}
