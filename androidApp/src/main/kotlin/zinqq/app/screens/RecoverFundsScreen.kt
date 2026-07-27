package zinqq.app.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.WalletHolder
import zinqq.app.components.CenteredNote
import zinqq.app.components.QrView
import zinqq.app.components.rememberCopiedFlash
import zinqq.app.midTruncate
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme
import zinqq.spike.formatBtc

/**
 * The PWA's RecoverFunds (U14, R9; `RecoverFunds.tsx`): dark room with the
 * explanation copy, the Stuck balance / Deposit needed card ("Unknown" when
 * the stuck estimate is null — never a lying zero), a `bitcoin:{address}`
 * QR, the address pill with 1,500ms copy feedback, and the ~14 day timelock
 * notice. Renders the recovery state snapshot; refreshes keep it live.
 */
@Composable
fun RecoverFundsScreen(
    holder: WalletHolder,
    onBack: () -> Unit,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors
    val recovery = state.recoveryState

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Recover Funds", onBack = onBack, tint = colors.onDark)

        if (recovery == null) {
            CenteredNote("No recovery needed")
            return@Column
        }

        val clipboard = LocalClipboardManager.current
        // The PWA's 1,500ms "Copied!" flash (RecoverFunds.tsx:20-21).
        var copied by rememberCopiedFlash(1_500)

        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .padding(bottom = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(24.dp),
        ) {
            Text(
                text = "Your payment channel closed unexpectedly. Your funds are safe — " +
                    "a small deposit is needed to move them back to your wallet.",
                color = colors.onDarkMuted,
                fontSize = 16.sp,
                textAlign = TextAlign.Center,
                lineHeight = 24.sp,
                modifier = Modifier.widthIn(max = 320.dp),
            )

            // Amounts card.
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(12.dp))
                    .background(colors.darkElevated)
                    .padding(horizontal = 20.dp, vertical = 16.dp),
                verticalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                AmountCardRow(
                    label = "Stuck balance",
                    value = recovery.stuckBalanceSat?.let { formatBtc(it.toLong()) } ?: "Unknown",
                    valueColor = colors.onDark,
                )
                HorizontalDivider(color = colors.darkBorder)
                AmountCardRow(
                    label = "Deposit needed",
                    value = formatBtc(recovery.depositNeededSat.toLong()),
                    valueColor = colors.amount,
                )
            }

            QrView(
                payload = "bitcoin:${recovery.depositAddress}",
                contentDescription = "QR code for deposit address ${recovery.depositAddress}",
                modifier = Modifier.size(200.dp),
            )

            // Address pill with the copy button.
            Row(
                modifier = Modifier
                    .clip(RoundedCornerShape(50))
                    .background(colors.darkElevated)
                    .padding(start = 20.dp, end = 8.dp, top = 8.dp, bottom = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = midTruncate(recovery.depositAddress, 12, 8, "..."),
                    color = colors.onDarkMuted,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 14.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                Row(
                    modifier = Modifier
                        .padding(start = 12.dp)
                        .clip(CircleShape)
                        .background(colors.pill)
                        .clickable {
                            clipboard.setText(AnnotatedString(recovery.depositAddress))
                            copied = true
                        }
                        .padding(horizontal = 12.dp, vertical = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    if (!copied) {
                        Icon(
                            painter = painterResource(R.drawable.ic_copy),
                            contentDescription = null,
                            tint = colors.onPill,
                            modifier = Modifier
                                .padding(end = 6.dp)
                                .size(14.dp),
                        )
                    }
                    // The PWA's pill copy is CSS-uppercased ('Copy'/'Copied!').
                    Text(
                        text = if (copied) "COPIED!" else "COPY",
                        color = colors.onPill,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Bold,
                        letterSpacing = 1.sp,
                    )
                }
            }

            // Timelock notice.
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(12.dp))
                    .background(colors.darkElevated)
                    .padding(16.dp),
                verticalAlignment = Alignment.Top,
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_clock),
                    contentDescription = null,
                    tint = colors.onDarkMuted,
                    modifier = Modifier
                        .padding(top = 2.dp)
                        .size(20.dp),
                )
                Text(
                    text = "After recovery, funds will be available in approximately 14 days",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    lineHeight = 18.sp,
                    modifier = Modifier.padding(start = 12.dp),
                )
            }
        }
    }
}

@Composable
private fun AmountCardRow(label: String, value: String, valueColor: androidx.compose.ui.graphics.Color) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = label,
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            fontWeight = FontWeight.Medium,
            modifier = Modifier.weight(1f),
        )
        Text(
            text = value,
            color = valueColor,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 18.sp,
        )
    }
}
