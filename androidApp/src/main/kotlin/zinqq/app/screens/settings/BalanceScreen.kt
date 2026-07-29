package zinqq.app.screens.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.WalletHolder
import zinqq.app.components.CenteredNote
import zinqq.app.theme.ZinqqTheme
import zinqq.main.formatBtc

/**
 * The PWA's Balance (U17, R12; `Balance.tsx`): the Total card with the
 * `+₿X pending` line, then the On-chain / Lightning breakdown — all derived
 * by [balanceBreakdown] from the core's split `balances()` snapshot.
 */
@Composable
fun BalanceScreen(
    holder: WalletHolder,
    onBack: (() -> Unit)?,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors

    SettingsScaffold(title = "Balance", onBack = onBack) {
        val balances = state.balances
        if (balances == null) {
            CenteredNote("Loading...")
            return@SettingsScaffold
        }
        val breakdown = balanceBreakdown(balances)
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .padding(top = 16.dp, bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            // Total card (Balance.tsx:24-32).
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(12.dp))
                    .background(colors.darkElevated)
                    .padding(16.dp),
            ) {
                Text(text = "Total", color = colors.onDarkMuted, fontSize = 14.sp)
                Text(
                    text = formatBtc(breakdown.totalSats),
                    color = colors.onDark,
                    fontFamily = ZinqqTheme.fonts.display,
                    fontWeight = FontWeight.Bold,
                    fontSize = 30.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
                if (breakdown.pendingSats > 0) {
                    Text(
                        text = "+${formatBtc(breakdown.pendingSats)} pending",
                        color = colors.onDarkMuted,
                        fontSize = 14.sp,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                }
            }
            // Breakdown rows (Balance.tsx:35-76).
            BreakdownRow(
                label = "On-chain",
                amountSats = breakdown.onchainSats,
                iconRes = R.drawable.ic_bitcoin_circle,
                iconTint = Color(0xFFFB923C), // orange-400
                iconBackground = Color(0xFFF97316).copy(alpha = 0.2f), // orange-500/20
            )
            BreakdownRow(
                label = "Lightning",
                amountSats = breakdown.lightningSats,
                iconRes = R.drawable.ic_bolt,
                iconTint = Color(0xFFFACC15), // yellow-400
                iconBackground = Color(0xFFEAB308).copy(alpha = 0.2f), // yellow-500/20
            )
        }
    }
}

@Composable
private fun BreakdownRow(
    label: String,
    amountSats: Long,
    iconRes: Int,
    iconTint: Color,
    iconBackground: Color,
) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(colors.darkElevated)
            .padding(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(36.dp)
                .clip(RoundedCornerShape(8.dp))
                .background(iconBackground),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(iconRes),
                contentDescription = null,
                tint = iconTint,
                modifier = Modifier.size(20.dp),
            )
        }
        Text(
            text = label,
            color = colors.onDark,
            fontSize = 16.sp,
            fontWeight = FontWeight.SemiBold,
            modifier = Modifier
                .weight(1f)
                .padding(start = 12.dp),
        )
        Text(
            text = formatBtc(amountSats),
            color = colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 18.sp,
        )
    }
}
