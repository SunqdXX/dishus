package org.dishus.app

import android.os.Build
import android.view.Surface
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.cancel
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.dishus.app.input.Evdev
import org.dishus.app.video.H264Decoder
import uniffi.dishus_core.ControlEvent
import uniffi.dishus_core.DishusException
import uniffi.dishus_core.Monitor
import uniffi.dishus_core.RemoteSession
import uniffi.dishus_core.StreamSettings
import java.util.concurrent.Executors
import kotlinx.coroutines.asCoroutineDispatcher

enum class ModState { OFF, STICKY, LOCKED }

private const val EXPIRED_MSG = "The pairing with this PC has expired. Run `dishus renew` on the PC, or pair again."
private const val LOCKDOWN_MSG = "The PC is locked down (dishus kill). Waiting for `dishus unlock` on the PC…"

data class RemoteUi(
    val status: String = "Connecting…",
    val connected: Boolean = false,
    val path: String = "",
    val rttMs: Long = 0,
    val encoder: String = "",
    val width: Int = 0,
    val height: Int = 0,
    val fps: Int = 0,
    val bitrateKbps: Int = 0,
    val monitors: List<Monitor> = emptyList(),
    val expiresAt: Long = 0,
    val notice: String? = null,
    /** Set when retrying makes no sense (revoked, killed, expired). */
    val fatal: Boolean = false,
    val mods: Map<Int, ModState> = emptyMap(),
)

/**
 * Owns one remote session: connect with backoff, pump video into the decoder, surface control
 * events as [ui], and translate keyboard/touch actions into protocol messages.
 */
class RemoteController(private val app: DishusApp, private val pcId: String) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val decodeDispatcher = Executors.newSingleThreadExecutor { r -> Thread(r, "dishus-decode") }.asCoroutineDispatcher()
    private val _ui = MutableStateFlow(RemoteUi())
    val ui: StateFlow<RemoteUi> = _ui

    @Volatile private var session: RemoteSession? = null
    private var decoder: H264Decoder? = null
    private var surface: Surface? = null
    private var runJob: Job? = null
    private val mods = java.util.concurrent.ConcurrentHashMap<Int, ModState>()

    fun start() {
        val old = runJob
        if (old?.isActive == true) return
        // A cancelled previous loop may still be closing its session; let it finish first.
        runJob = scope.launch { old?.join(); runLoop() }
    }

    fun stop() {
        // runLoop owns the session: cancelling it releases keys, closes and destroys the session
        // only after the video/stats children have stopped using it.
        runJob?.cancel()
        mods.clear()
        publishMods()
        _ui.value = _ui.value.copy(connected = false, status = "Disconnected")
    }

    fun shutdown() {
        val job = runJob
        stop()
        scope.launch {
            job?.join()
            withContext(decodeDispatcher) { decoder?.release(); decoder = null }
            decodeDispatcher.close()
            scope.cancel()
        }
    }

    fun attachSurface(s: Surface?) {
        scope.launch(decodeDispatcher) {
            decoder?.release()
            surface = s
            decoder = s?.let { H264Decoder(it) }
            runCatching { session?.requestKeyframe() }
        }
    }

    private suspend fun runLoop() {
        var backoff = 1000L
        val deviceName = (Build.MODEL ?: "Android").take(64)
        while (currentCoroutineContext().isActive) {
            _ui.value = _ui.value.copy(status = "Connecting…", connected = false)
            val s = try {
                app.core.connect(pcId, deviceName, app.prefs.stream)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                val refusal = (e as? DishusException.Rejected)?.msg
                val pc = runCatching { app.core.pairedPcs().firstOrNull { it.id == pcId } }.getOrNull()
                if (refusal == "Expired" || (pc != null && pc.expiresAt.toLong() <= System.currentTimeMillis() / 1000)) {
                    // Retrying can never succeed.
                    _ui.value = _ui.value.copy(fatal = true, status = EXPIRED_MSG)
                    return
                }
                if (refusal == "Lockdown") {
                    // Keep trying slowly: `dishus unlock` at the PC is a deliberate act.
                    _ui.value = _ui.value.copy(status = LOCKDOWN_MSG)
                    backoff = backoff.coerceAtLeast(5_000)
                } else {
                    _ui.value = _ui.value.copy(status = "Can't reach PC: ${e.message?.take(120)}")
                }
                delay(backoff)
                backoff = (backoff * 2).coerceAtMost(15_000)
                continue
            }
            backoff = 1000L
            var reason = "Cancelled"
            try {
                session = s
                coroutineScope {
                    val video = launch(decodeDispatcher) { quietly { videoLoop(s) } }
                    val stats = launch { quietly { statsLoop(s) } }
                    reason = try {
                        controlLoop(s)
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        "Error: ${e.message?.take(80)}"
                    }
                    video.cancel(); stats.cancel()
                }
            } finally {
                session = null
                // Runs on cancellation too (stop/background): never leave keys held on the PC.
                withContext(NonCancellable) {
                    runCatching { s.releaseAll() }
                    runCatching { withTimeoutOrNull(1500) { s.disconnect() } }
                    s.destroy()
                }
                mods.clear(); publishMods()
            }
            val fatal = reason in setOf("Revoked", "Killed", "Expired")
            _ui.value = _ui.value.copy(
                connected = false,
                fatal = fatal,
                status = when (reason) {
                    "Revoked" -> "This phone's pairing was revoked or expired on the PC."
                    "Killed" -> "The PC cut all sessions (dishus kill)."
                    "Replaced" -> "Another session took over."
                    "Expired" -> EXPIRED_MSG
                    "Lockdown" -> LOCKDOWN_MSG
                    else -> "Disconnected ($reason). Reconnecting…"
                },
            )
            if (fatal || reason == "Replaced") return
            if (reason == "Lockdown") backoff = backoff.coerceAtLeast(5_000)
            delay(backoff)
        }
    }

    /** Returns the close reason. */
    private suspend fun controlLoop(s: RemoteSession): String {
        while (true) {
            when (val ev = s.nextControl()) {
                null -> continue
                is ControlEvent.Welcome -> _ui.value = _ui.value.copy(
                    connected = true, status = "Connected to ${ev.pcName}",
                    monitors = ev.monitors, expiresAt = ev.expiresAt.toLong(), fatal = false,
                )
                is ControlEvent.StreamInfo -> {
                    _ui.value = _ui.value.copy(
                        encoder = ev.encoder, width = ev.width.toInt(), height = ev.height.toInt(),
                        fps = ev.fps.toInt(), bitrateKbps = ev.bitrateKbps.toInt(),
                    )
                    withContext(decodeDispatcher) { decoder?.reset(ev.width.toInt(), ev.height.toInt()) }
                }
                is ControlEvent.Renewed -> {
                    _ui.value = _ui.value.copy(expiresAt = ev.expiresAt.toLong(), notice = "Pairing renewed")
                    runCatching { app.core.recordRenewal(pcId, ev.expiresAt); app.persistVault() }
                }
                is ControlEvent.Notice -> _ui.value = _ui.value.copy(notice = ev.message)
                is ControlEvent.Closed -> return ev.reason
            }
        }
    }

    /** FFI calls on a closed session throw; a dying child must not crash the app. */
    private inline fun quietly(block: () -> Unit) {
        try { block() } catch (e: CancellationException) { throw e } catch (_: Exception) {}
    }

    private suspend fun videoLoop(s: RemoteSession) {
        var lastKfRequest = 0L
        while (true) {
            val f = s.nextVideo() ?: return
            val d = decoder ?: continue
            val ok = d.feed(f.data, f.ptsUs.toLong(), f.keyframe)
            if (!ok) {
                val now = System.currentTimeMillis()
                if (now - lastKfRequest > 300) {
                    lastKfRequest = now
                    runCatching { s.requestKeyframe() }
                }
            }
        }
    }

    private suspend fun statsLoop(s: RemoteSession) {
        while (true) {
            delay(1000)
            _ui.value = _ui.value.copy(rttMs = s.rttMs().toLong(), path = s.path())
        }
    }

    fun clearNotice() { _ui.value = _ui.value.copy(notice = null) }

    // ---------- input ----------

    private fun send(block: (RemoteSession) -> Unit) {
        val s = session ?: return
        runCatching { block(s) }
    }

    /** Non-modifier key press/release. Releasing a key consumes sticky modifiers. */
    fun key(code: Int, down: Boolean) {
        if (code in Evdev.MODIFIERS) return
        send { it.key(code.toUShort(), down) }
        if (!down) releaseSticky()
    }

    /** Tap on a modifier: OFF → STICKY → OFF; LOCKED → OFF. */
    fun modifierTap(code: Int) {
        when (mods[code] ?: ModState.OFF) {
            ModState.OFF -> { mods[code] = ModState.STICKY; send { it.key(code.toUShort(), true) } }
            ModState.STICKY, ModState.LOCKED -> { mods.remove(code); send { it.key(code.toUShort(), false) } }
        }
        publishMods()
    }

    /** Long-press on a modifier: lock it down until tapped again. */
    fun modifierLock(code: Int) {
        if (mods[code] == null) send { it.key(code.toUShort(), true) }
        mods[code] = ModState.LOCKED
        publishMods()
    }

    private fun releaseSticky() {
        val sticky = mods.filterValues { it == ModState.STICKY }.keys
        if (sticky.isEmpty()) return
        for (c in sticky) { mods.remove(c); send { it.key(c.toUShort(), false) } }
        publishMods()
    }

    private fun publishMods() { _ui.value = _ui.value.copy(mods = HashMap(mods)) }

    fun mouseMove(dx: Int, dy: Int) {
        if (dx == 0 && dy == 0) return
        send { it.mouseMove(dx.coerceIn(-4096, 4096).toShort(), dy.coerceIn(-4096, 4096).toShort()) }
    }

    fun button(b: Int, down: Boolean) {
        send { it.mouseButton(b.toUByte(), down) }
        if (!down) releaseSticky()
    }

    fun click(b: Int) { button(b, true); button(b, false) }

    fun scroll(dx: Int, dy: Int) {
        if (dx == 0 && dy == 0) return
        send { it.scroll(dx.coerceIn(-2400, 2400).toShort(), dy.coerceIn(-2400, 2400).toShort()) }
    }

    fun releaseAll() {
        mods.clear(); publishMods()
        send { it.releaseAll() }
    }

    fun applyStream(settings: StreamSettings) {
        app.prefs.stream = settings
        send { it.setStream(settings) }
    }

    fun renew(days: Int) = send { it.renew(days.toUShort()) }
}
