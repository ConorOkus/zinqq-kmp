package zinqq.app.screens.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.delay
import zinqq.app.R
import zinqq.app.WalletHolder
import zinqq.app.nav.Route
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Advanced (U17, R12; `Advanced.tsx`): the Node ID copy card with
 * the 2,000 ms "Copied!" flash, then the Balance and Peers rows. The node id
 * is cached on [zinqq.app.UiState] (queried per refresh — `node_id()` needs
 * a running node); like the PWA's not-ready gate, the card simply doesn't
 * render before the first successful start.
 */
@Composable
fun AdvancedScreen(
    holder: WalletHolder,
    onBack: (() -> Unit)?,
    onOpenRow: (Route) -> Unit,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors
    val clipboard = LocalClipboardManager.current
    var copied by remember { mutableStateOf(false) }

    // The PWA's 2,000 ms copied flash (Advanced.tsx:56-62).
    LaunchedEffect(copied) {
        if (copied) {
            delay(2_000)
            copied = false
        }
    }

    SettingsScaffold(title = "Advanced", onBack = onBack) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
        ) {
            state.nodeId?.let { nodeId ->
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(bottom = 16.dp)
                        .clip(RoundedCornerShape(12.dp))
                        .background(colors.darkElevated)
                        .clickable {
                            clipboard.setText(AnnotatedString(nodeId))
                            copied = true
                        }
                        .padding(16.dp),
                ) {
                    Text(
                        text = "Node ID",
                        color = colors.onDarkMuted,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                    )
                    Text(
                        text = nodeId,
                        color = colors.onDark,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 12.sp,
                        lineHeight = 18.sp,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                    Text(
                        text = if (copied) "Copied!" else "Tap to copy",
                        color = colors.onDarkMuted,
                        fontSize = 12.sp,
                        modifier = Modifier.padding(top = 8.dp),
                    )
                }
            }
            ADVANCED_ROWS.forEach { row ->
                SettingsRowItem(
                    row = row,
                    iconRes = if (row.label == "Balance") {
                        R.drawable.ic_card_plus
                    } else {
                        R.drawable.ic_users
                    },
                    onClick = row.destination?.let { { onOpenRow(it) } },
                )
            }
        }
    }
}
