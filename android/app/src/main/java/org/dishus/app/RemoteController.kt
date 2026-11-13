package org.dishus.app

import android.os.Build
import android.util.Log
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
import org.dishus.app.audio.AudioPlayer
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
    /** The PC can send audio (its DiShus speaks session/2). */
    val audioSupported: Boolean = false,
    val audioOn: Boolean = false,
)

/**
 * Owns one remote session: connect with backoff, pump video into the decoder, surface control
 * events as [ui], and translate keyboard/touch actions into protocol messages.
 */
class RemoteController(private val app: DishusApp, private val pcId: String) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val decodeDispatcher = Executors.newSingleThreadExecutor { r -> Thread(r, "dishus-decode") }.asCoroutineDispatcher()
    private val audioDispatcher = Executors.newSingleThreadExecutor { r -> Thread(r, "dishus-audio") }.asCoroutineDispatcher()
    /** Video arrival minus capture time over the last second (µs, raw clocks), for the A/V log. */
    @Volatile private var videoDelayUs: Long? = null
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
            audioDispatcher.close()
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
                app.core.connect(pcId, deviceName, app.prefs.toSend(app.prefs.stream))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                val refusal = (e as? DishusException.Rejected)?.message
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
                val audioOk = s.audioSupported()
                _ui.value = _ui.value.copy(audioSupported = audioOk, audioOn = audioOk && app.prefs.audioOn)
                if (audioOk && app.prefs.audioOn) runCatching { s.setAudio(true) }
                coroutineScope {
                    val video = launch(decodeDispatcher) { quietly("video") { videoLoop(s) } }
                    val audio = launch(audioDispatcher) { quietly("audio") { audioLoop(s) } }
                    val stats = launch { quietly("stats") { statsLoop(s) } }
                    reason = try {
                        controlLoop(s)
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        "Error: ${e.message?.take(80)}"
                    }
                    video.cancel(); audio.cancel(); stats.cancel()
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
                    "Killed" -> "The PC cut all sessions (dishus kill). After `dishus unlock` on the PC, tap Connect again."
                    "Replaced" -> "Another session took over."
                    "Expired" -> EXPIRED_MSG
                    "Lockdown" -> LOCKDOWN_MSG
                    else -> "Disconnected ($reason). Reconnecting…"
                },
            )
            if (fatal || reason == "Replaced") return
            if (reason == "Lockdown") backoff = backoff.coerceAtLeast(5_000)
            // The PC went quiet (usually the network changed under us): retry at once; if it is
            // really gone, the failed connect falls back to the normal backoff.
            delay(if (reason == "PC not responding") 300L else backoff)
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
                is ControlEvent.AudioStarted -> Log.i("DishusAudio", "PC audio started: ${ev.sampleRate} Hz, ${ev.channels} ch")
                is ControlEvent.Closed -> return ev.reason
            }
        }
    }

    /** FFI calls on a closed session throw; a dying child must not crash the app. */
    private inline fun quietly(name: String, block: () -> Unit) {
        try { block() } catch (e: CancellationException) { throw e } catch (e: Exception) {
            Log.e("Dishus", "$name loop died", e)
        }
    }

    private suspend fun videoLoop(s: RemoteSession) {
        var lastKfRequest = 0L
        // Once a second in logcat (tag DishusVideo): where frames stop when the picture freezes.
        var rx = 0; var fed = 0; var rejected = 0; var noDecoder = 0; var keys = 0; var kfReq = 0
        var bytes = 0L
        // Delay growth: arrival minus capture time (different clocks), relative to the session's
        // fastest frame. 0 = as fast as the best frame; rising = frames are queueing somewhere.
        var baseDelayUs = Long.MAX_VALUE; var delaySumUs = 0L; var delayMaxUs = 0L
        var lastLog = System.currentTimeMillis()
        while (true) {
            val f = s.nextVideo() ?: return
            rx++; if (f.keyframe) keys++
            bytes += f.data.size
            val delayUs = android.os.SystemClock.elapsedRealtimeNanos() / 1000 - f.ptsUs.toLong()
            if (delayUs < baseDelayUs) baseDelayUs = delayUs
            val extra = delayUs - baseDelayUs
            delaySumUs += extra; if (extra > delayMaxUs) delayMaxUs = extra
            val now = System.currentTimeMillis()
            if (now - lastLog >= 1000) {
                val secs = (now - lastLog) / 1000.0
                val dec = decoder?.takeDecodeStats()
                Log.i("DishusVideo", "rx=$rx keys=$keys fed=$fed rejected=$rejected noDecoder=$noDecoder kfReq=$kfReq " +
                    "rendered=${decoder?.rendered?.getAndSet(0)} surface=${surface != null} " +
                    "kbps=${(bytes * 8 / 1000 / secs).toInt()} delay_avg=${delaySumUs / 1000 / rx}ms " +
                    "delay_max=${delayMaxUs / 1000}ms decode_avg=${dec?.first?.let { "%.1f".format(it) }}ms " +
                    "decode_max=${dec?.second?.let { "%.1f".format(it) }}ms rtt=${s.rttMs()}ms path=${s.path()}")
                videoDelayUs = baseDelayUs + delaySumUs / rx
                rx = 0; fed = 0; rejected = 0; noDecoder = 0; keys = 0; kfReq = 0; lastLog = now
                bytes = 0; delaySumUs = 0; delayMaxUs = 0
            }
            val d = decoder ?: run { noDecoder++; null } ?: continue
            val ok = try {
                d.feed(f.data, f.ptsUs.toLong(), f.keyframe)
            } catch (e: Exception) { // feed() catches everything; belt and braces
                Log.e("DishusVideo", "feed threw", e)
                runCatching { d.release() }
                false
            }
            if (ok) fed++ else rejected++
            if (!ok) {
                if (now - lastKfRequest > 300) {
                    lastKfRequest = now
                    kfReq++
                    runCatching { s.requestKeyframe() }
                }
            }
        }
    }

    /** Only touched on [audioDispatcher]. One per audio stream: the PC numbers packets from 1. */
    private var player: AudioPlayer? = null

    private fun releasePlayer() { player?.release(); player = null }

    /** Plays PC audio while it's on. */
    private suspend fun audioLoop(s: RemoteSession) {
        try {
            while (true) {
                val p = s.nextAudio() ?: return
                if (!_ui.value.audioOn) continue // turned off; stragglers still in flight
                val pl = player ?: AudioPlayer(app).also { it.setVolume(app.prefs.audioVolume); player = it }
                pl.play(p.seq.toLong(), p.ptsUs.toLong(), p.data, videoDelayUs)
            }
        } finally {
            withContext(NonCancellable) { releasePlayer() }
        }
    }

    /** One tap: PC audio on or off; remembered for next time. */
    fun setAudio(on: Boolean) {
        if (!_ui.value.audioSupported) return
        app.prefs.audioOn = on
        _ui.value = _ui.value.copy(audioOn = on)
        send { it.setAudio(on) }
        // Off: free the player now (audio focus, the speaker path); a new stream gets a new one.
        if (!on) scope.launch(audioDispatcher) { releasePlayer() }
    }

    fun setVolume(v: Float) {
        app.prefs.audioVolume = v
        scope.launch(audioDispatcher) { player?.setVolume(v) }
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
        send { it.setStream(app.prefs.toSend(settings)) }
    }

    fun renew(days: Int) = send { it.renew(days.toUShort()) }
}
