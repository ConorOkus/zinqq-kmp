package zinqq.app.screens

import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.wallet_core.ActivityDirection
import uniffi.wallet_core.ActivityKind
import zinqq.app.R
import zinqq.app.WalletHolder
import zinqq.app.activityAmountText
import zinqq.app.components.CenteredNote
import zinqq.app.explorerTxUrl
import zinqq.app.formatDetailDate
import zinqq.app.formatDetailTime
import zinqq.app.midTruncate
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme
import zinqq.app.txStatusLabel

/**
 * The PWA's TransactionDetail (U14, R11; `TransactionDetail.tsx`): dark
 * room, hero direction + signed amount, Date/Time (en-GB) / Status / Type
 * rows, and a mempool.space link for on-chain rows. The row is looked up by
 * id from the activity snapshot (the PWA's router-state fast path collapses
 * to the same lookup). Channel closes redirect to their live detail page —
 * a close spans ~14 days and a snapshot would go stale.
 */
@Composable
fun TransactionDetailScreen(
    holder: WalletHolder,
    txId: String,
    onBack: () -> Unit,
    onRedirectToClose: (String) -> Unit,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors
    val transactions = state.activity
    val tx = transactions?.firstOrNull { it.id == txId }

    val closeChannelId = if (tx?.kind == ActivityKind.CHANNEL_CLOSE) tx.channelId else null
    LaunchedEffect(closeChannelId) {
        closeChannelId?.let(onRedirectToClose)
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Payment Details", onBack = onBack, tint = colors.onDark)

        when {
            tx == null && transactions == null -> CenteredNote("Loading...")
            tx == null -> CenteredNote("Transaction not found")
            closeChannelId != null -> Unit // redirecting
            else -> {
                val isSent = tx.direction == ActivityDirection.SENT
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 24.dp)
                        .padding(top = 32.dp, bottom = 24.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(
                            painter = painterResource(
                                if (isSent) R.drawable.ic_arrow_up_right
                                else R.drawable.ic_arrow_down_left,
                            ),
                            contentDescription = null,
                            tint = colors.onDarkMuted,
                            modifier = Modifier.size(20.dp),
                        )
                        Text(
                            text = if (isSent) "Sent" else "Received",
                            color = colors.onDarkMuted,
                            fontSize = 18.sp,
                            fontWeight = FontWeight.SemiBold,
                            modifier = Modifier.padding(start = 8.dp),
                        )
                    }
                    Text(
                        text = activityAmountText(tx),
                        color = colors.onDark,
                        fontFamily = ZinqqTheme.fonts.display,
                        fontWeight = FontWeight.Bold,
                        fontSize = 36.sp,
                        modifier = Modifier.padding(top = 8.dp),
                    )
                }

                HorizontalDivider(
                    color = colors.onDark.copy(alpha = 0.1f),
                    modifier = Modifier.padding(horizontal = 24.dp),
                )

                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 24.dp)
                        .padding(top = 8.dp),
                ) {
                    val timestamp = tx.createdAtMs.toLong()
                    DetailRow(label = "Date", value = formatDetailDate(timestamp))
                    DetailRow(label = "Time", value = formatDetailTime(timestamp))
                    DetailRow(label = "Status", value = txStatusLabel(tx.status))
                    DetailRow(
                        label = "Type",
                        value = if (tx.kind == ActivityKind.LIGHTNING) "Lightning" else "On-chain",
                    )
                    if (tx.kind == ActivityKind.ONCHAIN) {
                        ExplorerLinkRow(txid = tx.id)
                    }
                }
            }
        }
    }
}

/** The PWA's `DetailRow`: muted label left, semibold value right. */
@Composable
internal fun DetailRow(label: String, value: String) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = label,
            color = colors.onDarkMuted,
            fontSize = 16.sp,
            modifier = Modifier.weight(1f),
        )
        Text(
            text = value,
            color = colors.onDark,
            fontSize = 16.sp,
            fontWeight = FontWeight.SemiBold,
        )
    }
}

/** "Transaction" row: mid-truncated txid opening mempool.space externally. */
@Composable
private fun ExplorerLinkRow(txid: String) {
    val colors = ZinqqTheme.colors
    val context = LocalContext.current
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = "Transaction",
            color = colors.onDarkMuted,
            fontSize = 16.sp,
            modifier = Modifier.weight(1f),
        )
        Text(
            text = midTruncate(txid, 8, 8, "..."),
            color = colors.onDark,
            fontSize = 16.sp,
            fontWeight = FontWeight.SemiBold,
            textDecoration = TextDecoration.Underline,
            modifier = Modifier.clickable {
                context.startActivity(
                    Intent(Intent.ACTION_VIEW, Uri.parse(explorerTxUrl(txid))),
                )
            },
        )
    }
}
