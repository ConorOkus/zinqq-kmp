package zinqq.app.theme

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.runBlocking

// Process-wide DataStore: exactly one instance may exist per file.
private val Context.settingsDataStore: DataStore<Preferences> by preferencesDataStore(
    name = "settings",
)

/**
 * UI preferences persisted with the PWA's exact keys (U13, KTD-11, R12):
 * `theme` (appearance mode) and `balance-visible` (BalanceDisplay toggle).
 * Pure UI state — nothing here touches the wallet core (R14).
 */
class SettingsRepository(private val dataStore: DataStore<Preferences>) {
    constructor(context: Context) : this(context.settingsDataStore)

    /** Current appearance mode; unknown stored values decay to the default. */
    val appearanceMode: Flow<AppearanceMode> =
        dataStore.data.map { AppearanceMode.fromStorage(it[THEME]) }

    /**
     * Synchronous first read for pre-first-frame theme application (KTD-11:
     * persisted selection applied before render). Deliberately `runBlocking`:
     * it runs once at process start against a tiny local file, mirroring the
     * PWA's synchronous localStorage read before mount.
     */
    fun appearanceModeBlocking(): AppearanceMode = runBlocking { appearanceMode.first() }

    suspend fun setAppearanceMode(mode: AppearanceMode) {
        dataStore.edit { it[THEME] = mode.storageValue }
    }

    /** Balance visibility; defaults to visible, like the PWA. */
    val balanceVisible: Flow<Boolean> = dataStore.data.map { it[BALANCE_VISIBLE] ?: true }

    suspend fun setBalanceVisible(visible: Boolean) {
        dataStore.edit { it[BALANCE_VISIBLE] = visible }
    }

    companion object {
        /** PWA localStorage key parity: `theme`. */
        val THEME = stringPreferencesKey("theme")

        /** PWA localStorage key parity: `balance-visible`. */
        val BALANCE_VISIBLE = booleanPreferencesKey("balance-visible")
    }
}
