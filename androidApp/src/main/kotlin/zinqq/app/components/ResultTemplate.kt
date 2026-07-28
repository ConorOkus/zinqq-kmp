package zinqq.app.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
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
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's success/error result screens as one template (U13, KTD-11, R12):
 * centered 80dp circle (badge + check on success, danger/15 + X on failure),
 * headline, optional detail, the load-bearing "Your funds are safe."
 * reassurance on failures, and a full-width CTA. Copy is carried verbatim
 * from the PWA at the call sites (U14–U17).
 */
@Composable
fun ResultTemplate(
    success: Boolean,
    headline: String,
    onCta: () -> Unit,
    modifier: Modifier = Modifier,
    detail: String? = null,
    fundsAreSafe: Boolean = !success,
    ctaLabel: String = "Done",
    extraContent: (@Composable () -> Unit)? = null,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = modifier
            .fillMaxSize()
            .background(colors.dark)
            .padding(horizontal = 32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Box(
            modifier = Modifier
                .size(80.dp)
                .clip(CircleShape)
                .background(if (success) colors.badge else colors.danger.copy(alpha = 0.15f)),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(if (success) R.drawable.ic_check else R.drawable.ic_x_close),
                contentDescription = if (success) "Success" else "Failure",
                tint = if (success) colors.onBadge else colors.danger,
                modifier = Modifier.size(40.dp),
            )
        }
        Text(
            text = headline,
            color = colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = if (success) 34.sp else 24.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 24.dp),
        )
        if (detail != null) {
            Text(
                text = detail,
                color = if (success) colors.onDarkMuted else colors.danger,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 8.dp),
            )
        }
        if (fundsAreSafe) {
            Text(
                text = "Your funds are safe.",
                color = colors.onDarkMuted,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
        extraContent?.let {
            Box(modifier = Modifier.padding(top = 16.dp)) { it() }
        }
        Box(
            modifier = Modifier
                .padding(top = 32.dp)
                .fillMaxWidth()
                .widthIn(max = 280.dp)
                .height(56.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(colors.cta)
                .clickable(onClick = onCta),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = ctaLabel,
                color = colors.onCta,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = 18.sp,
            )
        }
    }
}
