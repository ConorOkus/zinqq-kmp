package zinqq.app.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.wallet_core.ActivityDirection
import uniffi.wallet_core.ActivityKind
import uniffi.wallet_core.ActivityRow
import zinqq.app.R
import zinqq.app.WalletHolder
import zinqq.app.activityAmountText
import zinqq.app.activityBadge
import zinqq.app.activityTitle
import zinqq.app.components.CenteredNote
import zinqq.app.formatRelativeTime
import zinqq.app.isAmountMuted
import zinqq.app.showsLightningGlyph
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Activity page (U14, R11; `Activity.tsx`): the merged feed from
 * `list_activity()` (failed Lightning rows already filtered by the core,
 * KTD-7) as direction-icon rows with Pending/close-status badges, relative
 * times, and signed amounts. Field screen; the TabBar is shell-owned (U13).
 */
@Composable
fun ActivityScreen(
    holder: WalletHolder,
    onOpenTx: (String) -> Unit,
    onOpenClose: (String) -> Unit,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors
    val transactions = state.activity
    // One render-time clock for every row, like the PWA's per-render Date.now().
    val nowMs = remember(transactions) { System.currentTimeMillis() }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.field)
            // No ScreenHeader on this tab, so the title needs the status-bar
            // inset itself (targetSdk 35 is edge-to-edge).
            .statusBarsPadding()
            .padding(top = 24.dp),
    ) {
        Text(
            text = "Activity",
            color = colors.onField,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 30.sp,
            modifier = Modifier.padding(horizontal = 24.dp, vertical = 0.dp),
        )

        when {
            transactions == null -> CenteredNote("Loading...", color = colors.onFieldMuted)
            transactions.isEmpty() -> CenteredNote("No transactions yet", color = colors.onFieldMuted)
            else -> LazyColumn(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(top = 24.dp),
            ) {
                items(transactions, key = { it.id }) { row ->
                    ActivityRowItem(
                        row = row,
                        nowMs = nowMs,
                        onClick = {
                            val channelId = row.channelId
                            if (row.kind == ActivityKind.CHANNEL_CLOSE && channelId != null) {
                                onOpenClose(channelId)
                            } else {
                                onOpenTx(row.id)
                            }
                        },
                    )
                }
            }
        }
    }
}

@Composable
private fun ActivityRowItem(
    row: ActivityRow,
    nowMs: Long,
    onClick: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 24.dp, vertical = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier.size(36.dp),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(
                    if (row.direction == ActivityDirection.SENT) {
                        R.drawable.ic_arrow_up_right
                    } else {
                        R.drawable.ic_arrow_down_left
                    },
                ),
                contentDescription = null,
                tint = colors.onField,
                modifier = Modifier.size(20.dp),
            )
        }
        Column(
            modifier = Modifier
                .weight(1f)
                .padding(start = 16.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    text = activityTitle(row),
                    color = colors.onField,
                    fontWeight = FontWeight.SemiBold,
                    fontSize = 16.sp,
                )
                activityBadge(row)?.let { badge ->
                    Text(
                        text = badge,
                        color = colors.onFieldMuted,
                        fontSize = 12.sp,
                        modifier = Modifier.padding(start = 8.dp),
                    )
                }
            }
            Text(
                text = buildString {
                    if (showsLightningGlyph(row)) append("⚡ ")
                    append(formatRelativeTime(row.createdAtMs.toLong(), nowMs))
                },
                color = colors.onFieldMuted,
                fontSize = 12.sp,
                modifier = Modifier.padding(top = 2.dp),
            )
        }
        Text(
            text = activityAmountText(row),
            color = if (isAmountMuted(row)) colors.onFieldMuted else colors.onField,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 16.sp,
        )
    }
}
