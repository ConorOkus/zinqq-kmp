package zinqq.app.screens.settings

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
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
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.draw.drawBehind
import kotlinx.coroutines.launch
import uniffi.wallet_core.ChannelStateLabel
import uniffi.wallet_core.ChannelView
import uniffi.wallet_core.PeerView
import zinqq.app.theme.ZinqqTheme

/**
 * The PWA's Peers (U17, R10 UI; `Peers.tsx`): the connect input — parsed
 * client-side like the PWA, then handed to OpenChannel where the core's
 * `open_channel` connects if needed — the `(N connected, M saved)` list with
 * Refresh, per-peer status dots and channel-guarded Forget, and the nested
 * channel rows with Close / Force Close actions.
 */
@Composable
fun PeersScreen(
    port: SettingsPort,
    onBack: (() -> Unit)?,
    onOpenChannel: (String) -> Unit,
    onCloseChannel: (channelId: String, force: Boolean) -> Unit,
) {
    val colors = ZinqqTheme.colors
    val scope = rememberCoroutineScope()

    var peerAddress by remember { mutableStateOf("") }
    var connectError by remember { mutableStateOf<String?>(null) }
    var peers by remember { mutableStateOf<List<PeerView>?>(null) }
    var channels by remember { mutableStateOf<Map<String, List<ChannelView>>>(emptyMap()) }
    var loadError by remember { mutableStateOf<String?>(null) }
    var forgetError by remember { mutableStateOf<String?>(null) }

    fun refresh() {
        scope.launch {
            try {
                val fetchedPeers = port.listPeers()
                channels = channelsByPeer(port.listChannels())
                peers = fetchedPeers
                loadError = null
            } catch (e: Exception) {
                loadError = e.message ?: "Lightning node error"
            }
        }
    }

    LaunchedEffect(Unit) { refresh() }

    fun connect() {
        connectError = null
        when (val parsed = parsePeerAddress(peerAddress.trim())) {
            is PeerAddressParse.Invalid -> connectError = parsed.message
            is PeerAddressParse.Valid -> onOpenChannel(peerAddress.trim())
        }
    }

    SettingsScaffold(title = "Peers", onBack = onBack) {
        val currentPeers = peers
        if (currentPeers == null && loadError == null) {
            CenteredNote("Loading Lightning node...")
            return@SettingsScaffold
        }
        if (loadError != null && currentPeers == null) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 24.dp)
                    .padding(top = 64.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text(
                    text = "Lightning node error",
                    color = colors.onDark,
                    fontWeight = FontWeight.SemiBold,
                    fontSize = 16.sp,
                )
                Text(
                    text = loadError.orEmpty(),
                    color = colors.danger,
                    fontSize = 14.sp,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(top = 8.dp),
                )
            }
            return@SettingsScaffold
        }

        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .padding(top = 8.dp, bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            // Connect form (Peers.tsx:167-193).
            Column {
                Text(
                    text = "Connect & Open Channel",
                    color = colors.onDarkMuted,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Medium,
                )
                Box(
                    modifier = Modifier
                        .padding(top = 8.dp)
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(12.dp))
                        .background(colors.darkElevated)
                        .padding(horizontal = 16.dp, vertical = 14.dp),
                ) {
                    BasicTextField(
                        value = peerAddress,
                        onValueChange = { peerAddress = it },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(
                            capitalization = KeyboardCapitalization.None,
                            autoCorrectEnabled = false,
                        ),
                        textStyle = TextStyle(
                            color = colors.onDark,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 14.sp,
                        ),
                        cursorBrush = SolidColor(colors.hot),
                        modifier = Modifier.fillMaxWidth(),
                        decorationBox = { innerTextField ->
                            if (peerAddress.isEmpty()) {
                                Text(
                                    text = "pubkey@host:port",
                                    color = colors.onDarkMuted,
                                    fontFamily = FontFamily.Monospace,
                                    fontSize = 14.sp,
                                )
                            }
                            innerTextField()
                        },
                    )
                }
                connectError?.let {
                    Text(
                        text = it,
                        color = colors.danger,
                        fontSize = 14.sp,
                        modifier = Modifier.padding(top = 8.dp),
                    )
                }
                SettingsCta(
                    label = "Next",
                    background = colors.cta,
                    contentColor = colors.onCta,
                    enabled = peerAddress.isNotBlank(),
                    disabledAlpha = 0.3f,
                    onClick = ::connect,
                    modifier = Modifier.padding(top = 8.dp),
                )
            }

            // Peer list (Peers.tsx:196-310).
            Column {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = peersCountLabel(currentPeers.orEmpty()),
                        color = colors.onDarkMuted,
                        fontSize = 14.sp,
                        fontWeight = FontWeight.Medium,
                        modifier = Modifier.weight(1f),
                    )
                    Text(
                        text = "Refresh",
                        color = colors.onDark,
                        fontSize = 12.sp,
                        textDecoration = TextDecoration.Underline,
                        modifier = Modifier
                            .clip(RoundedCornerShape(6.dp))
                            .clickable { refresh() }
                            .padding(8.dp),
                    )
                }
                forgetError?.let {
                    Text(
                        text = it,
                        color = colors.danger,
                        fontSize = 14.sp,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                }
                if (currentPeers.isNullOrEmpty()) {
                    Text(
                        text = "No peers connected",
                        color = colors.onDarkMuted,
                        fontSize = 14.sp,
                        textAlign = TextAlign.Center,
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(vertical = 16.dp),
                    )
                } else {
                    Column(
                        modifier = Modifier.padding(top = 8.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        currentPeers.forEach { peer ->
                            PeerCard(
                                peer = peer,
                                channels = channels[peer.pubkey].orEmpty(),
                                onForget = {
                                    forgetError = null
                                    scope.launch {
                                        try {
                                            port.forgetPeer(peer.pubkey)
                                            refresh()
                                        } catch (e: Exception) {
                                            forgetError = forgetErrorMessage(e)
                                        }
                                    }
                                },
                                onCloseChannel = onCloseChannel,
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun PeerCard(
    peer: PeerView,
    channels: List<ChannelView>,
    onForget: () -> Unit,
    onCloseChannel: (channelId: String, force: Boolean) -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(colors.darkElevated)
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(
                modifier = Modifier
                    .size(10.dp)
                    .clip(CircleShape)
                    .background(if (peer.connected) colors.success else colors.onDarkMuted),
            )
            Text(
                text = peerDisplayId(peer.pubkey),
                color = colors.onDark,
                fontFamily = FontFamily.Monospace,
                fontSize = 13.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier
                    .weight(1f)
                    .padding(horizontal = 12.dp),
            )
            Text(
                text = peerStatusLabel(peer.connected),
                color = if (peer.connected) colors.success else colors.onDarkMuted,
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
            )
            if (showsForget(peer)) {
                val enabled = forgetEnabled(peer)
                Text(
                    text = "Forget",
                    color = colors.danger,
                    fontSize = 12.sp,
                    modifier = Modifier
                        .padding(start = 12.dp)
                        .alpha(if (enabled) 1f else 0.3f)
                        .clip(RoundedCornerShape(6.dp))
                        .clickable(enabled = enabled, onClick = onForget)
                        .padding(4.dp),
                )
            }
        }
        channels.forEach { channel ->
            ChannelRow(channel = channel, onCloseChannel = onCloseChannel)
        }
    }
}

@Composable
private fun ChannelRow(
    channel: ChannelView,
    onCloseChannel: (channelId: String, force: Boolean) -> Unit,
) {
    val colors = ZinqqTheme.colors
    val closing = channel.state == ChannelStateLabel.CLOSING
    // `ml-5 border-l pl-3` (Peers.tsx:253): indented with a left border rule.
    Column(
        modifier = Modifier
            .padding(start = 20.dp)
            .fillMaxWidth()
            .leftBorder(colors.darkBorder)
            .padding(start = 12.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = channelStateText(channel.state),
                color = if (closing) colors.warning else colors.onDarkMuted,
                fontSize = 12.sp,
                fontWeight = if (closing) FontWeight.SemiBold else FontWeight.Normal,
                modifier = Modifier.weight(1f),
            )
            Text(
                text = channelCapacityText(channel),
                color = colors.onDark,
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
            )
        }
        if (closing) {
            Text(
                text = CLOSING_IN_PROGRESS_NOTE,
                color = colors.onDarkMuted,
                fontSize = 12.sp,
            )
        }
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Row(
                modifier = Modifier.weight(1f),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Text(
                    text = channelSendText(channel),
                    color = colors.onDarkMuted,
                    fontSize = 12.sp,
                )
                Text(
                    text = channelReceiveText(channel),
                    color = colors.onDarkMuted,
                    fontSize = 12.sp,
                )
                channelReserveText(channel)?.let {
                    Text(text = it, color = colors.onDarkMuted, fontSize = 12.sp)
                }
            }
            Text(
                text = channelCloseActionLabel(channel.state),
                color = colors.danger,
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
                modifier = Modifier
                    .clip(RoundedCornerShape(6.dp))
                    .clickable { onCloseChannel(channel.channelId, closing) }
                    .padding(4.dp),
            )
        }
    }
}

/** A 1dp left border rule, like the PWA's `border-l border-dark-border`. */
private fun Modifier.leftBorder(color: Color): Modifier = drawBehind {
    drawLine(
        color = color,
        start = androidx.compose.ui.geometry.Offset(0f, 0f),
        end = androidx.compose.ui.geometry.Offset(0f, size.height),
        strokeWidth = 1.dp.toPx(),
    )
}
