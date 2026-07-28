package zinqq.app.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
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
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.WalletHolder
import zinqq.app.components.BalanceDisplay
import zinqq.app.components.Banner
import zinqq.app.components.BannerIcon
import zinqq.app.homeBalance
import zinqq.app.recoveryBanner
import zinqq.app.sweepBanner
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Home (U14, R12; `Home.tsx`): the unified BalanceDisplay,
 * RecoveryBanner / PendingSweepBanner, and the two 88dp CTAs. The PWA's
 * install button and its manual refresh are both omitted natively — wallet
 * data re-queries itself from core events. A fatal start failure replaces the
 * content with "Something went wrong" (`Home.tsx:29-42`).
 */
@Composable
fun HomeScreen(
    holder: WalletHolder,
    onSend: () -> Unit,
    onRequest: () -> Unit,
    onRecover: () -> Unit,
    onReceive: () -> Unit,
) {
    val state by holder.state.collectAsState()
    val colors = ZinqqTheme.colors

    if (state.startError != null) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.field)
                .padding(horizontal = 24.dp),
            verticalArrangement = Arrangement.Center,
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                text = "Something went wrong",
                color = colors.onField,
                fontSize = 18.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                text = state.startError.orEmpty(),
                color = colors.onFieldMuted,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 8.dp),
            )
        }
        return
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.field)
            .padding(horizontal = 24.dp)
            .padding(top = 16.dp),
    ) {
        // Balance centered in the flexible middle, like the PWA's
        // justify-between column.
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .weight(1f),
            contentAlignment = Alignment.CenterStart,
        ) {
            val balance = state.balances?.let(::homeBalance)
            BalanceDisplay(
                balanceSats = balance?.totalSats ?: 0L,
                visible = state.balanceVisible,
                onToggleVisible = { holder.setBalanceVisible(!state.balanceVisible) },
                pendingSats = balance?.pendingSats,
                loading = balance == null,
            )
        }

        recoveryBanner(state.recoveryState, state.recoveryBannerDismissed)?.let { banner ->
            Banner(
                icon = if (banner.dismissible) BannerIcon.CHECK else BannerIcon.WARNING_HOT,
                title = banner.title,
                subtitle = banner.subtitle,
                onClick = if (banner.navigatesToRecover) onRecover else null,
                onDismiss = if (banner.dismissible) holder::dismissRecoveryBanner else null,
                modifier = Modifier.padding(bottom = 12.dp),
            )
        }

        sweepBanner(state.pendingSweep)?.let { banner ->
            Banner(
                icon = if (banner.navigatesToReceive) BannerIcon.WARNING_HOT else BannerIcon.WARNING,
                title = banner.heading,
                subtitle = banner.subtitle,
                onClick = if (banner.navigatesToReceive) onReceive else null,
                modifier = Modifier.padding(bottom = 12.dp),
            )
        }

        // The two 88dp CTAs: Send filled (the field screen's hot moment),
        // Request outlined (Home.tsx:92-107).
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(bottom = 12.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            HomeCta(
                label = "Send",
                iconRes = R.drawable.ic_arrow_up_right,
                background = colors.fieldCta,
                contentColor = colors.onFieldCta,
                outline = null,
                onClick = onSend,
                modifier = Modifier.weight(1f),
            )
            HomeCta(
                label = "Request",
                iconRes = R.drawable.ic_arrow_down_left,
                background = Color.Transparent,
                contentColor = colors.onField,
                outline = colors.fieldOutline,
                onClick = onRequest,
                modifier = Modifier.weight(1f),
            )
        }
    }
}

@Composable
private fun HomeCta(
    label: String,
    iconRes: Int,
    background: Color,
    contentColor: Color,
    outline: Color?,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val shape = RoundedCornerShape(16.dp)
    Row(
        modifier = modifier
            .height(88.dp)
            .clip(shape)
            .background(background)
            .let { if (outline != null) it.border(2.dp, outline, shape) else it }
            .clickable(onClick = onClick)
            .semantics { contentDescription = label },
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        Text(
            text = label.uppercase(),
            color = contentColor,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 20.sp,
            letterSpacing = 1.sp,
        )
        Icon(
            painter = painterResource(iconRes),
            contentDescription = null,
            tint = contentColor,
            modifier = Modifier
                .padding(start = 12.dp)
                .size(22.dp),
        )
    }
}
