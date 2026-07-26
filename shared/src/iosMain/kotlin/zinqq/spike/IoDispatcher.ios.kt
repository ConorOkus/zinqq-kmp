package zinqq.spike

import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.Dispatchers

// Kotlin/Native has no Dispatchers.IO in commonMain; the blocking work happens
// inside the Rust core's own tokio runtime, so Default is the right pool here.
internal actual val ioDispatcher: CoroutineDispatcher = Dispatchers.Default
