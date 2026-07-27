package zinqq.app.screens.settings

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
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
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import zinqq.app.R
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Wallet Backup (U17, R1 UI; `Backup.tsx`): the write-on-paper
 * warning → "Reveal Seed Phrase" → 2-column numbered word grid with the 60 s
 * "Hides in Ns" auto-hide. Platform-mandated additions (plan U17): the grid
 * hides the instant the screen leaves the foreground (ON_PAUSE — the PWA's
 * `visibilitychange`), and FLAG_SECURE blocks screenshots/recents thumbnails
 * while the words are visible.
 */
@Composable
fun BackupScreen(
    port: SettingsPort,
    onBack: (() -> Unit)?,
    onDone: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    val scope = rememberCoroutineScope()
    var ui by remember { mutableStateOf<BackupUi>(BackupUi.Warning) }

    // FLAG_SECURE exactly while the grid is visible (plan U17, R1).
    val activity = LocalContext.current.findActivity()
    val revealed = ui is BackupUi.Revealed
    DisposableEffect(revealed) {
        if (revealed) {
            activity?.window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
        onDispose {
            activity?.window?.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
    }

    // Lifecycle hide: ON_PAUSE covers backgrounding, recents, and screen-off
    // (the PWA's document-hidden), before any thumbnail is captured.
    val lifecycleOwner = LocalLifecycleOwner.current
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_PAUSE || event == Lifecycle.Event.ON_STOP) {
                ui = hideBackup(ui)
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

    // 1-second countdown while revealed; hitting zero auto-hides.
    LaunchedEffect(revealed) {
        while (ui is BackupUi.Revealed) {
            delay(1_000)
            ui = tickBackup(ui)
        }
    }

    SettingsScaffold(title = "Wallet Backup", onBack = onBack) {
        when (val current = ui) {
            is BackupUi.Warning -> WarningBody(
                onReveal = {
                    scope.launch {
                        ui = try {
                            revealBackup(port.revealMnemonic())
                        } catch (e: Exception) {
                            BackupUi.Error(revealErrorMessage(e))
                        }
                    }
                },
            )
            is BackupUi.Revealed -> RevealedBody(
                words = current.words,
                secondsLeft = current.secondsLeft,
                onDone = onDone,
            )
            is BackupUi.Error -> CenteredNote(current.message, color = colors.danger)
        }
    }
}

@Composable
private fun WarningBody(onReveal: () -> Unit) {
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
            text = "Your recovery phrase is the master key to your wallet.",
            color = colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 20.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 24.dp),
        )
        Text(
            text = "Anyone who has these 12 words can access and steal your funds. " +
                "Never share them with anyone.",
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 12.dp),
        )
        Column(
            modifier = Modifier.padding(top = 24.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            listOf(
                "Write them down on paper and store securely",
                "Do not take a screenshot",
                "Do not copy to clipboard or save digitally",
            ).forEach { bullet ->
                Row {
                    Text(text = "•", color = colors.onDarkMuted, fontSize = 14.sp)
                    Text(
                        text = bullet,
                        color = colors.onDarkMuted,
                        fontSize = 14.sp,
                        modifier = Modifier.padding(start = 8.dp),
                    )
                }
            }
        }
        SettingsCta(
            label = "Reveal Seed Phrase",
            background = colors.cta,
            contentColor = colors.onCta,
            onClick = onReveal,
            modifier = Modifier.padding(top = 40.dp),
        )
    }
}

@Composable
private fun RevealedBody(
    words: List<String>,
    secondsLeft: Int,
    onDone: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 16.dp)
            .padding(top = 16.dp, bottom = 32.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "Write down these 12 words in order.",
                color = colors.onDarkMuted,
                fontSize = 14.sp,
                modifier = Modifier.weight(1f),
            )
            Text(
                text = countdownText(secondsLeft),
                color = colors.onDarkMuted,
                fontSize = 12.sp,
            )
        }
        MnemonicWordGrid(
            words = words,
            modifier = Modifier.padding(top = 24.dp),
        )
        SettingsCta(
            label = "Done",
            background = colors.darkElevated,
            contentColor = colors.onDark,
            onClick = onDone,
            modifier = Modifier.padding(top = 40.dp),
        )
    }
}

/** The PWA's `MnemonicWordGrid`: 2 columns of numbered mono word chips. */
@Composable
private fun MnemonicWordGrid(words: List<String>, modifier: Modifier = Modifier) {
    val colors = ZinqqTheme.colors
    Column(modifier = modifier, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        words.chunked(2).forEachIndexed { rowIndex, pair ->
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                pair.forEachIndexed { colIndex, word ->
                    Row(
                        modifier = Modifier
                            .weight(1f)
                            .clip(RoundedCornerShape(12.dp))
                            .background(colors.darkElevated)
                            .padding(horizontal = 16.dp, vertical = 12.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            text = "${rowIndex * 2 + colIndex + 1}.",
                            color = colors.onDarkMuted,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 14.sp,
                            textAlign = TextAlign.End,
                            modifier = Modifier.width(24.dp),
                        )
                        Text(
                            text = word,
                            color = colors.onDark,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 14.sp,
                            modifier = Modifier.padding(start = 8.dp),
                        )
                    }
                }
                if (pair.size == 1) Box(modifier = Modifier.weight(1f))
            }
        }
    }
}

/** Unwrap the hosting [Activity] through any context wrappers. */
internal fun Context.findActivity(): Activity? =
    generateSequence(this) { (it as? ContextWrapper)?.baseContext }
        .filterIsInstance<Activity>()
        .firstOrNull()
