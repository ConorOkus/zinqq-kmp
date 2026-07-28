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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import uniffi.wallet_core.CloseInitiatorView
import uniffi.wallet_core.CloseRecordView
import uniffi.wallet_core.CloseStatusLabel
import uniffi.wallet_core.CloseTxView
import zinqq.app.WalletHolder
import zinqq.app.blocksRemaining
import zinqq.app.closeAmountText
import zinqq.app.closeStatusLabel
import zinqq.app.closeTxRoleLabel
import zinqq.app.closeTypeLabel
import zinqq.app.components.CenteredNote
import zinqq.app.components.rememberCopiedFlash
import zinqq.app.confirmationText
import zinqq.app.explorerTxUrl
import zinqq.app.formatCloseDate
import zinqq.app.humanizeBlocks
import zinqq.app.isTerminalClose
import zinqq.app.midTruncate
import zinqq.app.nav.ScreenHeader
import zinqq.app.needsDeposit
import zinqq.app.theme.ZinqqTheme
import zinqq.app.totalFeesSats
import zinqq.main.formatBtc

/**
 * The PWA's ChannelCloseDetail (U14, R9/R11; `ChannelCloseDetail.tsx`):
 * live-updating dark room — status label, `~₿X` estimate while non-terminal,
 * the "Accessible in ~N days (N blocks)" countdown, a needs-deposit link to
 * Recover when the recovery state names this channel, fact rows, and the
 * per-tx list with role labels, confirmation counts, mempool links, and
 * 1,500ms copy feedback. Renders from `close_detail(channel_id)`, re-queried
 * on every wallet-data refresh.
 */
@Composable
fun ChannelCloseDetailScreen(
    holder: WalletHolder,
    channelId: String,
    onBack: () -> Unit,
    onRecover: () -> Unit,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors

    LaunchedEffect(channelId) { holder.loadCloseDetail(channelId) }
    val detail = state.closeDetail?.takeIf { it.channelId == channelId }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Channel Close", onBack = onBack, tint = colors.onDark)

        when {
            detail == null -> CenteredNote("Loading...")
            detail.record == null -> CenteredNote("Close record not found")
            else -> CloseDetailBody(
                record = detail.record,
                needsDepositLink = needsDeposit(state.recoveryState, channelId),
                onRecover = onRecover,
            )
        }
    }
}

@Composable
private fun CloseDetailBody(
    record: CloseRecordView,
    needsDepositLink: Boolean,
    onRecover: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    val terminal = isTerminalClose(record.status)
    val remaining = blocksRemaining(record)

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState()),
    ) {
        // Hero: live status label + estimated amount.
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .padding(top = 32.dp, bottom = 24.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                text = closeStatusLabel(record.status),
                color = colors.onDarkMuted,
                fontSize = 18.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                text = closeAmountText(record),
                color = colors.onDark,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = 36.sp,
                modifier = Modifier.padding(top = 8.dp),
            )
            if (!terminal) {
                Text(
                    text = buildString {
                        append(
                            if (record.initiator == CloseInitiatorView.REMOTE) {
                                "This channel was closed by the network. Your funds are safe " +
                                    "and return to your wallet automatically."
                            } else {
                                "Your funds return to your wallet automatically."
                            },
                        )
                        if (remaining != null && remaining > 0) {
                            append(
                                " Accessible in ${humanizeBlocks(remaining)} ($remaining blocks).",
                            )
                        }
                    },
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(top = 8.dp),
                )
            }
            if (record.status == CloseStatusLabel.RESOLVED_UNVERIFIED) {
                Text(
                    text = "The close resolved on-chain, but this wallet couldn't verify " +
                        "receiving the funds — they may have been swept on another device.",
                    color = colors.warning,
                    fontSize = 14.sp,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(top = 8.dp),
                )
            }
        }

        if (needsDepositLink) {
            Text(
                text = "A small deposit is needed to recover these funds — tap to continue.",
                color = colors.warning,
                fontSize = 14.sp,
                modifier = Modifier
                    .padding(horizontal = 24.dp)
                    .padding(bottom = 16.dp)
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(8.dp))
                    .background(colors.warning.copy(alpha = 0.1f))
                    .clickable(onClick = onRecover)
                    .padding(12.dp),
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
            DetailRow(label = "Initiated", value = formatCloseDate(record.createdAtMs.toLong()))
            record.closureReason?.let { DetailRow(label = "Reason", value = it) }
            DetailRow(label = "Close type", value = closeTypeLabel(record.closeType))
            val totalFees = totalFeesSats(record)
            if (terminal && totalFees > 0) {
                DetailRow(label = "Total fees paid", value = formatBtc(totalFees))
            }
            record.completedAtMs?.let {
                DetailRow(label = "Completed", value = formatCloseDate(it.toLong()))
            }
        }

        if (record.txs.isNotEmpty()) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 24.dp)
                    .padding(top = 16.dp, bottom = 24.dp),
            ) {
                Text(
                    text = "Transactions",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                )
                record.txs.forEachIndexed { index, tx ->
                    CloseTxRow(tx = tx, lastRow = index == record.txs.lastIndex)
                }
            }
        }
    }
}

@Composable
private fun CloseTxRow(tx: CloseTxView, lastRow: Boolean) {
    val colors = ZinqqTheme.colors
    val context = LocalContext.current
    val clipboard = LocalClipboardManager.current
    // The PWA's 1,500ms "Copied" flash (ChannelCloseDetail.tsx:56-59).
    var copied by rememberCopiedFlash(1_500)

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 12.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = closeTxRoleLabel(tx.role),
                color = colors.onDark,
                fontSize = 14.sp,
                fontWeight = FontWeight.SemiBold,
                modifier = Modifier.weight(1f),
            )
            Text(
                text = confirmationText(tx),
                color = colors.onDarkMuted,
                fontSize = 12.sp,
            )
        }
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(top = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = midTruncate(tx.txid, 10, 10, "…"),
                color = colors.onDark,
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
                textDecoration = TextDecoration.Underline,
                modifier = Modifier
                    .weight(1f)
                    .clickable {
                        context.startActivity(
                            Intent(Intent.ACTION_VIEW, Uri.parse(explorerTxUrl(tx.txid))),
                        )
                    },
            )
            Text(
                text = if (copied) "Copied" else "Copy txid",
                color = colors.onDark,
                fontSize = 12.sp,
                textDecoration = TextDecoration.Underline,
                modifier = Modifier
                    .padding(start = 8.dp)
                    .clickable {
                        clipboard.setText(AnnotatedString(tx.txid))
                        copied = true
                    },
            )
        }
        tx.feeSats?.let { fee ->
            Text(
                text = "Fee: ${formatBtc(fee.toLong())}",
                color = colors.onDarkMuted,
                fontSize = 12.sp,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
        if (!lastRow) {
            HorizontalDivider(
                color = colors.onDark.copy(alpha = 0.1f),
                modifier = Modifier.padding(top = 12.dp),
            )
        }
    }
}
