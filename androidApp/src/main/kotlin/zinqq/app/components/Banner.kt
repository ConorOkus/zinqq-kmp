package zinqq.app.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
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
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Home-screen banner card pattern (U13, KTD-11, R12), generalized
 * over its variants — `RecoveryBanner`, `PendingSweepBanner`, and the spike's
 * sync-failure line: rounded card on `onField/10`, 36dp icon slot, display
 * -font title, muted subtitle, optional tap-through chevron or dismiss.
 * U14 supplies the real state; copy is carried verbatim from the PWA.
 */
enum class BannerIcon { WARNING_HOT, WARNING, CHECK }

@Composable
fun Banner(
    icon: BannerIcon,
    title: String,
    subtitle: String,
    modifier: Modifier = Modifier,
    onClick: (() -> Unit)? = null,
    onDismiss: (() -> Unit)? = null,
) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(colors.onField.copy(alpha = 0.1f))
            .let { if (onClick != null) it.clickable(onClick = onClick) else it }
            .padding(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier.size(36.dp),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(
                    when (icon) {
                        BannerIcon.WARNING_HOT, BannerIcon.WARNING -> R.drawable.ic_alert_triangle
                        BannerIcon.CHECK -> R.drawable.ic_check
                    },
                ),
                contentDescription = null,
                tint = if (icon == BannerIcon.WARNING_HOT) colors.hot else colors.onField,
                modifier = Modifier.size(20.dp),
            )
        }
        Column(
            modifier = Modifier
                .weight(1f)
                .padding(start = 12.dp),
        ) {
            Text(
                text = title,
                color = colors.onField,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = 16.sp,
            )
            Text(
                text = subtitle,
                color = colors.onFieldMuted,
                fontSize = 12.sp,
                modifier = Modifier.padding(top = 2.dp),
            )
        }
        when {
            onDismiss != null -> Box(
                modifier = Modifier
                    .size(32.dp)
                    .clip(CircleShape)
                    .clickable(onClick = onDismiss),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_x_close),
                    contentDescription = "Dismiss",
                    tint = colors.onFieldMuted,
                    modifier = Modifier.size(16.dp),
                )
            }
            onClick != null -> Icon(
                painter = painterResource(R.drawable.ic_chevron_right),
                contentDescription = null,
                tint = colors.onFieldMuted,
                modifier = Modifier.size(20.dp),
            )
        }
    }
}
