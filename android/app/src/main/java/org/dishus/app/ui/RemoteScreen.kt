package org.dishus.app.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import org.dishus.app.DishusApp
import org.dishus.app.RemoteController
import uniffi.dishus_core.StreamQuality
import uniffi.dishus_core.StreamSettings

@Composable
fun RemoteScreen(app: DishusApp, pcId: String, onExit: () -> Unit) {
    val controller = remember(pcId) { RemoteController(app, pcId) }
    val ui by controller.ui.collectAsState()
    val bg by app.backgrounded.collectAsState()
    var sheet by remember { mutableStateOf(false) }

    // Never keep controlling the PC from a pocket: drop the session when backgrounded, release
    // everything, and reconnect (fast) when the app comes back.
    LaunchedEffect(bg) { if (bg) controller.stop() else controller.start() }
    DisposableEffect(controller) { onDispose { controller.shutdown() } }
    BackHandler { controller.releaseAll(); onExit() }

    Column(Modifier.fillMaxSize().background(Color.Black).safeDrawingPadding()) {
        // ---- 60 %: PC screen ----
        Box(Modifier.fillMaxWidth().weight(0.6f)) {
            ScreenView(controller, ui.width, ui.height, app.prefs.pointerSpeed, Modifier.fillMaxSize())
            Row(
                Modifier.align(Alignment.TopEnd).padding(6.dp)
                    .background(Color(0xAA000000), RoundedCornerShape(12.dp))
                    .padding(horizontal = 8.dp, vertical = 2.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                val dot = if (ui.connected) Color(0xFF7FD1AE) else Color(0xFFFFB74D)
                Text("●", color = dot)
                Text(
                    if (ui.connected) " ${ui.path} · app ${ui.rttMs} ms" else " ${ui.status.take(48)}",
                    color = Color.White, style = MaterialTheme.typography.labelSmall,
                )
                TextButton(onClick = { sheet = true }) { Text("⚙", color = Color.White) }
            }
            if (!ui.connected) {
                Text(
                    ui.status, color = Color.White, modifier = Modifier.align(Alignment.Center).padding(24.dp),
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
        }
        // ---- 40 %: keyboard ----
        PcKeyboard(controller, ui.mods, app.prefs.haptics, Modifier.fillMaxWidth().weight(0.4f))
    }

    ui.notice?.let { n ->
        AlertDialog(
            onDismissRequest = { controller.clearNotice() },
            text = { Text(n) },
            confirmButton = { TextButton(onClick = { controller.clearNotice() }) { Text("OK") } },
        )
    }
    if (ui.fatal) {
        AlertDialog(
            onDismissRequest = onExit,
            title = { Text("Session ended") },
            text = { Text(ui.status) },
            confirmButton = { TextButton(onClick = onExit) { Text("Back") } },
        )
    }
    if (sheet) QuickSettings(app, controller, onDismiss = { sheet = false }, onDisconnect = { sheet = false; onExit() })
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun QuickSettings(app: DishusApp, c: RemoteController, onDismiss: () -> Unit, onDisconnect: () -> Unit) {
    val ui by c.ui.collectAsState()
    var s by remember { mutableStateOf(app.prefs.stream) }
    var appSettings by remember { mutableStateOf(false) }
    fun apply(n: StreamSettings) { s = n; c.applyStream(n) }

    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(Modifier.padding(horizontal = 20.dp).padding(bottom = 24.dp).verticalScroll(rememberScrollState())) {
            Text("Connection", style = MaterialTheme.typography.titleMedium)
            Text(
                if (ui.connected) "${ui.path}\napp round-trip ${ui.rttMs} ms\n${ui.encoder} ${ui.width}×${ui.height} · ${ui.fps} fps · ${ui.bitrateKbps} kbit/s (start values; adaptive)"
                else ui.status,
                fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall,
            )
            Text("Pairing ${expiryText(ui.expiresAt)}", style = MaterialTheme.typography.bodySmall)
            HorizontalDivider(Modifier.padding(vertical = 10.dp))

            Text("Frame rate", style = MaterialTheme.typography.labelLarge)
            ChipRow(listOf("Adaptive" to 0, "30" to 30, "60" to 60), s.fps.toInt()) { apply(s.copy(fps = it.toUByte())) }
            Text("Quality", style = MaterialTheme.typography.labelLarge)
            ChipRow(
                listOf("Auto" to StreamQuality.AUTO, "Low" to StreamQuality.LOW, "Medium" to StreamQuality.MEDIUM, "High" to StreamQuality.HIGH),
                s.quality,
            ) { apply(s.copy(quality = it)) }
            Text("Resolution", style = MaterialTheme.typography.labelLarge)
            ChipRow(listOf("Native" to 0, "1080p" to 1080, "720p" to 720), s.maxHeight.toInt()) { apply(s.copy(maxHeight = it.toUShort())) }
            if (ui.monitors.size > 1) {
                Text("Monitor", style = MaterialTheme.typography.labelLarge)
                ChipRow(ui.monitors.map { it.name to it.index.toInt() }, s.monitor.toInt()) { apply(s.copy(monitor = it.toUByte())) }
            }
            HorizontalDivider(Modifier.padding(vertical = 10.dp))
            Text("Pairing", style = MaterialTheme.typography.labelLarge)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = { c.renew(30) }, enabled = ui.connected) { Text("Renew 30 days") }
                OutlinedButton(onClick = { c.renew(60) }, enabled = ui.connected) { Text("Renew 60 days") }
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = { c.releaseAll() }) { Text("Release all keys") }
                OutlinedButton(onClick = { appSettings = true }) { Text("Relays & app…") }
            }
            Spacer(Modifier.height(8.dp))
            Button(onClick = { c.releaseAll(); onDisconnect() }, modifier = Modifier.fillMaxWidth()) { Text("Disconnect") }
        }
    }
    if (appSettings) AppSettingsDialog(app) { appSettings = false }
}

@Composable
private fun <T> ChipRow(options: List<Pair<String, T>>, selected: T, onPick: (T) -> Unit) {
    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        for ((label, v) in options) {
            FilterChip(selected = v == selected, onClick = { onPick(v) }, label = { Text(label) })
        }
    }
}
