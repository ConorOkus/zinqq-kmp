package zinqq.spike

/**
 * BIP177 / msat formatting helpers, shared by both platform shells (U13,
 * KTD-11, R12). Pure functions with vectors ported from the PWA's
 * `format-btc.test.ts` / `msat.test.ts` so both clients render identical
 * amounts.
 */

/**
 * Format satoshis as a BIP177 `₿`-prefixed comma-grouped integer.
 * `formatBtc(0)` → `"₿0"`, `formatBtc(50_000)` → `"₿50,000"`,
 * `formatBtc(-1)` → `"-₿1"`.
 */
fun formatBtc(sats: Long): String {
    val grouped = commaGrouped(if (sats < 0) -sats else sats)
    return if (sats < 0) "-₿$grouped" else "₿$grouped"
}

/** Convert millisatoshis to satoshis using floor division (never overstates). */
fun msatToSatFloor(msat: Long): Long = msat / 1_000

/** Convert millisatoshis to satoshis using ceiling division (never understates). */
fun msatToSatCeil(msat: Long): Long = (msat + 999) / 1_000

/**
 * Format satoshis as a plain 8-decimal-place BTC string:
 * `satsToBtcString(50_000)` → `"0.00050000"`.
 */
fun satsToBtcString(sats: Long): String {
    val abs = if (sats < 0) -sats else sats
    val whole = abs / 100_000_000
    val frac = (abs % 100_000_000).toString().padStart(8, '0')
    val sign = if (sats < 0) "-" else ""
    return "$sign$whole.$frac"
}

/** Groups an absolute value into comma-separated thousands, PWA-style. */
private fun commaGrouped(abs: Long): String {
    val digits = abs.toString()
    val out = StringBuilder(digits.length + digits.length / 3)
    for ((i, ch) in digits.withIndex()) {
        if (i > 0 && (digits.length - i) % 3 == 0) out.append(',')
        out.append(ch)
    }
    return out.toString()
}
