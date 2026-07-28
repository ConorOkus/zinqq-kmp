package zinqq.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import zinqq.app.nav.ZinqqApp
import zinqq.app.theme.ZinqqTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // The holder is process-scoped, so this activity only reads it — it
        // neither creates nor stops the node (see WalletHolder). The holder
        // read the persisted appearance mode synchronously at construction,
        // so the very first frame is already themed (KTD-11).
        val holder = (application as ZinqqApplication).walletHolder
        setContent {
            val state by holder.state.collectAsState()
            ZinqqTheme(mode = state.appearanceMode) {
                ZinqqApp(
                    holder = holder,
                    // Fenced-screen "Quit": the other client stays the active
                    // one; leave the task entirely rather than back-stacking.
                    onQuit = { finishAndRemoveTask() },
                )
            }
        }
    }
}
