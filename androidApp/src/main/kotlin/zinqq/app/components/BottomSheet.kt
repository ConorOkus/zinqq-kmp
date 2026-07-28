package zinqq.app.components

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.tween
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.zIndex
import zinqq.app.theme.ZinqqDimens
import zinqq.app.theme.ZinqqTheme
import zinqq.app.theme.ZinqqZ

/**
 * The PWA's `BottomSheet` (U13, KTD-11): scrim + bottom-anchored elevated
 * card with the 200ms slide-up, capped at the 430dp content width, sitting at
 * z-300 in the ladder. Used for the copy-sheet pattern (Receive/Send land in
 * U15/U16). Scrim taps and system back both close it at the call site.
 */
@Composable
fun BottomSheet(
    open: Boolean,
    onClose: () -> Unit,
    content: @Composable () -> Unit,
) {
    if (!open) return
    Box(
        modifier = Modifier
            .fillMaxSize()
            .zIndex(ZinqqZ.SHEET),
        contentAlignment = Alignment.BottomCenter,
    ) {
        // Scrim: black/50, click-to-close (like the PWA's backdrop).
        Box(
            modifier = Modifier
                .fillMaxSize()
                .background(Color.Black.copy(alpha = 0.5f))
                .clickable(
                    interactionSource = remember { MutableInteractionSource() },
                    indication = null,
                    onClick = onClose,
                ),
        )
        AnimatedVisibility(
            visible = open,
            enter = slideInVertically(animationSpec = tween(200)) { it },
            exit = slideOutVertically(animationSpec = tween(200)) { it },
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .widthIn(max = ZinqqDimens.ContentMaxWidth)
                    .clip(RoundedCornerShape(topStart = 16.dp, topEnd = 16.dp))
                    .background(ZinqqTheme.colors.darkElevated)
                    // Consume clicks so they don't fall through to the scrim.
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                        onClick = {},
                    )
                    .padding(horizontal = 24.dp, vertical = 24.dp),
            ) {
                content()
            }
        }
    }
}
