package zinqq.app.screens.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch
import uniffi.wallet_core.OpenFeeEstimate
import zinqq.app.components.Numpad
import zinqq.app.components.ResultTemplate
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme
import zinqq.main.formatBtc
import zinqq.main.numpadDigitReducer

/** The PWA OpenChannel's step machine (`OpenChannel.tsx:22-27`). */
private sealed interface OpenStep {
    data object Amount : OpenStep
    data class Reviewing(val amountSats: ULong, val fee: OpenFeeEstimate) : OpenStep
    data object Opening : OpenStep
    data object Success : OpenStep
    data class Failed(val message: String) : OpenStep
}

/**
 * The PWA's OpenChannel (U17, R10 UI; `OpenChannel.tsx`): numpad amount step
 * with the 20,000–16,777,215 bounds and balance gate, the Peer / Channel
 * Size / Est. fee / Total review, "Connect & Open Channel" (the core's
 * `open_channel` connects if needed), and the Channel Opening / failure
 * results. [peerAddress] arrives from the Peers connect form like the PWA's
 * `location.state`; missing state redirects back to Peers.
 */
@Composable
fun OpenChannelScreen(
    port: SettingsPort,
    peerAddress: String?,
    onBack: () -> Unit,
    onDone: () -> Unit,
    onMissingPeer: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    val scope = rememberCoroutineScope()

    // Guard: no peer in route state → back to Peers (OpenChannel.tsx:57-62).
    if (peerAddress == null || parsePeerAddress(peerAddress) !is PeerAddressParse.Valid) {
        LaunchedEffect(Unit) { onMissingPeer() }
        return
    }
    val peerPubkey = peerAddress.substringBefore('@')

    var step by remember { mutableStateOf<OpenStep>(OpenStep.Amount) }
    var digits by remember { mutableStateOf("") }
    var amountError by remember { mutableStateOf<String?>(null) }
    var fee by remember { mutableStateOf<OpenFeeEstimate?>(null) }

    // Fee estimate on entry; failures fall back like the PWA's getFeeRate
    // catch (1 sat/vB × 140 vB).
    LaunchedEffect(Unit) {
        fee = try {
            port.estimateOpenFee()
        } catch (_: Exception) {
            fallbackOpenFee()
        }
    }

    val amountSats = digits.toULongOrNull() ?: 0uL
    val balanceSats = port.onchainBalanceSats()

    when (val current = step) {
        is OpenStep.Amount -> Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.dark),
        ) {
            ScreenHeader(title = "Channel Size", onBack = onBack, tint = colors.onDark)
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .weight(1f),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                Text(
                    text = "${formatBtc(balanceSats.toLong())} available",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                )
                Text(
                    text = formatBtc(amountSats.toLong()),
                    color = colors.amount,
                    fontFamily = ZinqqTheme.fonts.display,
                    fontWeight = FontWeight.Bold,
                    fontSize = if (digits.length > 5) 48.sp else 72.sp,
                    modifier = Modifier.padding(top = 8.dp),
                )
                amountError?.let {
                    Text(
                        text = it,
                        color = colors.danger,
                        fontSize = 14.sp,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                }
            }
            Numpad(
                onKey = { key ->
                    amountError = null
                    digits = numpadDigitReducer(digits, key, OPEN_AMOUNT_MAX_DIGITS)
                },
                onNext = {
                    val estimate = fee ?: fallbackOpenFee()
                    val error = validateOpenAmount(
                        amountSats = amountSats,
                        estimatedFeeSats = estimate.estimatedFeeSats,
                        balanceSats = balanceSats,
                    )
                    if (error != null) {
                        amountError = error
                    } else {
                        step = OpenStep.Reviewing(amountSats, estimate)
                    }
                },
                nextEnabled = amountSats > 0uL,
            )
        }

        is OpenStep.Reviewing -> Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.dark),
        ) {
            ScreenHeader(
                title = "Review",
                onBack = { step = OpenStep.Amount },
                tint = colors.onDark,
            )
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .weight(1f)
                    .padding(horizontal = 24.dp)
                    .padding(top = 32.dp),
                verticalArrangement = Arrangement.spacedBy(24.dp),
            ) {
                SettingsFactRow(label = "Peer", value = reviewPeerDisplay(peerPubkey), mono = true)
                SettingsFactRow(
                    label = "Channel Size",
                    value = formatBtc(current.amountSats.toLong()),
                )
                SettingsFactRow(
                    label = openFeeRateLabel(current.fee.feeRateSatPerVb),
                    value = "≈ ${formatBtc(current.fee.estimatedFeeSats.toLong())}",
                )
                HorizontalDivider(color = colors.darkBorder)
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = "Total",
                        color = colors.onDark,
                        fontSize = 18.sp,
                        fontWeight = FontWeight.SemiBold,
                        modifier = Modifier.weight(1f),
                    )
                    Text(
                        text = "≈ " + formatBtc(
                            openTotalSats(
                                current.amountSats,
                                current.fee.estimatedFeeSats,
                            ).toLong(),
                        ),
                        color = colors.onDark,
                        fontFamily = ZinqqTheme.fonts.display,
                        fontWeight = FontWeight.Bold,
                        fontSize = 30.sp,
                    )
                }
            }
            SettingsCta(
                label = "Connect & Open Channel",
                background = colors.cta,
                contentColor = colors.onCta,
                onClick = {
                    step = OpenStep.Opening
                    scope.launch {
                        step = try {
                            port.openChannel(peerAddress, current.amountSats)
                            OpenStep.Success
                        } catch (e: Exception) {
                            OpenStep.Failed(openChannelErrorMessage(e))
                        }
                    }
                },
                modifier = Modifier.padding(horizontal = 24.dp, vertical = 24.dp),
            )
        }

        is OpenStep.Opening -> Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.dark)
                .padding(horizontal = 32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            CircularProgressIndicator(
                color = colors.onDark,
                strokeWidth = 4.dp,
                modifier = Modifier.size(40.dp),
            )
            Text(
                text = "Connecting to peer & opening channel...",
                color = colors.onDarkMuted,
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(top = 16.dp),
            )
        }

        is OpenStep.Success -> ResultTemplate(
            success = true,
            headline = "Channel Opening",
            detail = "Your channel is being set up. It will be ready once the funding " +
                "transaction confirms on-chain.",
            ctaLabel = "Done",
            onCta = onDone,
        )

        is OpenStep.Failed -> ResultTemplate(
            success = false,
            headline = "Channel Open Failed",
            detail = current.message,
            ctaLabel = "Try Again",
            onCta = {
                digits = ""
                amountError = null
                step = OpenStep.Amount
            },
        )
    }
}
