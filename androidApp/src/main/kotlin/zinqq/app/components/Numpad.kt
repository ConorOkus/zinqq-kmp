package zinqq.app.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
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
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.theme.ZinqqTheme
import zinqq.spike.NumpadKey

/**
 * The PWA's sats-only `Numpad` (U13, KTD-11, R12): a Next CTA above a 3×4
 * digit grid on the elevated dark surface. Key presses feed the shared
 * `numpadDigitReducer` at the call site (8-digit cap, leading-zero collapse
 * live in commonMain, not here — R14-style split of logic vs pixels). All
 * targets are at least 44dp.
 */
@Composable
fun Numpad(
    onKey: (NumpadKey) -> Unit,
    onNext: () -> Unit,
    nextEnabled: Boolean,
    modifier: Modifier = Modifier,
    nextLabel: String = "Next",
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(topStart = 16.dp, topEnd = 16.dp))
            .background(colors.darkElevated)
            .padding(start = 24.dp, end = 24.dp, top = 16.dp, bottom = 24.dp),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(56.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(colors.cta)
                .alpha(if (nextEnabled) 1f else 0.3f)
                .clickable(enabled = nextEnabled, onClick = onNext)
                .semantics { contentDescription = nextLabel },
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.Center,
        ) {
            Text(
                text = nextLabel.uppercase(),
                color = colors.onCta,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = 18.sp,
                letterSpacing = 1.sp,
            )
            Icon(
                painter = painterResource(R.drawable.ic_arrow_right),
                contentDescription = null,
                tint = colors.onCta,
                modifier = Modifier
                    .padding(start = 8.dp)
                    .size(20.dp),
            )
        }

        val rows = listOf(
            listOf("1", "2", "3"),
            listOf("4", "5", "6"),
            listOf("7", "8", "9"),
        )
        Column(
            modifier = Modifier.padding(top = 16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            rows.forEach { row ->
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    row.forEach { digit ->
                        DigitKey(
                            label = digit,
                            onClick = { onKey(NumpadKey.Digit(digit.single())) },
                            modifier = Modifier.weight(1f),
                        )
                    }
                }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Box(modifier = Modifier.weight(1f))
                DigitKey(
                    label = "0",
                    onClick = { onKey(NumpadKey.Digit('0')) },
                    modifier = Modifier.weight(1f),
                )
                Box(
                    modifier = Modifier
                        .weight(1f)
                        .height(64.dp)
                        .clip(RoundedCornerShape(12.dp))
                        .clickable { onKey(NumpadKey.Backspace) }
                        .semantics { contentDescription = "Delete" },
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        painter = painterResource(R.drawable.ic_backspace),
                        contentDescription = null,
                        tint = colors.onDark.copy(alpha = 0.7f),
                        modifier = Modifier.size(28.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun DigitKey(
    label: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Box(
        modifier = modifier
            .height(64.dp)
            .clip(RoundedCornerShape(12.dp))
            .clickable(onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label,
            color = ZinqqTheme.colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.SemiBold,
            fontSize = 24.sp,
        )
    }
}
