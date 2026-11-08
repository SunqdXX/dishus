package org.dishus.app.video

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.view.Surface
import java.nio.ByteBuffer

/**
 * Hardware H.264 decode straight onto a Surface, tuned for latency:
 * KEY_LOW_LATENCY where supported, every decoded frame rendered immediately (no
 * presentation-time pacing).
 *
 * Output is drained on its own thread. It used to be drained only when the next frame was fed,
 * so the newest picture sat inside the codec until the PC sent another frame: on a real desktop
 * (frames only when something changes) the phone always showed the screen one change behind.
 *
 * The PC sends SPS/PPS in-band before every IDR; we pull them out of the first IDR to configure
 * the codec. [feed] returns false when the caller should request a keyframe.
 */
class H264Decoder(private val surface: Surface) {
    private var codec: MediaCodec? = null
    private var width = 1920
    private var height = 1080
    private var output: Thread? = null
    @Volatile private var running = false
    /** Frames handed to the Surface (for the video stats log). */
    val rendered = java.util.concurrent.atomic.AtomicLong()
    /** Set by the output thread when the codec failed; the next feed() resets it. */
    @Volatile private var broken = false

    /** Called on StreamInfo: a new stream (possibly a different size) starts with an IDR. */
    fun reset(w: Int, h: Int) {
        if (w > 0 && h > 0) {
            width = w; height = h
        }
        release()
    }

    fun feed(data: ByteArray, ptsUs: Long, keyframe: Boolean): Boolean {
        try {
            if (broken) release()
            val c = codec ?: run {
                if (!keyframe) return false
                val (sps, pps) = extractParameterSets(data) ?: return false
                configure(sps, pps)
            }
            val idx = c.dequeueInputBuffer(20_000)
            if (idx < 0) {
                // Decoder saturated: dropping this frame breaks the reference chain.
                return false
            }
            val buf = c.getInputBuffer(idx) ?: return false
            buf.clear()
            if (buf.capacity() < data.size) {
                c.queueInputBuffer(idx, 0, 0, ptsUs, 0)
                return false
            }
            buf.put(data)
            c.queueInputBuffer(idx, 0, data.size, ptsUs, if (keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
            return true
        } catch (e: Exception) {
            // Anything (CodecException, IOException from creating the codec, a bad argument):
            // start over from the next keyframe. Never let it escape: that killed the video loop
            // while input kept working ("picture frozen").
            android.util.Log.w("DishusVideo", "decoder failed, resetting", e)
            release()
            return false
        }
    }

    /** Render every decoded frame as soon as it is ready. */
    private fun drainLoop(c: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (running) {
            try {
                val out = c.dequeueOutputBuffer(info, 10_000)
                if (out >= 0) { c.releaseOutputBuffer(out, true); rendered.incrementAndGet() }
                // else: timeout, or format / buffers changed: just loop
            } catch (e: IllegalStateException) { // includes CodecException
                if (running) android.util.Log.w("DishusVideo", "decoder output failed: $e")
                if (running) broken = true // not a release() in progress: feed() resets
                return
            }
        }
    }

    private fun configure(sps: ByteArray, pps: ByteArray): MediaCodec {
        val fmt = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(sps))
            setByteBuffer("csd-1", ByteBuffer.wrap(pps))
            if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 4 * 1024 * 1024)
        }
        val c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        c.configure(fmt, surface, null, 0)
        c.start()
        codec = c
        broken = false
        running = true
        output = Thread({ drainLoop(c) }, "h264-output").apply { isDaemon = true; start() }
        return c
    }

    fun release() {
        running = false
        output?.let { runCatching { it.join(500) } }
        output = null
        codec?.let { runCatching { it.stop() }; runCatching { it.release() } }
        codec = null
    }

    companion object {
        /** Find SPS (type 7) and PPS (type 8) NAL units, returned with their start codes. */
        fun extractParameterSets(au: ByteArray): Pair<ByteArray, ByteArray>? {
            var sps: ByteArray? = null
            var pps: ByteArray? = null
            val starts = mutableListOf<Int>() // index of first byte after a start code
            var i = 0
            while (i + 3 <= au.size) {
                if (au[i] == 0.toByte() && au[i + 1] == 0.toByte()) {
                    if (au[i + 2] == 1.toByte()) { starts += i + 3; i += 3; continue }
                    if (i + 4 <= au.size && au[i + 2] == 0.toByte() && au[i + 3] == 1.toByte()) {
                        starts += i + 4; i += 4; continue
                    }
                }
                i++
            }
            for ((n, s) in starts.withIndex()) {
                if (s >= au.size) continue
                var end = if (n + 1 < starts.size) starts[n + 1] - 3 else au.size
                // trim the zero of a 4-byte start code that belongs to the next NAL
                while (end > s && au[end - 1] == 0.toByte()) end--
                val type = au[s].toInt() and 0x1f
                val nal = byteArrayOf(0, 0, 0, 1) + au.copyOfRange(s, end)
                if (type == 7 && sps == null) sps = nal
                if (type == 8 && pps == null) pps = nal
            }
            return if (sps != null && pps != null) sps to pps else null
        }
    }
}
