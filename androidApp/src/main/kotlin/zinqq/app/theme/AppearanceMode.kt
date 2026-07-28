package zinqq.app.theme

/**
 * The PWA's three appearance modes (U13, KTD-11, R12), persisted under the
 * same `theme` key with the same string values (`hybrid`/`dark`/`light`) as
 * the PWA's localStorage entry — parity in behavior and vocabulary.
 *
 * - HYBRID (default): bone accent floods Home/Activity/tab bar; Send /
 *   Receive / Settings stay warm near-black.
 * - DARK: warm near-black everywhere, ember on the action.
 * - LIGHT: warm paper everywhere, ember on the action.
 */
enum class AppearanceMode(val storageValue: String) {
    HYBRID("hybrid"),
    DARK("dark"),
    LIGHT("light"),
    ;

    companion object {
        val DEFAULT = HYBRID

        /** Unknown/absent stored values fall back to the default, like the PWA. */
        fun fromStorage(value: String?): AppearanceMode =
            entries.firstOrNull { it.storageValue == value } ?: DEFAULT
    }
}
