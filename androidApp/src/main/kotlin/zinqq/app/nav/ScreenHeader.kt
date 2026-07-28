package zinqq.app.nav

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.theme.ZinqqDimens
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's `ScreenHeader` (U13, KTD-11): fixed 56dp bar, centered title,
 * 44dp back button on the left navigating to the screen's declared `backTo`
 * destination, optional close/right action on the right.
 */
@Composable
fun ScreenHeader(
    title: String,
    onBack: (() -> Unit)? = null,
    onClose: (() -> Unit)? = null,
    tint: Color = ZinqqTheme.colors.onDark,
    rightAction: (@Composable () -> Unit)? = null,
) {
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(ZinqqDimens.HeaderHeight)
            .padding(horizontal = 16.dp),
    ) {
        if (onBack != null) {
            IconButton(
                onClick = onBack,
                modifier = Modifier
                    .align(Alignment.CenterStart)
                    .size(ZinqqDimens.MinTouchTarget)
                    .clip(CircleShape),
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_chevron_back),
                    contentDescription = "Back",
                    tint = tint,
                )
            }
        }
        Text(
            text = title,
            color = tint,
            fontSize = 18.sp,
            fontWeight = FontWeight.SemiBold,
            fontFamily = ZinqqTheme.fonts.sans,
            modifier = Modifier.align(Alignment.Center),
        )
        when {
            rightAction != null -> Box(modifier = Modifier.align(Alignment.CenterEnd)) {
                rightAction()
            }
            onClose != null -> IconButton(
                onClick = onClose,
                modifier = Modifier
                    .align(Alignment.CenterEnd)
                    .size(ZinqqDimens.MinTouchTarget)
                    .clip(CircleShape),
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_x_close),
                    contentDescription = "Close",
                    tint = tint,
                )
            }
        }
    }
}
