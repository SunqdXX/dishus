package org.dishus.app.ui

import android.view.HapticFeedbackConstants
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.waitForUpOrCancellation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.platform.LocalViewConfiguration
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import org.dishus.app.ModState
import org.dishus.app.RemoteController
import org.dishus.app.input.Evdev
import org.dishus.app.input.Key
import org.dishus.app.input.Layout

/**
 * Full PC keyboard. Normal keys send press on touch-down and release on touch-up (so holding a
 * key auto-repeats on the PC). Modifiers: tap = sticky for the next key, long-press = locked
 * until tapped again.
 */
@Composable
fun PcKeyboard(controller: RemoteController, mods: Map<Int, ModState>, haptics: Boolean, modifier: Modifier = Modifier) {
    val shiftOn = (mods[Evdev.LEFTSHIFT] ?: mods[Evdev.RIGHTSHIFT]) != null
    Column(
        modifier.background(Color(0xFF0B0E11)).padding(2.dp),
        verticalArrangement = Arrangement.spacedBy(3.dp),
    ) {
        for (row in Layout.rows) {
            Row(Modifier.fillMaxWidth().weight(1f), horizontalArrangement = Arrangement.spacedBy(3.dp)) {
                for (k in row) {
                    KeyCap(k, mods[k.code], shiftOn, haptics, controller, Modifier.weight(k.weight).fillMaxHeight())
                }
            }
        }
    }
}

@Composable
private fun KeyCap(k: Key, mod: ModState?, shiftOn: Boolean, haptics: Boolean, c: RemoteController, modifier: Modifier) {
    var pressed by remember { mutableStateOf(false) }
    val view = LocalView.current
    val longPressMs = LocalViewConfiguration.current.longPressTimeoutMillis
    val isMod = k.code in Evdev.MODIFIERS
    val bg = when {
        mod == ModState.LOCKED -> KeyLocked
        mod == ModState.STICKY -> KeySticky
        pressed -> KeyBgPressed
        else -> KeyBg
    }
    Box(
        modifier
            .clip(RoundedCornerShape(5.dp))
            .background(bg)
            .pointerInput(k.code, haptics) {
                awaitEachGesture {
                    val down = awaitFirstDown()
                    down.consume()
                    pressed = true
                    if (haptics) view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                    if (!isMod) c.key(k.code, true)
                    val up = waitForUpOrCancellation()
                    pressed = false
                    if (isMod) {
                        val held = (up?.uptimeMillis ?: down.uptimeMillis) - down.uptimeMillis
                        if (up != null && held >= longPressMs) c.modifierLock(k.code) else if (up != null) c.modifierTap(k.code)
                    } else {
                        c.key(k.code, false)
                    }
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        val label = if (shiftOn && k.shiftLabel != null) k.shiftLabel else k.label
        KeyLabel(label, mod == ModState.LOCKED, isMod)
    }
}

/**
 * A key is a fixed-size box, so its label must fit it: honour the system font scale when there
 * is room, shrink when there isn't (at font scale 1.3 on a narrow cover screen, "F10"/"F11"/"F12"
 * were all clipped to "F1").
 */
@Composable
private fun KeyLabel(label: String, locked: Boolean, isMod: Boolean) {
    BoxWithConstraints(contentAlignment = Alignment.Center) {
        val density = LocalDensity.current
        val wanted = with(density) { (if (label.length > 3) 11.sp else 15.sp).toPx() }
        // ~0.62 em per glyph is a safe average for the default sans font, caps included.
        val fitW = with(density) { (maxWidth - 4.dp).toPx() } / (label.length.coerceAtLeast(1) * 0.62f)
        val fitH = with(density) { maxHeight.toPx() } * 0.6f
        val px = minOf(wanted, fitW, fitH).coerceAtLeast(1f)
        Text(
            label,
            color = if (locked) Color(0xFF07130D) else Color(0xFFE6EBEF),
            fontSize = with(density) { px.toSp() },
            fontWeight = if (isMod) FontWeight.SemiBold else FontWeight.Normal,
            maxLines = 1,
            softWrap = false,
        )
    }
}
