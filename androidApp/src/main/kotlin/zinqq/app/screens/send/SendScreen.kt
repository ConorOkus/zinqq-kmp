package zinqq.app.screens.send

import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import zinqq.app.components.Numpad
import zinqq.app.components.ResultTemplate
import zinqq.app.explorerTxUrl
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme
import zinqq.main.formatBtc
import zinqq.main.msatToSatCeil

/**
 * The Send screen (U15, F1, R5/R7 UI): the PWA's six-step machine rendered
 * from [SendStep]. All protocol decisions arrive as core results through
 * [SendController]; this composable only places them (R14).
 *
 * [scannedInput] is the Scan screen's raw decode (R13) — it runs the exact
 * same classify path as typed/pasted input.
 */
@Composable
fun SendScreen(
    port: SendPort,
    scannedInput: String?,
    onDone: () -> Unit,
    onBackToHome: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val controller = remember { SendController(port, scope) }
    val step by controller.step.collectAsState()
    var inputValue by remember { mutableStateOf("") }

    LaunchedEffect(scannedInput) {
        if (!scannedInput.isNullOrBlank()) {
            inputValue = scannedInput.take(SEND_INPUT_MAX_LENGTH)
            controller.submitInput(scannedInput)
        }
    }

    when (val current = step) {
        is SendStep.Input -> InputStepScreen(
            value = inputValue,
            onValueChange = { inputValue = it.take(SEND_INPUT_MAX_LENGTH) },
            error = current.error,
            resolving = current.resolving,
            onNext = { controller.submitInput(inputValue) },
            onAbortResolve = controller::abortResolve,
            onPaste = { pasted ->
                inputValue = pasted.take(SEND_INPUT_MAX_LENGTH)
                controller.submitInput(pasted)
            },
            onBack = onBackToHome,
        )

        is SendStep.Amount -> AmountStepScreen(
            step = current,
            onchainBalanceSats = port.onchainBalanceSats(),
            unifiedTotalSats = unifiedTotalSats(
                port.onchainBalanceSats(),
                port.lightningCapacityMsat(),
            ),
            onKey = controller::onNumpadKey,
            onNext = controller::submitAmountStep,
            onSendMax = controller::setOnchainSendMax,
            onLnAvailable = controller::setLightningAvailable,
            onBack = controller::backToInput,
        )

        is SendStep.ReviewLightning -> LightningReviewScreen(
            step = current,
            onConfirm = controller::confirmLightning,
            onBack = controller::backFromReview,
        )

        is SendStep.ReviewOnchain -> OnchainReviewScreen(
            step = current,
            onConfirm = controller::confirmOnchain,
            onBack = controller::backFromReview,
        )

        is SendStep.Dispatching -> DispatchingScreen(amountMsat = current.amountMsat)

        is SendStep.Success -> {
            val context = LocalContext.current
            ResultTemplate(
                success = true,
                headline = formatBtc(current.amountSats.toLong()),
                detail = "sent successfully",
                onCta = onDone,
                ctaLabel = "Done",
                extraContent = current.txid?.let { txid ->
                    {
                        // PWA oc-success "View on explorer" (Send.tsx:871-884).
                        Text(
                            text = "View on explorer",
                            color = ZinqqTheme.colors.onDark,
                            fontSize = 14.sp,
                            modifier = Modifier
                                .clip(RoundedCornerShape(24.dp))
                                .clickable {
                                    context.startActivity(
                                        Intent(
                                            Intent.ACTION_VIEW,
                                            Uri.parse(explorerTxUrl(txid)),
                                        ),
                                    )
                                }
                                .padding(horizontal = 24.dp, vertical = 12.dp),
                        )
                    }
                },
            )
        }

        is SendStep.Failure -> ResultTemplate(
            success = false,
            headline = "Send Failed",
            detail = current.message,
            onCta = {
                val retry = current.retry
                if (retry != null) controller.retry(retry) else onDone()
            },
            ctaLabel = if (current.retry != null) "Try Again" else "Done",
        )

        is SendStep.TimedOut -> ResultTemplate(
            success = false,
            headline = "Payment is taking longer than expected",
            detail = "It may still complete — check Activity for the final status.",
            fundsAreSafe = false,
            onCta = onDone,
            ctaLabel = "Done",
        )
    }
}

// --- Recipient step (PWA Send.tsx:1146-1202) ---

@Composable
private fun InputStepScreen(
    value: String,
    onValueChange: (String) -> Unit,
    error: String?,
    resolving: Boolean,
    onNext: () -> Unit,
    onAbortResolve: () -> Unit,
    onPaste: (String) -> Unit,
    onBack: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    val clipboard = LocalClipboardManager.current
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Send", onBack = onBack)
        Column(
            modifier = Modifier
                .weight(1f)
                .padding(horizontal = 24.dp)
                .padding(top = 24.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = "Recipient",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                    modifier = Modifier.weight(1f),
                )
                Text(
                    text = "Paste",
                    color = colors.onDark,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                    modifier = Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .clickable(enabled = !resolving) {
                            clipboard.getText()?.text
                                ?.takeIf { it.isNotBlank() }
                                ?.let(onPaste)
                        }
                        .padding(horizontal = 12.dp, vertical = 8.dp)
                        .semantics { contentDescription = "Paste from clipboard" },
                )
            }
            Box(
                modifier = Modifier
                    .padding(top = 8.dp)
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(12.dp))
                    .background(colors.darkElevated)
                    .padding(horizontal = 16.dp, vertical = 14.dp),
            ) {
                BasicTextField(
                    value = value,
                    onValueChange = onValueChange,
                    enabled = !resolving,
                    textStyle = TextStyle(
                        color = colors.onDark,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 14.sp,
                    ),
                    cursorBrush = SolidColor(colors.hot),
                    modifier = Modifier
                        .fillMaxWidth()
                        .semantics { contentDescription = "Payment request or address" },
                    decorationBox = { innerTextField ->
                        if (value.isEmpty()) {
                            Text(
                                text = "payment request or user@domain",
                                color = colors.onDarkMuted,
                                fontFamily = FontFamily.Monospace,
                                fontSize = 14.sp,
                            )
                        }
                        innerTextField()
                    },
                )
            }
            if (!error.isNullOrBlank()) {
                Text(
                    text = error,
                    color = colors.danger,
                    fontSize = 14.sp,
                    modifier = Modifier.padding(top = 8.dp),
                )
            }
        }
        CtaButton(
            label = if (resolving) "Resolving..." else "Next",
            enabled = value.isNotBlank() || resolving,
            showSpinner = resolving,
            onClick = if (resolving) onAbortResolve else onNext,
        )
    }
}

// --- Amount step (PWA Send.tsx:1095-1144) ---

@Composable
private fun AmountStepScreen(
    step: SendStep.Amount,
    onchainBalanceSats: ULong,
    unifiedTotalSats: ULong,
    onKey: (zinqq.main.NumpadKey) -> Unit,
    onNext: () -> Unit,
    onSendMax: () -> Unit,
    onLnAvailable: () -> Unit,
    onBack: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    val isOnchain = step.isOnchain
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Send", onBack = onBack)
        Column(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            if (isOnchain) {
                // "₿X available · Max" pill (Send.tsx:1103-1115).
                Text(
                    text = "${formatBtc(onchainBalanceSats.toLong())} available · Max",
                    color = if (step.isSendMax) colors.onPill else colors.onDarkMuted,
                    fontSize = 14.sp,
                    fontWeight = if (step.isSendMax) FontWeight.SemiBold else FontWeight.Normal,
                    modifier = Modifier
                        .clip(RoundedCornerShape(24.dp))
                        .background(if (step.isSendMax) colors.pill else colors.dark)
                        .clickable(onClick = onSendMax)
                        .padding(horizontal = 16.dp, vertical = 6.dp)
                        .semantics { contentDescription = "Send maximum" },
                )
            } else {
                // "₿X available" (Send.tsx:1116-1123).
                Text(
                    text = "${formatBtc(unifiedTotalSats.toLong())} available",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    modifier = Modifier
                        .clip(RoundedCornerShape(24.dp))
                        .clickable(onClick = onLnAvailable)
                        .padding(horizontal = 16.dp, vertical = 6.dp),
                )
            }
            Text(
                text = formatBtc(step.amountSats.toLong()),
                color = colors.amount,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = if (step.digits.length > 5) 48.sp else 72.sp,
                modifier = Modifier.padding(top = 8.dp),
            )
            val min = step.minSats
            val max = step.maxSats
            if (min != null || max != null) {
                // "Min ₿X · Max ₿X" (Send.tsx:1132-1138).
                val parts = buildList {
                    min?.let { add("Min ${formatBtc(it.toLong())}") }
                    max?.let { add("Max ${formatBtc(it.toLong())}") }
                }
                Text(
                    text = parts.joinToString(" · "),
                    color = colors.onDarkMuted,
                    fontSize = 12.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
            if (!step.error.isNullOrBlank()) {
                Text(
                    text = step.error,
                    color = colors.danger,
                    fontSize = 14.sp,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(top = 8.dp, start = 24.dp, end = 24.dp),
                )
            }
            if (step.fetchingInvoice) {
                CircularProgressIndicator(
                    color = colors.onDarkMuted,
                    strokeWidth = 2.dp,
                    modifier = Modifier
                        .padding(top = 12.dp)
                        .size(20.dp),
                )
            }
        }
        Numpad(
            onKey = onKey,
            onNext = onNext,
            nextEnabled = step.amountSats > 0uL && !step.fetchingInvoice,
        )
    }
}

// --- Lightning review (PWA Send.tsx:1063-1093) ---

@Composable
private fun LightningReviewScreen(
    step: SendStep.ReviewLightning,
    onConfirm: () -> Unit,
    onBack: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Review", onBack = onBack)
        Column(
            modifier = Modifier
                .weight(1f)
                .padding(horizontal = 24.dp)
                .padding(top = 32.dp),
            verticalArrangement = Arrangement.spacedBy(24.dp),
        ) {
            ReviewRow(label = "To") {
                Text(
                    text = step.recipient,
                    color = colors.onDark,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.SemiBold,
                    textAlign = TextAlign.End,
                )
            }
            ReviewRow(label = "Amount") {
                Text(
                    text = formatBtc(msatToSatCeil(step.amountMsat.toLong())),
                    color = colors.onDark,
                    fontFamily = ZinqqTheme.fonts.display,
                    fontWeight = FontWeight.Bold,
                    fontSize = 30.sp,
                )
            }
        }
        CtaButton(label = "Confirm Send", enabled = true, onClick = onConfirm)
    }
}

// --- On-chain review (PWA Send.tsx:988-1061) ---

@Composable
private fun OnchainReviewScreen(
    step: SendStep.ReviewOnchain,
    onConfirm: () -> Unit,
    onBack: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Review", onBack = onBack)
        Column(
            modifier = Modifier
                .weight(1f)
                .padding(horizontal = 24.dp)
                .padding(top = 32.dp),
            verticalArrangement = Arrangement.spacedBy(24.dp),
        ) {
            if (step.amountsUpdated) {
                // R5 drift banner (Send.tsx:995-1002), verbatim.
                Text(
                    text = "Amounts were updated — conditions changed since your last review.",
                    color = colors.warning,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                    modifier = Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(8.dp))
                        .background(colors.warning.copy(alpha = 0.1f))
                        .padding(horizontal = 12.dp, vertical = 8.dp),
                )
            }
            if (step.isSendMax) {
                Text(
                    text = "Sending all available onchain funds",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                )
            }
            ReviewRow(label = "To") {
                Text(
                    text = onchainRecipientLabel(step.address),
                    color = colors.onDark,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.SemiBold,
                    textAlign = TextAlign.End,
                )
            }
            ReviewRow(label = "Amount") {
                Text(
                    text = formatBtc(step.amountSats.toLong()),
                    color = colors.onDark,
                    fontWeight = FontWeight.SemiBold,
                )
            }
            Column {
                ReviewRow(label = "Network fee (${step.feeRateSatPerVb} sat/vB)") {
                    Text(
                        text = formatBtc(step.feeSats.toLong()),
                        color = colors.onDark,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
                if (step.isSendMax && step.reserveSats > 0uL) {
                    Text(
                        text = "Final fee may vary slightly",
                        color = colors.onDarkMuted,
                        fontSize = 12.sp,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                }
            }
            if (step.isSendMax && step.reserveSats > 0uL) {
                ReviewRow(label = "Kept for Lightning channel safety") {
                    Text(
                        text = formatBtc(step.reserveSats.toLong()),
                        color = colors.onDark,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
            }
            HorizontalDivider(color = colors.darkBorder)
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = "Total",
                    color = colors.onDark,
                    fontSize = 18.sp,
                    fontWeight = FontWeight.SemiBold,
                )
                Text(
                    text = formatBtc(step.totalSats.toLong()),
                    color = colors.onDark,
                    fontFamily = ZinqqTheme.fonts.display,
                    fontWeight = FontWeight.Bold,
                    fontSize = 30.sp,
                )
            }
        }
        CtaButton(
            label = if (step.broadcasting) "Sending…" else "Confirm Send",
            enabled = !step.broadcasting,
            showSpinner = step.broadcasting,
            onClick = onConfirm,
        )
    }
}

// --- Dispatching (PWA ln-sending, Send.tsx:946-986; no cancel: the core
// exposes no abandon FFI, so the flow waits for the outcome event) ---

@Composable
private fun DispatchingScreen(amountMsat: ULong) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        CircularProgressIndicator(
            color = colors.onDark,
            strokeWidth = 2.dp,
            modifier = Modifier.size(32.dp),
        )
        Text(
            text = "Sending payment...",
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            modifier = Modifier.padding(top = 16.dp),
        )
        Text(
            text = formatBtc(msatToSatCeil(amountMsat.toLong())),
            color = colors.onDarkMuted,
            fontSize = 12.sp,
            modifier = Modifier.padding(top = 4.dp),
        )
    }
}

// --- Shared bits ---

@Composable
private fun ReviewRow(label: String, value: @Composable () -> Unit) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = label,
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            fontWeight = FontWeight.Medium,
            modifier = Modifier.padding(end = 16.dp),
        )
        Box(modifier = Modifier.weight(1f), contentAlignment = Alignment.CenterEnd) {
            value()
        }
    }
}

@Composable
private fun CtaButton(
    label: String,
    enabled: Boolean,
    onClick: () -> Unit,
    showSpinner: Boolean = false,
) {
    val colors = ZinqqTheme.colors
    Box(modifier = Modifier.padding(horizontal = 24.dp, vertical = 24.dp)) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(56.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(colors.cta)
                .alpha(if (enabled) 1f else 0.3f)
                .clickable(enabled = enabled, onClick = onClick)
                .semantics { contentDescription = label },
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.Center,
        ) {
            if (showSpinner) {
                CircularProgressIndicator(
                    color = colors.onCta,
                    strokeWidth = 2.dp,
                    modifier = Modifier
                        .padding(end = 8.dp)
                        .size(20.dp),
                )
            }
            Text(
                text = label.uppercase(),
                color = colors.onCta,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = 18.sp,
                letterSpacing = 1.sp,
            )
        }
    }
}
