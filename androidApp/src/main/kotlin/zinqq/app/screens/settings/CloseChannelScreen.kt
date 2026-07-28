package zinqq.app.screens.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.border
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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch
import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.CloseEstimate
import zinqq.app.R
import zinqq.app.components.CenteredNote
import zinqq.app.nav.ScreenHeader
import zinqq.app.theme.ZinqqTheme
import zinqq.main.formatBtc
import zinqq.main.msatToSatFloor

/** The PWA CloseChannel's step machine (`CloseChannel.tsx:27-30`). */
private sealed interface CloseStep {
    data class Confirm(val channel: ChannelView, val force: Boolean) : CloseStep
    data class Success(val force: Boolean) : CloseStep
    data class Error(val failure: CloseFailureUi, val channel: ChannelView) : CloseStep
}

/**
 * The PWA's CloseChannel (U17, R10 UI; `CloseChannel.tsx`): the confirm
 * screen with the Cooperative / Force Close toggle, the informational
 * estimate that never blocks closing (nullable-safe placeholders), the
 * non-anchor and in-flight warnings, the method-colored CTA, the success
 * screen with "Track Progress" into the close detail, and the coop-failure
 * "Force Close Instead" escalation.
 */
@Composable
fun CloseChannelScreen(
    port: SettingsPort,
    channelId: String?,
    initialForce: Boolean,
    onBack: () -> Unit,
    onTrackProgress: (String) -> Unit,
    onDone: () -> Unit,
    onMissingChannel: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    val scope = rememberCoroutineScope()

    // Guard: no channel in route state → back to Peers (CloseChannel.tsx:57-62).
    if (channelId == null) {
        LaunchedEffect(Unit) { onMissingChannel() }
        return
    }

    var step by remember { mutableStateOf<CloseStep?>(null) }
    var isClosing by remember { mutableStateOf(false) }
    var estimate by remember { mutableStateOf<CloseEstimate?>(null) }
    var estimateLoading by remember { mutableStateOf(true) }
    var lookupFailed by remember { mutableStateOf(false) }

    // Channel lookup + best-effort estimate (informational only — a failure
    // leaves placeholders; closing is never blocked, CloseChannel.tsx:49-54).
    LaunchedEffect(channelId) {
        val match = try {
            port.listChannels().firstOrNull { it.channelId == channelId }
        } catch (_: Exception) {
            null
        }
        if (match == null) {
            lookupFailed = true
            return@LaunchedEffect
        }
        step = CloseStep.Confirm(match, force = initialForce)
        estimate = try {
            port.estimateClose(channelId)
        } catch (_: Exception) {
            null
        }
        estimateLoading = false
    }

    if (lookupFailed) {
        LaunchedEffect(Unit) { onMissingChannel() }
        return
    }

    when (val current = step) {
        null -> Column(
            modifier = Modifier
                .fillMaxSize()
                .background(colors.dark),
        ) {
            ScreenHeader(title = "Close Channel", onBack = onBack, tint = colors.onDark)
            CenteredNote("Loading...")
        }

        is CloseStep.Confirm -> ConfirmBody(
            channel = current.channel,
            force = current.force,
            estimate = estimate,
            estimateLoading = estimateLoading,
            isClosing = isClosing,
            onBack = onBack,
            onSetForce = { step = current.copy(force = it) },
            onConfirm = {
                if (!isClosing) {
                    isClosing = true
                    scope.launch {
                        step = try {
                            port.closeChannel(current.channel.channelId, current.force)
                            CloseStep.Success(current.force)
                        } catch (e: Exception) {
                            CloseStep.Error(
                                failure = closeFailure(e, force = current.force),
                                channel = current.channel,
                            )
                        }
                        isClosing = false
                    }
                }
            },
        )

        is CloseStep.Success -> SuccessBody(
            force = current.force,
            estimate = estimate,
            onTrackProgress = { onTrackProgress(channelId) },
            onDone = onDone,
        )

        is CloseStep.Error -> ErrorBody(
            failure = current.failure,
            onForceCloseInstead = {
                step = CloseStep.Confirm(current.channel, force = true)
            },
            onTryAgain = {
                step = CloseStep.Confirm(current.channel, force = false)
            },
        )
    }
}

@Composable
private fun ConfirmBody(
    channel: ChannelView,
    force: Boolean,
    estimate: CloseEstimate?,
    estimateLoading: Boolean,
    isClosing: Boolean,
    onBack: () -> Unit,
    onSetForce: (Boolean) -> Unit,
    onConfirm: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark),
    ) {
        ScreenHeader(title = "Close Channel", onBack = onBack, tint = colors.onDark)
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .weight(1f)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .padding(top = 16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            SettingsFactRow(
                label = "Peer",
                value = reviewPeerDisplay(channel.counterpartyPubkey),
                mono = true,
            )
            SettingsFactRow(
                label = "Channel Capacity",
                value = formatBtc(channel.capacitySats.toLong()),
            )
            SettingsFactRow(
                label = "Your Balance",
                value = formatBtc(msatToSatFloor(channel.outboundMsat.toLong())),
            )
            SettingsFactRow(
                label = "Remote Balance",
                value = formatBtc(msatToSatFloor(channel.inboundMsat.toLong())),
            )

            HorizontalDivider(color = colors.darkBorder)

            SettingsFactRow(
                label = "You Get Back",
                value = expectedBackLabel(estimate, estimateLoading),
            )
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                SettingsFactRow(
                    label = "Estimated Cost to You",
                    value = closeCostLabel(estimate, force, estimateLoading),
                )
                if (lspPaysCloseFee(estimate, force)) {
                    Text(text = LSP_PAYS_NOTE, color = colors.onDarkMuted, fontSize = 12.sp)
                }
                Text(text = ESTIMATE_CAVEAT, color = colors.onDarkMuted, fontSize = 12.sp)
            }
            SettingsFactRow(
                label = "Funds Available",
                value = closeTimelineLabel(estimate, force),
            )

            HorizontalDivider(color = colors.darkBorder)

            // Close-method toggle (CloseChannel.tsx:357-384).
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    text = "Close Method",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    MethodButton(
                        label = "Cooperative",
                        selected = !force,
                        selectedBackground = colors.cta,
                        selectedContent = colors.onCta,
                        onClick = { onSetForce(false) },
                        modifier = Modifier.weight(1f),
                    )
                    MethodButton(
                        label = "Force Close",
                        selected = force,
                        selectedBackground = colors.danger.copy(alpha = 0.15f),
                        selectedContent = colors.danger,
                        onClick = { onSetForce(true) },
                        modifier = Modifier.weight(1f),
                    )
                }
            }

            // Info / warning boxes (CloseChannel.tsx:386-413).
            if (force) {
                NoteBox(
                    text = forceCloseInfoText(estimate),
                    background = colors.danger.copy(alpha = 0.1f),
                    contentColor = colors.danger,
                )
            } else {
                NoteBox(
                    text = COOP_CLOSE_INFO,
                    background = colors.darkElevated,
                    contentColor = colors.onDarkMuted,
                )
            }
            if (showsNonAnchorWarning(estimate, force)) {
                NoteBox(
                    text = NON_ANCHOR_WARNING,
                    background = colors.warning.copy(alpha = 0.1f),
                    contentColor = colors.warning,
                )
            }
            pendingHtlcWarning(estimate)?.let {
                NoteBox(
                    text = it,
                    background = colors.warning.copy(alpha = 0.1f),
                    contentColor = colors.warning,
                )
            }
        }
        SettingsCta(
            label = closeCtaLabel(force, isClosing),
            background = if (force) colors.dangerStrong else colors.hot,
            contentColor = if (force) Color.White else colors.onHot,
            enabled = !isClosing,
            disabledAlpha = 0.3f,
            onClick = onConfirm,
            modifier = Modifier.padding(horizontal = 24.dp, vertical = 24.dp),
        )
    }
}

@Composable
private fun SuccessBody(
    force: Boolean,
    estimate: CloseEstimate?,
    onTrackProgress: () -> Unit,
    onDone: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark)
            .padding(horizontal = 32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Box(
            modifier = Modifier
                .size(80.dp)
                .clip(CircleShape)
                .background(colors.badge),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_check),
                contentDescription = "Success",
                tint = colors.onBadge,
                modifier = Modifier.size(40.dp),
            )
        }
        Text(
            text = "Channel Closing",
            color = colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 24.sp,
            modifier = Modifier.padding(top = 24.dp),
        )
        Text(
            text = closeSuccessDetail(force, estimate),
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 8.dp),
        )
        SettingsCta(
            label = "Track Progress",
            background = colors.cta,
            contentColor = colors.onCta,
            onClick = onTrackProgress,
            modifier = Modifier.padding(top = 32.dp),
        )
        OutlineCta(
            label = "Done",
            onClick = onDone,
            borderColor = colors.darkBorder,
            contentColor = colors.onDark,
            modifier = Modifier.padding(top = 12.dp),
        )
    }
}

@Composable
private fun ErrorBody(
    failure: CloseFailureUi,
    onForceCloseInstead: () -> Unit,
    onTryAgain: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(colors.dark)
            .padding(horizontal = 32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Box(
            modifier = Modifier
                .size(80.dp)
                .clip(CircleShape)
                .background(colors.danger.copy(alpha = 0.15f)),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_x_close),
                contentDescription = "Failure",
                tint = colors.danger,
                modifier = Modifier.size(40.dp),
            )
        }
        Text(
            text = "Close Failed",
            color = colors.onDark,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 24.sp,
            modifier = Modifier.padding(top = 24.dp),
        )
        Text(
            text = failure.message,
            color = colors.danger,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(top = 8.dp),
        )
        Text(
            text = "Your funds are safe.",
            color = colors.onDarkMuted,
            fontSize = 14.sp,
            modifier = Modifier.padding(top = 4.dp),
        )
        if (failure.canForceClose) {
            OutlineCta(
                label = "Force Close Instead",
                onClick = onForceCloseInstead,
                borderColor = colors.danger,
                contentColor = colors.danger,
                modifier = Modifier.padding(top = 32.dp),
            )
        }
        SettingsCta(
            label = "Try Again",
            background = colors.cta,
            contentColor = colors.onCta,
            onClick = onTryAgain,
            modifier = Modifier.padding(top = if (failure.canForceClose) 12.dp else 32.dp),
        )
    }
}

@Composable
private fun MethodButton(
    label: String,
    selected: Boolean,
    selectedBackground: Color,
    selectedContent: Color,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = ZinqqTheme.colors
    Box(
        modifier = modifier
            .height(44.dp)
            .clip(RoundedCornerShape(8.dp))
            .background(if (selected) selectedBackground else colors.darkElevated)
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label,
            color = if (selected) selectedContent else colors.onDarkMuted,
            fontSize = 14.sp,
            fontWeight = FontWeight.SemiBold,
        )
    }
}

@Composable
private fun NoteBox(text: String, background: Color, contentColor: Color) {
    Text(
        text = text,
        color = contentColor,
        fontSize = 14.sp,
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(8.dp))
            .background(background)
            .padding(12.dp),
    )
}

@Composable
private fun OutlineCta(
    label: String,
    onClick: () -> Unit,
    borderColor: Color,
    contentColor: Color,
    modifier: Modifier = Modifier,
) {
    Box(
        modifier = modifier
            .fillMaxWidth()
            .height(56.dp)
            .clip(RoundedCornerShape(12.dp))
            .border(2.dp, borderColor, RoundedCornerShape(12.dp))
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = label,
            color = contentColor,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 17.sp,
        )
    }
}
