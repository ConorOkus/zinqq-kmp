package zinqq.app.screens.settings

import androidx.compose.foundation.background
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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme

/**
 * Shared scaffolding for the eight settings screens (U17, R12): the PWA's
 * dark-room screen shell, its icon-tile row button, and its full-width
 * rounded CTA in the recurring color variants.
 */

/** Dark-room screen: `bg-dark` column under a [ScreenHeader]. */
@Composable
fun SettingsScaffold(
    title: String,
    onBack: (() -> Unit)?,
    content: @Composable () -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(ZinqqTheme.colors.dark),
    ) {
        ScreenHeader(title = title, onBack = onBack, tint = ZinqqTheme.colors.onDark)
        content()
    }
}

/** The Settings/Advanced icon-tile row (`Settings.tsx:122-136`). */
@Composable
fun SettingsRowItem(
    row: SettingsRowSpec,
    iconRes: Int,
    onClick: (() -> Unit)?,
) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            // Inert rows (How It Works / Get Help) stay tappable-looking but
            // do nothing, exactly like the PWA's `route: null` buttons.
            .clickable(onClick = onClick ?: {})
            .padding(horizontal = 8.dp, vertical = 16.dp)
            .semantics { contentDescription = row.label },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(44.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(colors.darkElevated),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(iconRes),
                contentDescription = null,
                tint = colors.onDarkMuted,
                modifier = Modifier.size(22.dp),
            )
        }
        Text(
            text = row.label,
            color = colors.onDark,
            fontSize = 16.sp,
            fontWeight = FontWeight.SemiBold,
            modifier = Modifier
                .weight(1f)
                .padding(start = 16.dp),
        )
        Text(
            text = row.detail,
            color = colors.onDarkMuted,
            fontSize = 14.sp,
        )
    }
}

/** Full-width rounded CTA (`rounded-xl px-6 py-4 font-display font-bold`). */
@Composable
fun SettingsCta(
    label: String,
    background: Color,
    contentColor: Color,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    disabledAlpha: Float = 0.4f,
) {
    Box(
        modifier = modifier
            .fillMaxWidth()
            .height(56.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(background)
            .alpha(if (enabled) 1f else disabledAlpha)
            .clickable(enabled = enabled, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label,
            color = contentColor,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 17.sp,
        )
    }
}

/** Centered muted note filling the remaining screen (Loading… etc.). */
@Composable
fun CenteredNote(text: String, color: Color = ZinqqTheme.colors.onDarkMuted) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Text(
            text = text,
            color = color,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
        )
    }
}
