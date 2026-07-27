package zinqq.app.screens.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.WalletHolder
import zinqq.app.nav.Route
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Settings (U17, R12; `Settings.tsx`): the five icon rows —
 * How It Works and Get Help preserved as inert no-ops — and the Appearance
 * three-way radiogroup persisted through [WalletHolder.setAppearanceMode].
 */
@Composable
fun SettingsScreen(
    holder: WalletHolder,
    onBack: (() -> Unit)?,
    onOpenRow: (Route) -> Unit,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors

    SettingsScaffold(title = "Settings", onBack = onBack) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
        ) {
            SETTINGS_ROWS.forEach { row ->
                SettingsRowItem(
                    row = row,
                    iconRes = settingsRowIcon(row.label),
                    onClick = row.destination?.let { { onOpenRow(it) } },
                )
            }

            // Appearance radiogroup (Settings.tsx:138-161).
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 8.dp)
                    .padding(top = 24.dp, bottom = 16.dp),
            ) {
                Text(
                    text = "Appearance",
                    color = colors.onDark,
                    fontSize = 16.sp,
                    fontWeight = FontWeight.SemiBold,
                )
                Row(
                    modifier = Modifier
                        .padding(top = 12.dp)
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(12.dp))
                        .background(colors.darkElevated)
                        .padding(4.dp)
                        .selectableGroup(),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    APPEARANCE_MODES.forEach { mode ->
                        val selected = state.appearanceMode == mode
                        Box(
                            modifier = Modifier
                                .weight(1f)
                                .height(40.dp)
                                .clip(RoundedCornerShape(8.dp))
                                .background(if (selected) colors.onDark else colors.darkElevated)
                                .selectable(
                                    selected = selected,
                                    role = Role.RadioButton,
                                    onClick = { holder.setAppearanceMode(mode) },
                                ),
                            contentAlignment = Alignment.Center,
                        ) {
                            Text(
                                text = appearanceLabel(mode),
                                color = if (selected) colors.dark else colors.onDarkMuted,
                                fontSize = 14.sp,
                                fontWeight = FontWeight.SemiBold,
                            )
                        }
                    }
                }
            }
        }
    }
}

private fun settingsRowIcon(label: String): Int = when (label) {
    "Wallet Backup" -> R.drawable.ic_lock
    "Recover Wallet" -> R.drawable.ic_restore_arc
    "Advanced" -> R.drawable.ic_gear
    "How It Works" -> R.drawable.ic_help_circle
    else -> R.drawable.ic_chat
}
