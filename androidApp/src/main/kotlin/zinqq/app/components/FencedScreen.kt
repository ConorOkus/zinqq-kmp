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
import androidx.compose.ui.zIndex
import zinqq.app.R
import zinqq.app.theme.ZinqqTheme
import zinqq.app.theme.ZinqqZ

/**
 * The blocking fenced screen (U13; plan "System-Wide Impact", KTD-3):
 * another client wrote divergent state into this seed's VSS namespace, the
 * core fenced itself durably, and no automatic un-fence exists. Rendered
 * above ALL destinations (top of the z-ladder) whenever the shell's fenced
 * flag is set — the two exits are user-owned: take over here (the U4
 * wipe-and-restore flow behind Settings → Restore) or quit and keep using
 * the other client.
 */
@Composable
fun FencedScreen(
    onRestore: () -> Unit,
    onQuit: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .zIndex(ZinqqZ.FENCED)
            .background(colors.dark)
            // Swallow all input: nothing below this screen is interactive.
            .clickable(enabled = false, onClick = {})
            .padding(horizontal = 32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Box(
            modifier = Modifier
                .size(80.dp)
                .clip(CircleShape)
                .background(colors.warning.copy(alpha = 0.15f)),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_alert_triangle),
                contentDescription = null,
                tint = colors.warning,
                modifier = Modifier.size(40.dp),
            )
        }
        Text(
            text = "This wallet is active on another device",
            color = colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 24.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 24.dp),
        )
        Text(
            text = "Another device took over this wallet's cloud backup. To keep " +
                "your funds safe, this device stopped. Restore from backup to take " +
                "over here, or quit and keep using the other device.",
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 12.dp),
        )
        Box(
            modifier = Modifier
                .padding(top = 32.dp)
                .fillMaxWidth()
                .height(56.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(colors.cta)
                .clickable(onClick = onRestore),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = "Restore from backup",
                color = colors.onCta,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = 18.sp,
            )
        }
        Box(
            modifier = Modifier
                .padding(top = 12.dp)
                .fillMaxWidth()
                .height(56.dp)
                .clip(RoundedCornerShape(12.dp))
                .clickable(onClick = onQuit),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = "Quit",
                color = colors.onDark,
                fontWeight = FontWeight.Medium,
                fontSize = 16.sp,
            )
        }
    }
}
