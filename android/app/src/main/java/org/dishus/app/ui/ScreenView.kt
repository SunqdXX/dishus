package org.dishus.app.ui

import android.graphics.SurfaceTexture
import android.os.SystemClock
import android.view.Surface
import android.view.TextureView
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.platform.LocalViewConfiguration
import androidx.compose.ui.viewinterop.AndroidView
import org.dishus.app.RemoteController
import kotlin.math.abs
import kotlin.math.hypot

/**
 * The PC screen, used like a laptop touchpad (relative pointer):
 *  - one finger moves the pointer, tap = left click
 *  - long-press = right click, two-finger tap = right click
 *  - double-tap and hold, then move = drag (left button held)
 *  - two-finger move = scroll
 *  - pinch = zoom the local view (pinch back to 1× to reset)
 */
@Composable
fun ScreenView(controller: RemoteController, videoW: Int, videoH: Int, pointerSpeed: Float, modifier: Modifier = Modifier) {
    var zoom by remember { mutableFloatStateOf(1f) }
    var pan by remember { mutableStateOf(Offset.Zero) }
    val vc = LocalViewConfiguration.current
    val aspect = if (videoW > 0 && videoH > 0) videoW.toFloat() / videoH else 16f / 9f

    Box(
        modifier
            .background(Color.Black)
            .pointerInput(controller, pointerSpeed) {
                var lastTapUpTime = 0L
                var lastTapPos = Offset.Zero
                var fracX = 0f
                var fracY = 0f
                var scrollAccX = 0f
                var scrollAccY = 0f
                awaitEachGesture {
                    val down = awaitFirstDown(requireUnconsumed = false)
                    val start = down.position
                    val startTime = down.uptimeMillis
                    val dragMode = startTime - lastTapUpTime < 300 && (start - lastTapPos).getDistance() < vc.touchSlop * 3
                    if (dragMode) controller.button(0, true)
                    var moved = false
                    var maxPointers = 1
                    var longFired = false
                    var pinching = false
                    var prevSpan = 0f
                    var prevCentroid = Offset.Zero
                    while (true) {
                        // down.uptimeMillis uses the SystemClock.uptimeMillis() time base
                        val remaining = vc.longPressTimeoutMillis - (SystemClock.uptimeMillis() - startTime)
                        val event = if (!moved && !longFired && maxPointers == 1 && !dragMode) {
                            if (remaining > 0) withTimeoutOrNull(remaining) { awaitPointerEvent(PointerEventPass.Main) } else null
                        } else {
                            awaitPointerEvent(PointerEventPass.Main)
                        }
                        if (event == null) {
                            controller.click(1) // long-press → right click
                            longFired = true
                            continue
                        }
                        val pressed = event.changes.filter { it.pressed }
                        if (pressed.isEmpty()) break
                        if (pressed.size > maxPointers) {
                            maxPointers = pressed.size
                            if (pressed.size >= 2) {
                                prevSpan = span(pressed.map { it.position })
                                prevCentroid = centroid(pressed.map { it.position })
                            }
                        }
                        if (pressed.size == 1 && maxPointers == 1) {
                            val c = pressed[0]
                            val d = c.positionChange()
                            if (!moved && (c.position - start).getDistance() > vc.touchSlop) moved = true
                            if (moved) {
                                fracX += d.x * pointerSpeed / zoom
                                fracY += d.y * pointerSpeed / zoom
                                val ix = fracX.toInt(); val iy = fracY.toInt()
                                fracX -= ix; fracY -= iy
                                controller.mouseMove(ix, iy)
                            }
                        } else if (pressed.size >= 2) {
                            val pts = pressed.map { it.position }
                            val s = span(pts)
                            val ctr = centroid(pts)
                            if (!pinching && abs(s - prevSpan) > vc.touchSlop * 2) pinching = true
                            if (pinching) {
                                val factor = if (prevSpan > 0f) s / prevSpan else 1f
                                val newZoom = (zoom * factor).coerceIn(1f, 5f)
                                pan = if (newZoom <= 1.001f) Offset.Zero else pan + (ctr - prevCentroid)
                                zoom = newZoom
                            } else {
                                val d = ctr - prevCentroid
                                if (d.getDistance() > 0f) moved = true
                                // 1 px of finger travel ≈ 4 hi-res units (≈ 30 px per notch)
                                scrollAccX += d.x * -4f
                                scrollAccY += d.y * -4f
                                val sx = scrollAccX.toInt(); val sy = scrollAccY.toInt()
                                scrollAccX -= sx; scrollAccY -= sy
                                controller.scroll(sx, sy)
                            }
                            prevSpan = s
                            prevCentroid = ctr
                        }
                        event.changes.forEach { it.consume() }
                    }
                    when {
                        dragMode -> controller.button(0, false)
                        longFired || pinching -> {}
                        maxPointers == 2 && !moved -> controller.click(1)
                        maxPointers == 1 && !moved -> {
                            controller.click(0)
                            lastTapUpTime = SystemClock.uptimeMillis()
                            lastTapPos = start
                        }
                    }
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        AndroidView(
            factory = { ctx ->
                TextureView(ctx).apply {
                    surfaceTextureListener = object : TextureView.SurfaceTextureListener {
                        var surface: Surface? = null
                        override fun onSurfaceTextureAvailable(st: SurfaceTexture, w: Int, h: Int) {
                            surface = Surface(st).also { controller.attachSurface(it) }
                        }
                        override fun onSurfaceTextureSizeChanged(st: SurfaceTexture, w: Int, h: Int) {}
                        override fun onSurfaceTextureDestroyed(st: SurfaceTexture): Boolean {
                            controller.attachSurface(null)
                            surface?.release(); surface = null
                            return true
                        }
                        override fun onSurfaceTextureUpdated(st: SurfaceTexture) {}
                    }
                }
            },
            modifier = Modifier
                .aspectRatio(aspect)
                .graphicsLayer {
                    scaleX = zoom; scaleY = zoom
                    translationX = pan.x; translationY = pan.y
                },
        )
    }
}

private fun span(p: List<Offset>): Float {
    val c = centroid(p)
    return p.map { hypot(it.x - c.x, it.y - c.y) }.average().toFloat()
}

private fun centroid(p: List<Offset>): Offset =
    Offset(p.map { it.x }.average().toFloat(), p.map { it.y }.average().toFloat())
