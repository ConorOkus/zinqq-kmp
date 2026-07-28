package zinqq.main

/**
 * Shared numpad digit-entry reducer (U13, R12): the PWA's
 * `numpadDigitReducer` ported verbatim — 8-digit cap, leading-zero collapse,
 * backspace — so amount entry behaves identically on every client.
 */

/** A key on the sats numpad: a digit `'0'..'9'` or backspace. */
sealed interface NumpadKey {
    data class Digit(val digit: Char) : NumpadKey {
        init {
            require(digit in '0'..'9') { "not a digit: $digit" }
        }
    }

    data object Backspace : NumpadKey
}

/** Numpad amounts cap at 8 digits (₿99,999,999), matching the PWA. */
const val NUMPAD_MAX_DIGITS: Int = 8

/**
 * Pure reducer for numpad entry over a digit-string state.
 * Mirrors the PWA rules: backspace drops the last digit; input past
 * [maxDigits] is ignored; a leading zero never accumulates (`"0"` + `0` stays
 * `"0"`, `"0"` + `5` becomes `"5"`, `""` + `0` becomes `"0"`).
 */
fun numpadDigitReducer(
    prev: String,
    key: NumpadKey,
    maxDigits: Int = NUMPAD_MAX_DIGITS,
): String =
    when (key) {
        is NumpadKey.Backspace -> prev.dropLast(1)
        is NumpadKey.Digit -> when {
            prev.length >= maxDigits -> prev
            prev == "0" && key.digit == '0' -> prev
            prev.isEmpty() && key.digit == '0' -> "0"
            prev == "0" -> key.digit.toString()
            else -> prev + key.digit
        }
    }
