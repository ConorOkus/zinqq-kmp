package zinqq.app.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.sp
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme

/**
 * Minimal stand-in for every not-yet-built destination (U13): correct header,
 * declared back target, and the right room/field background so all 16 routes
 * are reachable in all three themes. U14–U17 replace these bodies.
 */
@Composable
fun PlaceholderScreen(
    title: String,
    onBack: (() -> Unit)?,
    isFieldScreen: Boolean = false,
) {
    val colors = ZinqqTheme.colors
    val background = if (isFieldScreen) colors.field else colors.dark
    val tint = if (isFieldScreen) colors.onField else colors.onDark
    val muted = if (isFieldScreen) colors.onFieldMuted else colors.onDarkMuted
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(background),
    ) {
        ScreenHeader(title = title, onBack = onBack, tint = tint)
        Box(
            modifier = Modifier
                .fillMaxSize()
                .weight(1f),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = "TODO U14-U17",
                color = muted,
                fontSize = 14.sp,
            )
        }
    }
}
