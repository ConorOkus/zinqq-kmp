package zinqq.spike

import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * The PWA's numpad reducer semantics (U13, R12): 8-digit cap, leading-zero
 * collapse, backspace.
 */
class NumpadReducerTest {
    @Test
    fun appendsDigits() {
        assertEquals("5", numpadDigitReducer("", NumpadKey.Digit('5')))
        assertEquals("50", numpadDigitReducer("5", NumpadKey.Digit('0')))
        assertEquals("509", numpadDigitReducer("50", NumpadKey.Digit('9')))
    }

    @Test
    fun backspaceDropsTheLastDigit() {
        assertEquals("5", numpadDigitReducer("50", NumpadKey.Backspace))
        assertEquals("", numpadDigitReducer("5", NumpadKey.Backspace))
    }

    @Test
    fun backspaceOnEmptyStaysEmpty() {
        assertEquals("", numpadDigitReducer("", NumpadKey.Backspace))
    }

    @Test
    fun capsAtEightDigits() {
        assertEquals("12345678", numpadDigitReducer("12345678", NumpadKey.Digit('9')))
    }

    @Test
    fun capIsConfigurable() {
        assertEquals("123", numpadDigitReducer("123", NumpadKey.Digit('4'), maxDigits = 3))
        assertEquals("1234", numpadDigitReducer("123", NumpadKey.Digit('4'), maxDigits = 4))
    }

    @Test
    fun backspaceStillWorksAtTheCap() {
        assertEquals("1234567", numpadDigitReducer("12345678", NumpadKey.Backspace))
    }

    @Test
    fun leadingZeroNeverAccumulates() {
        assertEquals("0", numpadDigitReducer("", NumpadKey.Digit('0')))
        assertEquals("0", numpadDigitReducer("0", NumpadKey.Digit('0')))
    }

    @Test
    fun leadingZeroCollapsesToTheNextDigit() {
        assertEquals("5", numpadDigitReducer("0", NumpadKey.Digit('5')))
    }
}
