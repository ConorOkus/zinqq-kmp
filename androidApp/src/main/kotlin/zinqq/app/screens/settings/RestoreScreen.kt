package zinqq.app.screens.settings

import android.view.WindowManager
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.R
import zinqq.app.RestoreUi
import zinqq.app.WalletHolder
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Recover Wallet (U17, F3, R1/R4 UI; `Restore.tsx`): the 12-input
 * 3-column grid with paste-fill into any field, the validation-gated
 * Continue (core `derive_debug_info` as the BIP39 check), the destructive
 * "Erase & Restore" confirm, live `RestoreProgress` steps, the PWA error
 * copy, and navigate-Home on success. The holder owns the stop → restore →
 * restart sequence; this screen renders [RestoreUi] and local input state.
 */
@Composable
fun RestoreScreen(
    holder: WalletHolder,
    onBack: (() -> Unit)?,
    onRestored: () -> Unit,
) {
    val state by holder.state.collectAsState()

    var words by remember { mutableStateOf(List(RESTORE_WORD_COUNT) { "" }) }
    var confirming by remember { mutableStateOf(false) }
    var mnemonicValid by remember { mutableStateOf(false) }

    // FLAG_SECURE exactly while typed seed words exist (mirrors BackupScreen,
    // plan U17, R1): the word-entry grid AND the destructive-confirm step —
    // any state still holding the typed words — must not appear in
    // screenshots or recents thumbnails.
    val activity = LocalContext.current.findActivity()
    val hasTypedWords = words.any { it.isNotBlank() }
    DisposableEffect(hasTypedWords) {
        if (hasTypedWords) {
            activity?.window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
        onDispose {
            activity?.window?.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
    }

    // Re-validate whenever the grid changes; `derive_debug_info` is the
    // exported BIP39 check (cheap key derivation, IO-dispatched).
    LaunchedEffect(words) {
        mnemonicValid = words.all { it.isNotBlank() } &&
            holder.validateMnemonic(mnemonicString(words))
    }

    val restore = state.restore
    if (restore is RestoreUi.Succeeded) {
        LaunchedEffect(Unit) {
            holder.clearRestore()
            onRestored()
        }
    }

    SettingsScaffold(title = "Recover Wallet", onBack = onBack) {
        when (restore) {
            is RestoreUi.InProgress -> RestoringBody(step = restore.step)
            is RestoreUi.Failed -> ErrorBody(
                message = restore.message,
                onTryAgain = {
                    confirming = false
                    holder.clearRestore()
                },
            )
            // Succeeded navigates away above; render the spinner meanwhile.
            is RestoreUi.Succeeded -> RestoringBody(step = "Restarting wallet...")
            null -> if (confirming) {
                ConfirmBody(
                    onRestore = { holder.startRestore(mnemonicString(words)) },
                    onCancel = { confirming = false },
                )
            } else {
                InputBody(
                    words = words,
                    continueEnabled = continueEnabled(words, mnemonicValid),
                    onWordChange = { index, value ->
                        words = applyWordChange(words, index, value)
                    },
                    onContinue = { confirming = true },
                )
            }
        }
    }
}

@Composable
private fun InputBody(
    words: List<String>,
    continueEnabled: Boolean,
    onWordChange: (Int, String) -> Unit,
    onContinue: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 16.dp)
            .padding(top = 16.dp, bottom = 32.dp),
    ) {
        Text(
            text = "Enter your 12-word recovery phrase to restore your wallet from " +
                "backup. You can paste all 12 words into the first field.",
            color = colors.onDarkMuted,
            fontSize = 14.sp,
        )
        // 3-column grid of numbered inputs (Restore.tsx:183-200).
        Column(
            modifier = Modifier.padding(top = 16.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            words.chunked(3).forEachIndexed { rowIndex, rowWords ->
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    rowWords.forEachIndexed { colIndex, word ->
                        val index = rowIndex * 3 + colIndex
                        Row(
                            modifier = Modifier.weight(1f),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(
                                text = "${index + 1}",
                                color = colors.onDarkMuted,
                                fontSize = 12.sp,
                                textAlign = TextAlign.End,
                                modifier = Modifier.width(20.dp),
                            )
                            Box(
                                modifier = Modifier
                                    .weight(1f)
                                    .padding(start = 4.dp)
                                    .clip(RoundedCornerShape(8.dp))
                                    .background(colors.darkElevated)
                                    .padding(horizontal = 8.dp, vertical = 10.dp),
                            ) {
                                BasicTextField(
                                    value = word,
                                    onValueChange = { onWordChange(index, it) },
                                    singleLine = true,
                                    keyboardOptions = KeyboardOptions(
                                        capitalization = KeyboardCapitalization.None,
                                        autoCorrectEnabled = false,
                                    ),
                                    textStyle = TextStyle(
                                        color = colors.onDark,
                                        fontSize = 14.sp,
                                    ),
                                    cursorBrush = SolidColor(colors.hot),
                                    modifier = Modifier.fillMaxWidth(),
                                )
                            }
                        }
                    }
                }
            }
        }
        SettingsCta(
            label = "Continue",
            background = colors.cta,
            contentColor = colors.onCta,
            enabled = continueEnabled,
            onClick = onContinue,
            modifier = Modifier.padding(top = 24.dp),
        )
    }
}

@Composable
private fun ConfirmBody(
    onRestore: () -> Unit,
    onCancel: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 16.dp)
            .padding(bottom = 32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Box(
            modifier = Modifier
                .size(64.dp)
                .clip(CircleShape)
                .background(colors.darkElevated),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_alert_triangle),
                contentDescription = null,
                tint = colors.warning,
                modifier = Modifier.size(32.dp),
            )
        }
        Text(
            text = "This will replace your current wallet",
            color = colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 20.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 24.dp),
        )
        Text(
            text = "All existing wallet data will be erased and replaced with the " +
                "restored wallet. Make sure you have backed up your current seed " +
                "phrase if needed.",
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 12.dp),
        )
        SettingsCta(
            label = "Erase & Restore",
            background = colors.hot,
            contentColor = colors.onHot,
            onClick = onRestore,
            modifier = Modifier.padding(top = 32.dp),
        )
        SettingsCta(
            label = "Cancel",
            background = colors.darkElevated,
            contentColor = colors.onDark,
            onClick = onCancel,
            modifier = Modifier.padding(top = 12.dp),
        )
    }
}

@Composable
private fun RestoringBody(step: String) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier.fillMaxSize(),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        CircularProgressIndicator(
            color = colors.onDark,
            strokeWidth = 2.dp,
            modifier = Modifier.size(32.dp),
        )
        Text(
            text = step,
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            modifier = Modifier.padding(top = 16.dp),
        )
    }
}

@Composable
private fun ErrorBody(message: String, onTryAgain: () -> Unit) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(horizontal = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Text(
            text = message,
            color = colors.danger,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
        )
        SettingsCta(
            label = "Try Again",
            background = colors.darkElevated,
            contentColor = colors.onDark,
            onClick = onTryAgain,
            modifier = Modifier.padding(top = 24.dp),
        )
    }
}
