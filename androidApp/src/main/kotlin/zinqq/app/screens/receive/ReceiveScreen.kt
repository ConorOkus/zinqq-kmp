package zinqq.app.screens.receive

import android.content.Intent
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
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.delay
import zinqq.app.R
import zinqq.app.components.BottomSheet
import zinqq.app.components.Numpad
import zinqq.app.components.QrView
import zinqq.app.components.ResultTemplate
import zinqq.app.components.rememberCopiedFlash
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme
import zinqq.spike.formatBtc

/**
 * The Receive screen (U16, F2, R6 UI, R12): the PWA's `Receive.tsx` overlay
 * as a dedicated route — same z-order semantics (no TabBar here, fenced still
 * covers it), same machine. All liquidity decisions arrive as core results
 * through [ReceiveController]; this composable only places them (R14).
 */
@Composable
fun ReceiveScreen(
    port: ReceivePort,
    onClose: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val controller = remember { ReceiveController(port, scope) }
    LaunchedEffect(controller) { controller.start() }
    val state by controller.state.collectAsState()
    val colors = ZinqqTheme.colors

    // Success screen (PWA Receive.tsx:604-637).
    val received = state.step as? ReceiveStep.Received
    if (received != null) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.dark),
        ) {
            ScreenHeader(title = "Request", onBack = onClose)
            ResultTemplate(
                success = true,
                headline = "Payment received",
                onCta = onClose,
                ctaLabel = "Done",
                modifier = Modifier.weight(1f),
                extraContent = {
                    Text(
                        text = formatBtc(received.amountSats.toLong()),
                        color = colors.onDark,
                        fontFamily = ZinqqTheme.fonts.display,
                        fontWeight = FontWeight.Bold,
                        fontSize = 34.sp,
                    )
                },
            )
        }
        return
    }

    if (state.loading) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.dark),
        ) {
            ScreenHeader(title = "Request", onBack = onClose)
            Box(modifier = Modifier.weight(1f), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(
                    color = colors.onDark,
                    strokeWidth = 2.dp,
                    modifier = Modifier.size(32.dp),
                )
            }
        }
        return
    }

    // PWA Receive.tsx:592-602: fatal entry failure.
    val loadError = state.loadError
    if (loadError != null) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.dark),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Text(
                text = "Failed to load wallet",
                color = colors.onDark,
                fontSize = 18.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                text = loadError,
                color = colors.danger,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 8.dp, start = 24.dp, end = 24.dp),
            )
            Text(
                text = "Close",
                color = colors.onDark,
                fontSize = 14.sp,
                modifier = Modifier
                    .padding(top = 24.dp)
                    .clip(RoundedCornerShape(8.dp))
                    .clickable(onClick = onClose)
                    .padding(horizontal = 16.dp, vertical = 8.dp),
            )
        }
        return
    }

    var showSheet by remember { mutableStateOf(false) }
    var page by remember { mutableStateOf(QrPage.UNIFIED) }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(
            title = "Request",
            onBack = onClose,
            rightAction = if (
                headerCopyVisible(state.address != null, state.editingAmount, state.step)
            ) {
                {
                    IconButton(
                        onClick = { showSheet = true },
                        modifier = Modifier
                            .size(44.dp)
                            .clip(CircleShape)
                            .semantics { contentDescription = "Copy payment request" },
                    ) {
                        Icon(
                            painter = painterResource(R.drawable.ic_copy),
                            contentDescription = null,
                            tint = colors.onDark,
                        )
                    }
                }
            } else {
                null
            },
        )

        when {
            state.step is ReceiveStep.Quoting -> QuotingSkeleton(state.confirmedAmountSats)

            state.step is ReceiveStep.JitReview || state.step is ReceiveStep.JitBelowMinimum ->
                JitReviewScreen(
                    step = state.step,
                    onGenerate = controller::generateInvoice,
                    onBack = controller::backFromReview,
                )

            state.step is ReceiveStep.Buying -> CenteredStatus("Generating payment request…")

            showExpiredScreen(state.step, state.editingAmount) -> ExpiredScreen(
                onRetry = controller::retryRequest,
                onBack = controller::backFromReview,
            )

            state.step is ReceiveStep.JitError -> JitErrorScreen(
                onRetry = controller::retryRequest,
                onBack = controller::backFromReview,
            )

            state.editingAmount -> AmountEntry(state, controller)

            else -> QrDisplay(
                state = state,
                page = page,
                onPageChanged = { page = it },
                onEditAmount = controller::editAmount,
                nowUnixSecs = { System.currentTimeMillis() / 1_000 },
            )
        }
    }

    // Copy bottom sheet (PWA Receive.tsx:1026-1039); 2,000 ms feedback.
    if (showSheet) {
        val clipboard = LocalClipboardManager.current
        var copied by rememberCopiedFlash(COPY_FEEDBACK_MS)
        val value = copyValue(page, state.bip321Uri, state.offer)
        BottomSheet(open = true, onClose = { showSheet = false }) {
            Text(
                text = copySheetTitle(page),
                color = colors.onDark,
                fontSize = 14.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                text = value,
                color = colors.onDarkMuted,
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
                modifier = Modifier.padding(top = 12.dp),
            )
            Row(
                modifier = Modifier
                    .padding(top = 16.dp)
                    .fillMaxWidth()
                    .height(48.dp)
                    .clip(RoundedCornerShape(12.dp))
                    .background(colors.pill)
                    .clickable {
                        clipboard.setText(AnnotatedString(value))
                        copied = true
                    }
                    .semantics { contentDescription = "Copy" },
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.Center,
            ) {
                Text(
                    text = if (copied) "Copied!" else "Copy",
                    color = colors.onPill,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.SemiBold,
                )
            }
        }
    }
}

// --- QR display (PWA Receive.tsx:931-1024) ---

@Composable
private fun QrDisplay(
    state: ReceiveUiState,
    page: QrPage,
    onPageChanged: (QrPage) -> Unit,
    onEditAmount: () -> Unit,
    nowUnixSecs: () -> Long,
) {
    val colors = ZinqqTheme.colors
    val context = LocalContext.current
    val showBolt12 = showBolt12Page(state.offerQrValue != null, state.needsAmount)
    val pagerState = rememberPagerState(pageCount = { if (showBolt12) 2 else 1 })

    // Reset to the unified page when the BOLT12 page is removed (PWA:373-375).
    LaunchedEffect(showBolt12) {
        if (!showBolt12 && pagerState.currentPage != 0) pagerState.scrollToPage(0)
    }
    LaunchedEffect(pagerState.currentPage) {
        onPageChanged(if (pagerState.currentPage == 1) QrPage.BOLT12 else QrPage.UNIFIED)
    }

    val invoicePath = (state.step as? ReceiveStep.Display)?.invoicePath ?: InvoicePath.NONE

    Column(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .padding(horizontal = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            state.invoiceError?.let {
                Text(
                    text = it,
                    color = colors.danger,
                    fontSize = 14.sp,
                    modifier = Modifier.padding(bottom = 16.dp),
                )
            }

            if (state.address != null) {
                if (state.confirmedAmountSats > 0uL) {
                    // Tappable amount above the QR (PWA:939-948).
                    Text(
                        text = formatBtc(state.confirmedAmountSats.toLong()),
                        color = colors.onDark,
                        fontFamily = ZinqqTheme.fonts.display,
                        fontWeight = FontWeight.Bold,
                        fontSize = 18.sp,
                        modifier = Modifier
                            .clip(RoundedCornerShape(8.dp))
                            .clickable(onClick = onEditAmount)
                            .padding(horizontal = 12.dp, vertical = 4.dp),
                    )
                }

                HorizontalPager(
                    state = pagerState,
                    modifier = Modifier
                        .padding(top = 16.dp)
                        .fillMaxWidth()
                        .widthIn(max = 300.dp),
                ) { index ->
                    val payload =
                        if (index == 1) state.offerQrValue.orEmpty() else state.qrValue
                    QrView(
                        payload = payload,
                        contentDescription = if (index == 1) {
                            "QR code for BOLT 12 offer"
                        } else {
                            "QR code for Bitcoin address ${state.address}"
                        },
                        modifier = Modifier.padding(horizontal = 16.dp),
                    )
                }

                // Dot indicators (PWA:980-989).
                if (showBolt12) {
                    Row(
                        modifier = Modifier.padding(top = 16.dp),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        QrPage.entries.forEach { dot ->
                            Box(
                                modifier = Modifier
                                    .size(8.dp)
                                    .clip(CircleShape)
                                    .background(
                                        if (dot == page) colors.onDark else colors.dotIdle,
                                    ),
                            )
                        }
                    }
                }

                Text(
                    text = qrCaption(page, invoicePath, state.openingFeeSats),
                    color = colors.onDarkMuted,
                    fontSize = 12.sp,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(top = 24.dp),
                )

                // Expiry countdown over a JIT invoice (R6), ticking locally;
                // the controller owns the flip to the expired step.
                if (countdownVisible(state.step, state.editingAmount, state.expiresAtUnix)) {
                    val expiresAt = state.expiresAtUnix ?: 0uL
                    var secondsLeft by remember(expiresAt) {
                        mutableLongStateOf(countdownSecondsLeft(expiresAt, nowUnixSecs()))
                    }
                    LaunchedEffect(expiresAt) {
                        while (secondsLeft > 0) {
                            delay(1_000)
                            secondsLeft = countdownSecondsLeft(expiresAt, nowUnixSecs())
                        }
                    }
                    Text(
                        text = countdownText(secondsLeft),
                        color = colors.onDarkMuted,
                        fontSize = 12.sp,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                }
            }
        }

        if (state.address != null) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 24.dp)
                    .padding(bottom = 24.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                SecondaryButton(
                    label = if (state.confirmedAmountSats > 0uL) "Edit amount" else "Add amount",
                    onClick = onEditAmount,
                )
                // System share = the PWA's navigator.share (R12: platform
                // share sheet is a sanctioned deviation).
                SecondaryButton(
                    label = "Share",
                    onClick = {
                        val send = Intent(Intent.ACTION_SEND).apply {
                            type = "text/plain"
                            putExtra(
                                Intent.EXTRA_TEXT,
                                copyValue(page, state.bip321Uri, state.offer),
                            )
                        }
                        context.startActivity(Intent.createChooser(send, null))
                    },
                )
            }
        }
    }
}

// --- amount entry (PWA Receive.tsx:893-930) ---

@Composable
private fun AmountEntry(state: ReceiveUiState, controller: ReceiveController) {
    val colors = ZinqqTheme.colors
    val needsJit = editingNeedsJit(state.usableInboundMsat, state.editingAmountSats)
    val belowMin = belowJitMinimum(needsJit, state.editingAmountSats, state.floorSats)

    Column(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .padding(horizontal = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            if (!state.needsAmount || state.confirmedAmountSats > 0uL) {
                Text(
                    text = "Cancel",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    modifier = Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .clickable(onClick = controller::cancelAmount)
                        .padding(horizontal = 12.dp, vertical = 8.dp),
                )
            }
            Text(
                text = formatBtc(state.editingAmountSats.toLong()),
                color = colors.amount,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = if (state.amountDigits.length > 5) 48.sp else 72.sp,
                modifier = Modifier.padding(vertical = 8.dp),
            )
            if (state.confirmedAmountSats > 0uL) {
                Text(
                    text = "Remove amount",
                    color = colors.danger,
                    fontSize = 14.sp,
                    modifier = Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .clickable(onClick = controller::removeAmount)
                        .padding(horizontal = 12.dp, vertical = 8.dp),
                )
            }
            if (belowMin) {
                // AE4: the below-floor block, PWA copy (Receive.tsx:918-921).
                Text(
                    text = minimumAlertText(state.floorSats),
                    color = colors.danger,
                    fontSize = 14.sp,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
        }
        Numpad(
            onKey = controller::onNumpadKey,
            onNext = controller::confirmAmount,
            nextEnabled = numpadNextEnabled(state.editingAmountSats, belowMin),
            nextLabel = numpadCtaLabel(state.needsAmount, state.confirmedAmountSats),
        )
    }
}

// --- JIT review + skeleton (PWA Receive.tsx:672-806) ---

@Composable
private fun QuotingSkeleton(amountSats: ULong) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier.fillMaxSize(),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Column(
            modifier = Modifier
                .widthIn(max = 300.dp)
                .fillMaxWidth()
                .padding(horizontal = 32.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            JitRow(label = "Amount") {
                Text(
                    text = formatBtc(amountSats.toLong()),
                    color = colors.onDark,
                    fontFamily = ZinqqTheme.fonts.display,
                    fontWeight = FontWeight.Bold,
                    fontSize = 18.sp,
                )
            }
            JitRow(label = "Setup fee") {
                Box(
                    modifier = Modifier
                        .size(width = 80.dp, height = 20.dp)
                        .clip(RoundedCornerShape(4.dp))
                        .background(colors.onDark.copy(alpha = 0.1f))
                        .semantics { contentDescription = "Loading setup fee" },
                )
            }
            HorizontalDivider(color = colors.darkBorder)
            JitRow(label = "You'll receive") {
                Box(
                    modifier = Modifier
                        .size(width = 96.dp, height = 20.dp)
                        .clip(RoundedCornerShape(4.dp))
                        .background(colors.onDark.copy(alpha = 0.1f))
                        .semantics { contentDescription = "Loading net amount" },
                )
            }
        }
        CircularProgressIndicator(
            color = colors.onDark,
            strokeWidth = 2.dp,
            modifier = Modifier
                .padding(top = 24.dp)
                .size(32.dp),
        )
    }
}

@Composable
private fun JitReviewScreen(
    step: ReceiveStep,
    onGenerate: () -> Unit,
    onBack: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth(),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            Column(
                modifier = Modifier
                    .widthIn(max = 300.dp)
                    .fillMaxWidth()
                    .padding(horizontal = 32.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                when (step) {
                    is ReceiveStep.JitReview -> {
                        JitRow(label = "Amount") {
                            AmountText(step.amountSats)
                        }
                        JitRow(label = "Setup fee") {
                            AmountText(step.setupFeeSats, prefix = "− ")
                        }
                        HorizontalDivider(color = colors.darkBorder)
                        JitRow(label = "You'll receive") {
                            AmountText(step.youReceiveSats)
                        }
                        // The PWA's fallback-provider warning slot
                        // (Receive.tsx:753-761) is intentionally absent: the
                        // core configures a single LSP, so quotes have no
                        // fallback role to disclose.
                    }

                    is ReceiveStep.JitBelowMinimum -> {
                        JitRow(label = "Amount") {
                            AmountText(step.amountSats)
                        }
                        HorizontalDivider(color = colors.darkBorder)
                        Text(
                            text = "Minimum receive: ${formatBtc(step.displayMinSats.toLong())}",
                            color = colors.onDarkMuted,
                            fontSize = 14.sp,
                        )
                    }

                    else -> Unit
                }
            }
        }
        BottomActions(
            primaryLabel = "Generate Payment Request",
            primaryEnabled = step is ReceiveStep.JitReview,
            onPrimary = onGenerate,
            onBack = onBack,
        )
    }
}

// --- expired / error (PWA Receive.tsx:814-892) ---

@Composable
private fun ExpiredScreen(onRetry: () -> Unit, onBack: () -> Unit) {
    val colors = ZinqqTheme.colors
    Column(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .padding(horizontal = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            StatusBadge(tint = colors.warning, icon = R.drawable.ic_clock)
            Text(
                text = "Payment request expired",
                color = colors.onDark,
                fontSize = 16.sp,
                fontWeight = FontWeight.SemiBold,
                modifier = Modifier.padding(top = 24.dp),
            )
            Text(
                text = "This request is no longer payable. Generate a new one to keep receiving.",
                color = colors.onDarkMuted,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 8.dp, start = 16.dp, end = 16.dp),
            )
        }
        BottomActions(
            primaryLabel = "Generate new request",
            primaryEnabled = true,
            onPrimary = onRetry,
            onBack = onBack,
        )
    }
}

@Composable
private fun JitErrorScreen(onRetry: () -> Unit, onBack: () -> Unit) {
    val colors = ZinqqTheme.colors
    Column(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .padding(horizontal = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            StatusBadge(tint = colors.danger, icon = R.drawable.ic_x_close)
            Text(
                text = "Could not generate payment request",
                color = colors.onDark,
                fontSize = 16.sp,
                fontWeight = FontWeight.SemiBold,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 24.dp),
            )
        }
        BottomActions(
            primaryLabel = "Try again",
            primaryEnabled = true,
            onPrimary = onRetry,
            onBack = onBack,
        )
    }
}

// --- shared bits ---

@Composable
private fun CenteredStatus(text: String) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier.fillMaxSize(),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Text(text = text, color = colors.onDarkMuted, fontSize = 14.sp)
        CircularProgressIndicator(
            color = colors.onDark,
            strokeWidth = 2.dp,
            modifier = Modifier
                .padding(top = 24.dp)
                .size(32.dp),
        )
    }
}

@Composable
private fun StatusBadge(tint: androidx.compose.ui.graphics.Color, icon: Int) {
    Box(
        modifier = Modifier
            .size(64.dp)
            .clip(CircleShape)
            .background(tint.copy(alpha = 0.15f)),
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            painter = painterResource(icon),
            contentDescription = null,
            tint = tint,
            modifier = Modifier.size(32.dp),
        )
    }
}

@Composable
private fun JitRow(label: String, value: @Composable () -> Unit) {
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
            modifier = Modifier.padding(end = 16.dp),
        )
        value()
    }
}

@Composable
private fun AmountText(sats: ULong, prefix: String = "") {
    Text(
        text = "$prefix${formatBtc(sats.toLong())}",
        color = ZinqqTheme.colors.onDark,
        fontFamily = ZinqqTheme.fonts.display,
        fontWeight = FontWeight.Bold,
        fontSize = 18.sp,
    )
}

@Composable
private fun SecondaryButton(label: String, onClick: () -> Unit) {
    val colors = ZinqqTheme.colors
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .height(56.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(colors.darkElevated)
            .clickable(onClick = onClick)
            .semantics { contentDescription = label },
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        Text(
            text = label,
            color = colors.onDark,
            fontSize = 14.sp,
            fontWeight = FontWeight.SemiBold,
        )
    }
}

@Composable
private fun BottomActions(
    primaryLabel: String,
    primaryEnabled: Boolean,
    onPrimary: () -> Unit,
    onBack: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 24.dp)
            .padding(bottom = 24.dp, top = 16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(56.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(colors.cta)
                .alpha(if (primaryEnabled) 1f else 0.7f)
                .clickable(enabled = primaryEnabled, onClick = onPrimary)
                .semantics { contentDescription = primaryLabel },
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.Center,
        ) {
            Text(
                text = primaryLabel,
                color = colors.onCta,
                fontFamily = ZinqqTheme.fonts.display,
                fontWeight = FontWeight.Bold,
                fontSize = 18.sp,
            )
        }
        SecondaryButton(label = "Back", onClick = onBack)
    }
}
