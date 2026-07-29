package zinqq.app.components

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.theme.ZinqqDimens
import zinqq.app.theme.ZinqqTheme
import zinqq.main.formatBtc

/**
 * The PWA's `BalanceDisplay` (U13, KTD-11, R12): unified total as a BIP177
 * `₿` amount in the display font, a `+₿X pending` line, and a hide/show
 * toggle. Visibility is persisted by the caller under the PWA's
 * `balance-visible` key (DataStore); hidden renders six dots. The readout
 * scales down past 5 digits (the PWA's clamp equivalent: text-7xl → text-5xl).
 */
@Composable
fun BalanceDisplay(
    balanceSats: Long,
    visible: Boolean,
    onToggleVisible: () -> Unit,
    modifier: Modifier = Modifier,
    pendingSats: Long? = null,
    breakdown: String? = null,
    loading: Boolean = false,
) {
    val colors = ZinqqTheme.colors
    Column(modifier = modifier, horizontalAlignment = Alignment.Start) {
        if (loading) {
            CircularProgressIndicator(
                color = colors.onField,
                trackColor = colors.onField.copy(alpha = 0.3f),
                strokeWidth = 3.dp,
                modifier = Modifier.size(32.dp),
            )
        } else {
            if (visible) {
                val formatted = formatBtc(balanceSats)
                // Digit-count breakpoint replaces the PWA's vw clamp: 5 digits
                // or fewer read at 72sp, longer amounts drop to 48sp.
                val digits = formatted.count { it.isDigit() }
                Text(
                    text = formatted,
                    color = colors.onField,
                    fontFamily = ZinqqTheme.fonts.display,
                    fontWeight = FontWeight.Bold,
                    fontSize = if (digits > 5) 48.sp else 72.sp,
                    lineHeight = if (digits > 5) 48.sp else 72.sp,
                    letterSpacing = (-1).sp,
                    modifier = Modifier.semantics {
                        contentDescription = "Balance $formatted"
                    },
                )
            } else {
                Text(
                    text = "••••••",
                    color = colors.onField,
                    fontFamily = ZinqqTheme.fonts.display,
                    fontWeight = FontWeight.Bold,
                    fontSize = 36.sp,
                    letterSpacing = 4.sp,
                    modifier = Modifier.semantics { contentDescription = "Balance hidden" },
                )
            }
            if (pendingSats != null && pendingSats > 0 && visible) {
                Text(
                    text = "+${formatBtc(pendingSats)} pending",
                    color = colors.onFieldMuted,
                    fontSize = 14.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            if (breakdown != null && visible) {
                Text(
                    text = breakdown,
                    color = colors.onFieldMuted,
                    fontSize = 14.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                modifier = Modifier
                    .padding(top = 12.dp)
                    .heightIn(min = ZinqqDimens.MinTouchTarget)
                    .clickable(onClick = onToggleVisible)
                    .semantics {
                        contentDescription = if (visible) "Hide balance" else "Show balance"
                    },
            ) {
                Icon(
                    painter = painterResource(
                        if (visible) R.drawable.ic_eye_off else R.drawable.ic_eye,
                    ),
                    contentDescription = null,
                    tint = colors.onFieldMuted,
                    modifier = Modifier.size(20.dp),
                )
                Text(
                    text = if (visible) "Hide balance" else "Show balance",
                    color = colors.onFieldMuted,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                )
            }
        }
    }
}
