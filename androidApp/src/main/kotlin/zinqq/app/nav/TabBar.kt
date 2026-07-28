package zinqq.app.nav

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.theme.ZinqqDimens
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's `TabBar` (U13, KTD-11, R12): a fixed 64dp field-colored bar shown
 * ONLY on Home and Activity — scan icon, WALLET pill, ACTIVITY pill, menu
 * icon (→ Settings). Pills use the uppercase display font; the active pill
 * inverts to `tabActive`/`onTabActive`.
 */
@Composable
fun TabBar(
    current: Route,
    onNavigate: (Route) -> Unit,
) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .height(ZinqqDimens.TabBarHeight)
            .background(colors.field)
            .padding(horizontal = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        IconButton(
            onClick = { onNavigate(Route.Scan) },
            modifier = Modifier
                .size(ZinqqDimens.MinTouchTarget)
                .clip(CircleShape),
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_scan),
                contentDescription = "Scan QR code",
                tint = colors.onField,
                modifier = Modifier.size(22.dp),
            )
        }

        TabPill(
            label = "Wallet",
            active = current == Route.Home,
            onClick = { onNavigate(Route.Home) },
            modifier = Modifier.weight(1f),
        )

        TabPill(
            label = "Activity",
            active = current == Route.Activity,
            onClick = { onNavigate(Route.Activity) },
            modifier = Modifier.weight(1f),
        )

        IconButton(
            onClick = { onNavigate(Route.Settings) },
            modifier = Modifier
                .size(ZinqqDimens.MinTouchTarget)
                .clip(CircleShape),
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_menu),
                contentDescription = "Settings menu",
                tint = colors.onField,
                modifier = Modifier.size(22.dp),
            )
        }
    }
}

@Composable
private fun TabPill(
    label: String,
    active: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = ZinqqTheme.colors
    Box(
        modifier = modifier
            .widthIn(max = 120.dp)
            .height(ZinqqDimens.MinTouchTarget)
            .clip(CircleShape)
            .background(if (active) colors.tabActive else colors.field)
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label.uppercase(),
            color = if (active) colors.onTabActive else colors.onFieldMuted,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 14.sp,
            letterSpacing = 1.sp,
        )
    }
}
