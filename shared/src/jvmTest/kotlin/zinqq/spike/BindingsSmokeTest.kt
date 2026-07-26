package zinqq.spike

import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class BindingsSmokeTest {
    @Test
    fun coreVersionCrossesTheFfiBoundary() {
        val version = WalletCore.coreVersion()
        assertTrue(
            version.startsWith("wallet-core 0.1.0"),
            "unexpected version string from Rust: $version",
        )
    }

    @Test
    fun pingAsyncCompletesViaSuspendBinding() = runBlocking {
        assertEquals("pong", WalletCore.pingAsync())
    }
}
