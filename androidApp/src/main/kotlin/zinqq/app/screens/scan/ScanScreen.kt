package zinqq.app.screens.scan

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.ImageAnalysis
import androidx.camera.mlkit.vision.MlKitAnalyzer
import androidx.camera.view.LifecycleCameraController
import androidx.camera.view.PreviewView
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import zinqq.app.nav.ScreenHeader
import zinqq.app.screens.send.SendPort
import zinqq.app.theme.ZinqqTheme

/**
 * The Scan screen (U15, R13): CameraX preview + MLKit QR-only analyzer over
 * the PWA's layout (`Scan.tsx`) — 256dp viewfinder frame, caption, 3 s
 * invalid toast, camera-error taxonomy — plus the plan's committed camera
 * permission contract. A valid decode navigates to Send with the RAW string;
 * validity itself is the core classifier's verdict via [SendPort] (R14).
 */
@Composable
fun ScanScreen(
    port: SendPort,
    onScanned: (String) -> Unit,
    onClose: () -> Unit,
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()

    var permission by remember {
        mutableStateOf(
            if (ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED
            ) {
                CameraPermissionUi.GRANTED
            } else {
                CameraPermissionUi.REQUESTING
            },
        )
    }
    var cameraError by remember { mutableStateOf<ScanCameraError?>(null) }
    var toastVisible by remember { mutableStateOf(false) }
    var navigated by remember { mutableStateOf(false) }

    val requestPermission = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        val rationale = (context as? Activity)
            ?.shouldShowRequestPermissionRationale(Manifest.permission.CAMERA) == true
        permission = reduceCameraPermission(granted, rationale)
    }

    // Committed contract: the initial request fires on entry.
    LaunchedEffect(Unit) {
        if (permission == CameraPermissionUi.REQUESTING) {
            requestPermission.launch(Manifest.permission.CAMERA)
        }
    }

    // Returning from OS Settings must pick up a grant without re-entering.
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME &&
                ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED
            ) {
                permission = CameraPermissionUi.GRANTED
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

    // The taxonomy's not-found arm is knowable up front.
    LaunchedEffect(Unit) {
        if (!context.packageManager.hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)) {
            cameraError = ScanCameraError.NOT_FOUND
        }
    }

    // Invalid toast auto-clears after 3 s (PWA Scan.tsx:79-84).
    LaunchedEffect(toastVisible) {
        if (toastVisible) {
            delay(INVALID_SCAN_TOAST_MS)
            toastVisible = false
        }
    }

    val colors = ZinqqTheme.colors
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black),
    ) {
        if (permission == CameraPermissionUi.GRANTED && cameraError == null) {
            AndroidView(
                modifier = Modifier.fillMaxSize(),
                factory = { viewContext ->
                    val previewView = PreviewView(viewContext)
                    val executor = ContextCompat.getMainExecutor(viewContext)
                    val scanner = BarcodeScanning.getClient(
                        BarcodeScannerOptions.Builder()
                            .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
                            .build(),
                    )
                    val controller = LifecycleCameraController(viewContext)
                    controller.setImageAnalysisAnalyzer(
                        executor,
                        MlKitAnalyzer(
                            listOf(scanner),
                            ImageAnalysis.COORDINATE_SYSTEM_ORIGINAL,
                            executor,
                        ) { result ->
                            val raw = result?.getValue(scanner)?.firstOrNull()?.rawValue
                            if (raw == null || navigated) return@MlKitAnalyzer
                            scope.launch {
                                // The core is the only classifier (R14).
                                val kind = port.classify(raw).kind
                                val outcome = routeDecode(
                                    raw = raw,
                                    classify = { kind },
                                    alreadyNavigated = navigated,
                                    toastVisible = toastVisible,
                                )
                                when (outcome) {
                                    is DecodeOutcome.Navigate -> {
                                        if (!navigated) {
                                            navigated = true
                                            onScanned(outcome.raw)
                                        }
                                    }
                                    DecodeOutcome.InvalidToast -> toastVisible = true
                                    DecodeOutcome.None -> Unit
                                }
                            }
                        },
                    )
                    // Camera-state errors feed the taxonomy (in-use/unknown).
                    controller.initializationFuture.addListener({
                        controller.cameraInfo?.cameraState?.observe(lifecycleOwner) { state ->
                            state.error?.code?.let { code ->
                                cameraStateError(code)?.let { cameraError = it }
                            }
                        }
                    }, executor)
                    controller.bindToLifecycle(lifecycleOwner)
                    previewView.controller = controller
                    previewView
                },
            )
        }

        Column(modifier = Modifier.fillMaxSize()) {
            ScreenHeader(title = "Scan", onClose = onClose, tint = Color.White)
            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth(),
                contentAlignment = Alignment.Center,
            ) {
                when {
                    permission == CameraPermissionUi.DENIED_RETRY ->
                        PermissionBanner(
                            message = CAMERA_RATIONALE_MESSAGE,
                            actionLabel = "Retry",
                            onAction = {
                                requestPermission.launch(Manifest.permission.CAMERA)
                            },
                        )

                    permission == CameraPermissionUi.DENIED_OPEN_SETTINGS ->
                        PermissionBanner(
                            message = CAMERA_SETTINGS_MESSAGE,
                            actionLabel = "Open Settings",
                            onAction = {
                                context.startActivity(
                                    Intent(
                                        Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                                        Uri.fromParts("package", context.packageName, null),
                                    ),
                                )
                            },
                        )

                    cameraError != null ->
                        Text(
                            text = cameraErrorMessage(cameraError ?: ScanCameraError.UNKNOWN),
                            color = colors.onDarkMuted,
                            fontSize = 14.sp,
                            textAlign = TextAlign.Center,
                            modifier = Modifier.padding(horizontal = 32.dp),
                        )

                    permission == CameraPermissionUi.GRANTED -> {
                        // Viewfinder frame + caption (PWA Scan.tsx:104-111).
                        Column(horizontalAlignment = Alignment.CenterHorizontally) {
                            Box(
                                modifier = Modifier
                                    .size(256.dp)
                                    .border(
                                        width = 2.dp,
                                        color = Color.White.copy(alpha = 0.6f),
                                        shape = RoundedCornerShape(16.dp),
                                    )
                                    .semantics { contentDescription = "QR viewfinder" },
                            )
                            Text(
                                text = SCAN_CAPTION,
                                color = Color.White.copy(alpha = 0.7f),
                                fontSize = 14.sp,
                                modifier = Modifier.padding(top = 24.dp),
                            )
                        }
                    }
                }

                // Transient invalid toast (PWA Scan.tsx:120-125).
                if (toastVisible) {
                    Text(
                        text = INVALID_SCAN_MESSAGE,
                        color = Color.White,
                        fontSize = 14.sp,
                        fontWeight = FontWeight.Medium,
                        textAlign = TextAlign.Center,
                        modifier = Modifier
                            .align(Alignment.BottomCenter)
                            .padding(horizontal = 16.dp)
                            .padding(bottom = 32.dp)
                            .fillMaxWidth()
                            .clip(RoundedCornerShape(12.dp))
                            .background(colors.dangerStrong.copy(alpha = 0.9f))
                            .padding(horizontal = 16.dp, vertical = 12.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun PermissionBanner(
    message: String,
    actionLabel: String,
    onAction: () -> Unit,
) {
    val colors = ZinqqTheme.colors
    Column(
        modifier = Modifier
            .padding(horizontal = 24.dp)
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(Color.White.copy(alpha = 0.1f))
            .padding(20.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(
            text = message,
            color = Color.White,
            fontSize = 14.sp,
            textAlign = TextAlign.Center,
        )
        Text(
            text = actionLabel,
            color = colors.onCta,
            fontFamily = ZinqqTheme.fonts.display,
            fontWeight = FontWeight.Bold,
            fontSize = 15.sp,
            modifier = Modifier
                .padding(top = 16.dp)
                .clip(RoundedCornerShape(10.dp))
                .background(colors.cta)
                .clickable(onClick = onAction)
                .padding(horizontal = 20.dp, vertical = 10.dp)
                .semantics { contentDescription = actionLabel },
        )
    }
}
