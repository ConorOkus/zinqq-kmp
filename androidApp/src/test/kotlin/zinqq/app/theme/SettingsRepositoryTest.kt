package zinqq.app.theme

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import java.io.File
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking

/**
 * Theme persistence round-trip (U13 test scenario): DataStore runs on plain
 * JVM, so this exercises the real preference file, not a fake — the same
 * `theme` / `balance-visible` keys the PWA persists (R12).
 */
class SettingsRepositoryTest {
    private fun withRepository(block: suspend (SettingsRepository) -> Unit) {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val dir = File.createTempFile("settings", null).apply { delete(); mkdirs() }
        try {
            val dataStore = PreferenceDataStoreFactory.create(scope = scope) {
                File(dir, "settings.preferences_pb")
            }
            runBlocking { block(SettingsRepository(dataStore)) }
        } finally {
            scope.cancel()
            dir.deleteRecursively()
        }
    }

    @Test
    fun appearanceModeDefaultsToHybrid() = withRepository { repo ->
        assertEquals(AppearanceMode.HYBRID, repo.appearanceMode.first())
    }

    @Test
    fun appearanceModeRoundTrips() = withRepository { repo ->
        repo.setAppearanceMode(AppearanceMode.DARK)
        assertEquals(AppearanceMode.DARK, repo.appearanceMode.first())

        repo.setAppearanceMode(AppearanceMode.LIGHT)
        assertEquals(AppearanceMode.LIGHT, repo.appearanceMode.first())
    }

    @Test
    fun blockingReadSeesThePersistedMode() = withRepository { repo ->
        repo.setAppearanceMode(AppearanceMode.DARK)
        assertEquals(AppearanceMode.DARK, repo.appearanceModeBlocking())
    }

    @Test
    fun balanceVisibleDefaultsTrueAndRoundTrips() = withRepository { repo ->
        assertTrue(repo.balanceVisible.first())

        repo.setBalanceVisible(false)
        assertFalse(repo.balanceVisible.first())

        repo.setBalanceVisible(true)
        assertTrue(repo.balanceVisible.first())
    }
}
