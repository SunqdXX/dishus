package org.dishus.app.ui

import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.dishus.app.DishusApp
import uniffi.dishus_core.PcInfo
import uniffi.dishus_core.RelaySettings

fun expiryText(expiresAt: Long): String {
    val left = expiresAt - System.currentTimeMillis() / 1000
    return when {
        left <= 0 -> "expired"
        left >= 86_400 -> "expires in ${(left + 43_200) / 86_400} d"
        else -> "expires in ${left / 3600} h"
    }
}

@Composable
fun HomeScreen(app: DishusApp, onConnect: (String) -> Unit) {
    var pcs by remember { mutableStateOf(runCatching { app.core.pairedPcs() }.getOrDefault(emptyList())) }
    var pairing by remember { mutableStateOf(false) }
    var pairError by remember { mutableStateOf<String?>(null) }
    var pasteOpen by remember { mutableStateOf(false) }
    var settingsOpen by remember { mutableStateOf(false) }
    var forget by remember { mutableStateOf<PcInfo?>(null) }
    // Confirmed but not yet carried out: hidden from the list, undoable for a few seconds.
    var pendingForget by remember { mutableStateOf<PcInfo?>(null) }
    val scope = rememberCoroutineScope()

    fun commitForget() {
        val pc = pendingForget ?: return
        pendingForget = null
        runCatching { app.core.forgetPc(pc.id); app.persistVault() }
        pcs = runCatching { app.core.pairedPcs() }.getOrDefault(emptyList())
    }
    LaunchedEffect(pendingForget) {
        if (pendingForget != null) { delay(6_000); commitForget() }
    }
    // Leaving the screen (connect, lock) carries out a pending forget rather than dropping it.
    DisposableEffect(Unit) { onDispose { commitForget() } }

    fun pair(link: String) {
        pairing = true; pairError = null
        scope.launch {
            try {
                val pc = app.core.pair(link.trim(), (Build.MODEL ?: "Android").take(64))
                app.persistVault()
                pcs = app.core.pairedPcs()
                app.prefs.lastPcId = pc.id
            } catch (e: Exception) {
                pairError = e.message ?: "Pairing failed"
            } finally {
                pairing = false
            }
        }
    }

    val scanner = rememberLauncherForActivityResult(ScanContract()) { r -> r.contents?.let { pair(it) } }

    Column(Modifier.fillMaxSize().safeDrawingPadding().padding(20.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("Your PCs", style = MaterialTheme.typography.headlineSmall, modifier = Modifier.weight(1f))
            TextButton(onClick = { settingsOpen = true }) { Text("Settings") }
            TextButton(onClick = { app.lockNow() }) { Text("Lock") }
        }
        Spacer(Modifier.height(12.dp))
        val shown = pcs.filter { it.id != pendingForget?.id }
        if (shown.isEmpty()) {
            Text(
                "No PC paired yet. On the PC run `dishus pair`, then scan the QR code it shows.",
                style = MaterialTheme.typography.bodyMedium,
            )
        }
        LazyColumn(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(10.dp)) {
            items(shown, key = { it.id }) { pc ->
                Card(Modifier.fillMaxWidth().clickable { onConnect(pc.id) }) {
                    Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text(pc.name, style = MaterialTheme.typography.titleMedium)
                            Text(
                                "${pc.id.take(12)} · ${expiryText(pc.expiresAt.toLong())}",
                                style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace,
                            )
                        }
                        Button(onClick = { onConnect(pc.id) }) { Text("Connect") }
                        // Forget lives in a menu: it used to sit right next to Connect.
                        var menu by remember { mutableStateOf(false) }
                        TextButton(onClick = { menu = true }) { Text("⋮") }
                        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                            DropdownMenuItem(text = { Text("Forget this PC…") }, onClick = { menu = false; forget = pc })
                        }
                    }
                }
            }
        }
        pendingForget?.let { pc ->
            Card(Modifier.fillMaxWidth()) {
                Row(Modifier.padding(start = 16.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                    Text("Forgot ${pc.name}.", modifier = Modifier.weight(1f))
                    TextButton(onClick = { pendingForget = null }) { Text("Undo") }
                }
            }
            Spacer(Modifier.height(8.dp))
        }
        pairError?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        Spacer(Modifier.height(8.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Button(enabled = !pairing, modifier = Modifier.weight(1f), onClick = {
                scanner.launch(
                    ScanOptions().setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                        .setPrompt("Scan the QR code from `dishus pair`").setBeepEnabled(false).setOrientationLocked(false),
                )
            }) { Text(if (pairing) "Pairing…" else "Scan QR") }
            OutlinedButton(enabled = !pairing, modifier = Modifier.weight(1f), onClick = { pasteOpen = true }) {
                Text("Paste link")
            }
        }
    }

    if (pasteOpen) {
        var link by remember { mutableStateOf("") }
        AlertDialog(
            onDismissRequest = { pasteOpen = false },
            title = { Text("Pairing link") },
            text = { OutlinedTextField(link, { link = it }, placeholder = { Text("dishus://pair?…") }) },
            confirmButton = { TextButton(onClick = { pasteOpen = false; pair(link) }) { Text("Pair") } },
            dismissButton = { TextButton(onClick = { pasteOpen = false }) { Text("Cancel") } },
        )
    }
    forget?.let { pc ->
        AlertDialog(
            onDismissRequest = { forget = null },
            title = { Text("Forget ${pc.name}?") },
            text = { Text("This phone forgets the PC. To also revoke it on the PC run `dishus revoke`.") },
            confirmButton = {
                TextButton(onClick = {
                    commitForget() // an earlier pending one, if any
                    pendingForget = pc
                    forget = null
                }) { Text("Forget") }
            },
            dismissButton = { TextButton(onClick = { forget = null }) { Text("Cancel") } },
        )
    }
    if (settingsOpen) AppSettingsDialog(app) { settingsOpen = false }
}

@Composable
fun AppSettingsDialog(app: DishusApp, onClose: () -> Unit) {
    var relays by remember { mutableStateOf(app.prefs.relays.joinToString("\n")) }
    var noRelay by remember { mutableStateOf(app.prefs.disableRelays) }
    var lockMin by remember { mutableIntStateOf(app.prefs.autoLockMinutes) }
    var autoConnect by remember { mutableStateOf(app.prefs.autoConnect) }
    var haptics by remember { mutableStateOf(app.prefs.haptics) }
    var shots by remember { mutableStateOf(app.prefs.allowScreenshots) }
    var speed by remember { mutableFloatStateOf(app.prefs.pointerSpeed) }
    var error by remember { mutableStateOf<String?>(null) }
    val phoneId = remember { runCatching { app.core.phoneId() }.getOrDefault("?") }

    AlertDialog(
        onDismissRequest = onClose,
        title = { Text("Settings") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                Text("Relay servers (one per line, empty = iroh public relays)", style = MaterialTheme.typography.labelMedium)
                OutlinedTextField(relays, { relays = it }, minLines = 2, modifier = Modifier.fillMaxWidth())
                LabeledSwitch("Direct connections only (no relay)", noRelay) { noRelay = it }
                HorizontalDivider(Modifier.padding(vertical = 8.dp))
                Text("Auto-lock after ${if (lockMin == 0) "leaving the app" else "$lockMin min in background"}")
                Slider(lockMin.toFloat(), { lockMin = it.toInt() }, valueRange = 0f..30f, steps = 29)
                LabeledSwitch("Connect to last PC on unlock", autoConnect) { autoConnect = it }
                LabeledSwitch("Keyboard haptics", haptics) { haptics = it }
                LabeledSwitch("Allow screenshots (the PC screen also shows in recents)", shots) { shots = it }
                Text("Pointer speed ${"%.1f".format(speed)}×")
                Slider(speed, { speed = it }, valueRange = 0.3f..5f)
                HorizontalDivider(Modifier.padding(vertical = 8.dp))
                Text("This phone's id (matches `dishus list` on the PC):", style = MaterialTheme.typography.labelMedium)
                Text(phoneId, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                val list = relays.split('\n').map { it.trim() }.filter { it.isNotEmpty() }
                try {
                    app.core.setRelays(RelaySettings(list, noRelay))
                    app.prefs.relays = list
                    app.prefs.disableRelays = noRelay
                    app.prefs.autoLockMinutes = lockMin
                    app.prefs.autoConnect = autoConnect
                    app.prefs.haptics = haptics
                    app.prefs.allowScreenshots = shots
                    app.screenshotsAllowed.value = shots
                    app.prefs.pointerSpeed = speed
                    onClose()
                } catch (e: Exception) {
                    error = e.message
                }
            }) { Text("Save") }
        },
        dismissButton = { TextButton(onClick = onClose) { Text("Cancel") } },
    )
}

@Composable
fun LabeledSwitch(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.fillMaxWidth()) {
        Text(label, Modifier.weight(1f))
        Spacer(Modifier.width(8.dp))
        Switch(checked, onChange)
    }
}
