package zinqq.app.components

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import kotlinx.coroutines.delay

/**
 * The PWA's "Copied!" flash: a boolean the call site raises after writing to
 * the clipboard, auto-cleared [durationMs] later (each screen keeps its own
 * PWA duration — 1,500 or 2,000 ms).
 */
@Composable
fun rememberCopiedFlash(durationMs: Long): MutableState<Boolean> {
    val copied = remember { mutableStateOf(false) }
    LaunchedEffect(copied.value) {
        if (copied.value) {
            delay(durationMs)
            copied.value = false
        }
    }
    return copied
}
