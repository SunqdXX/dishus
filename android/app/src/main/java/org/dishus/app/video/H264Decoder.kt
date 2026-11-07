package org.dishus.app.video

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.view.Surface
import java.nio.ByteBuffer

/**
 * Hardware H.264 decode straight onto a Surface, tuned for latency:
 * synchronous MediaCodec, KEY_LOW_LATENCY where supported, every decoded frame rendered
 * immediately (no presentation-time pacing).
 *
 * The PC sends SPS/PPS in-band before every IDR; we pull them out of the first IDR to configure
 * the codec. [feed] returns false when the caller should request a keyframe.
 */
class H264Decoder(private val surface: Surface) {
    private var codec: MediaCodec? = null
    private var width = 1920
    private var height = 1080
    private val info = MediaCodec.BufferInfo()

    /** Called on StreamInfo: a new stream (possibly a different size) starts with an IDR. */
    fun reset(w: Int, h: Int) {
        if (w > 0 && h > 0) {
            width = w; height = h
        }
        release()
    }

    fun feed(data: ByteArray, ptsUs: Long, keyframe: Boolean): Boolean {
        try {
            val c = codec ?: run {
                if (!keyframe) return false
                val (sps, pps) = extractParameterSets(data) ?: return false
                configure(sps, pps)
            }
            val idx = c.dequeueInputBuffer(20_000)
            if (idx < 0) {
                // Decoder saturated: dropping this frame breaks the reference chain.
                drain(c)
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
            drain(c)
            return true
        } catch (e: MediaCodec.CodecException) {
            release()
            return false
        } catch (e: IllegalStateException) {
            release()
            return false
        }
    }

    private fun drain(c: MediaCodec) {
        while (true) {
            val out = c.dequeueOutputBuffer(info, 0)
            when {
                out >= 0 -> c.releaseOutputBuffer(out, true)
                out == MediaCodec.INFO_TRY_AGAIN_LATER -> return
                else -> continue // format / buffers changed
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
        return c
    }

    fun release() {
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
