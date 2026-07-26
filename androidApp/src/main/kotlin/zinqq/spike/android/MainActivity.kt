package zinqq.spike.android

import android.graphics.Bitmap
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.QRCodeWriter
import kotlinx.coroutines.delay

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // The holder is process-scoped, so this activity only reads it — it
        // neither creates nor stops the node (see WalletHolder).
        val holder = (application as SpikeApplication).walletHolder
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    WalletScreen(holder)
                }
            }
        }
    }
}

@Composable
private fun WalletScreen(holder: WalletHolder) {
    val state by holder.state.collectAsState()
    var receiveAmount by remember { mutableStateOf("") }
    var sendBolt11 by remember { mutableStateOf("") }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = "${state.balanceMsat / 1_000uL} sats",
                    style = MaterialTheme.typography.headlineMedium,
                )
                Text(
                    text = "on-chain ${state.onchainSats} sats · " +
                        "node ${if (state.nodeRunning) "running" else "stopped"}",
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            TextButton(onClick = holder::refreshBalances) { Text("Refresh") }
        }
        state.syncBanner?.let {
            Text(text = it, color = MaterialTheme.colorScheme.error)
        }

        HorizontalDivider()
        Text(text = "Receive", style = MaterialTheme.typography.titleMedium)
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            OutlinedTextField(
                value = receiveAmount,
                onValueChange = { receiveAmount = it },
                label = { Text("Amount (sats)") },
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            val parsedAmount = remember(receiveAmount) { receiveAmount.toULongOrNull() }
            Button(
                onClick = { parsedAmount?.let(holder::requestInvoice) },
                enabled = state.nodeRunning && parsedAmount != null,
            ) { Text("Invoice") }
        }
        state.currentInvoice?.let { InvoiceDisplay(it) }

        HorizontalDivider()
        Text(text = "Send", style = MaterialTheme.typography.titleMedium)
        OutlinedTextField(
            value = sendBolt11,
            onValueChange = { sendBolt11 = it },
            label = { Text("BOLT11 invoice") },
            modifier = Modifier.fillMaxWidth(),
        )
        Button(
            onClick = { holder.sendPayment(sendBolt11) },
            enabled = state.nodeRunning && sendBolt11.isNotBlank(),
        ) { Text("Pay") }

        state.lastOutcome?.let {
            HorizontalDivider()
            Text(text = it, style = MaterialTheme.typography.bodyMedium)
        }
    }
}

@Composable
private fun ColumnScope.InvoiceDisplay(invoice: InvoiceUi) {
    // The BOLT11 is opaque display data here (R4): it goes straight from the
    // InvoiceReady event into pixels.
    val qr = remember(invoice.bolt11) { qrBitmap(invoice.bolt11) }
    Image(
        bitmap = qr.asImageBitmap(),
        contentDescription = "Invoice QR code",
        modifier = Modifier
            .size(240.dp)
            .align(Alignment.CenterHorizontally),
    )
    ExpiryCountdown(expiryUnixSecs = invoice.expiryUnixSecs)
    Text(text = invoice.bolt11, style = MaterialTheme.typography.bodySmall)
}

@Composable
private fun ExpiryCountdown(expiryUnixSecs: ULong) {
    var remaining by remember(expiryUnixSecs) { mutableLongStateOf(secsUntil(expiryUnixSecs)) }
    LaunchedEffect(expiryUnixSecs) {
        while (remaining > 0) {
            delay(1_000)
            remaining = secsUntil(expiryUnixSecs)
        }
    }
    Text(
        text = if (remaining > 0) {
            "Expires in ${remaining / 60}m ${remaining % 60}s"
        } else {
            "Invoice expired"
        },
        style = MaterialTheme.typography.bodyMedium,
    )
}

private fun secsUntil(expiryUnixSecs: ULong): Long =
    expiryUnixSecs.toLong() - System.currentTimeMillis() / 1_000

private fun qrBitmap(text: String, sizePx: Int = 512): Bitmap {
    val matrix = QRCodeWriter().encode(
        text,
        BarcodeFormat.QR_CODE,
        sizePx,
        sizePx,
        mapOf(EncodeHintType.MARGIN to 1),
    )
    val pixels = IntArray(sizePx * sizePx) { i ->
        if (matrix.get(i % sizePx, i / sizePx)) 0xFF000000.toInt() else 0xFFFFFFFF.toInt()
    }
    return Bitmap.createBitmap(pixels, sizePx, sizePx, Bitmap.Config.ARGB_8888)
}
