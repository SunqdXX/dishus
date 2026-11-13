package org.dishus.app.audio

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTimestamp
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.os.SystemClock
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * PC audio: Opus packets (48 kHz stereo, 20 ms) → Android's built-in Opus decoder → a
 * low-latency AudioTrack. Not thread-safe: one thread calls [play] and [release].
 *
 * Latency is bounded: playback starts once [START_MS] is queued, and while more than [MAX_MS] is
 * queued new packets are dropped, so a slow network or clock drift between PC and phone never
 * builds up a lag. A lost packet is not concealed (Android's decoder has no PLC entry point):
 * the buffer covers it, or the track briefly plays silence (counted as an underrun).
 */
class AudioPlayer(context: Context) {
    private companion object {
        const val RATE = 48_000
        const val CHANNELS = 2
        const val FRAME_BYTES = 2 * CHANNELS
        const val START_MS = 40
        const val MAX_MS = 120
        const val TAG = "DishusAudio"
    }

    private val audioManager = context.getSystemService(AudioManager::class.java)
    private val attrs = AudioAttributes.Builder()
        .setUsage(AudioAttributes.USAGE_MEDIA)
        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
        .build()

    /** Another app took audio focus (a call, a voice message): stay silent until it's back. */
    @Volatile private var ducked = false
    @Volatile private var volume = 1f
    private val focus = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
        .setAudioAttributes(attrs)
        .setOnAudioFocusChangeListener { change ->
            ducked = change != AudioManager.AUDIOFOCUS_GAIN
            applyVolume()
        }
        .build()

    private val track: AudioTrack = AudioTrack.Builder()
        .setAudioAttributes(attrs)
        .setAudioFormat(
            AudioFormat.Builder()
                .setSampleRate(RATE)
                .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .build()
        )
        .setBufferSizeInBytes(
            maxOf(
                AudioTrack.getMinBufferSize(RATE, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT) * 2,
                RATE * FRAME_BYTES * (MAX_MS + 40) / 1000,
            )
        )
        .setTransferMode(AudioTrack.MODE_STREAM)
        .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
        .build()

    private val codec: MediaCodec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS).apply {
        val f = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, RATE, CHANNELS)
        // RFC 7845 identification header; pre-skip 0 (live stream, nothing to trim).
        val head = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
            .put("OpusHead".toByteArray()).put(1).put(CHANNELS.toByte())
            .putShort(0).putInt(RATE).putShort(0).put(0)
        head.flip()
        f.setByteBuffer("csd-0", head)
        // Pre-skip and seek pre-roll in ns, as Android's decoder wants them (native order).
        f.setByteBuffer("csd-1", ns(0))
        f.setByteBuffer("csd-2", ns(80_000_000))
        configure(f, null, null, 0)
        start()
    }

    private fun ns(v: Long): ByteBuffer =
        ByteBuffer.allocate(8).order(ByteOrder.nativeOrder()).putLong(v).also { it.flip() }

    private var framesWritten = 0L
    private var playing = false
    private var lastSeq = -1L
    private val pcm = ByteArray(RATE * FRAME_BYTES / 10) // 100 ms, more than one packet

    // Once a second: packets, lost (seq gaps), dropped (buffer over MAX_MS), underruns,
    // buffer ms, and the sound-vs-picture offset.
    private var rx = 0; private var lost = 0; private var dropped = 0
    private var lastUnderruns = 0
    private var audioDelaySumUs = 0L
    private var lastLog = SystemClock.elapsedRealtime()
    private val ts = AudioTimestamp()

    init {
        audioManager.requestAudioFocus(focus)
        applyVolume()
    }

    fun setVolume(v: Float) {
        volume = v.coerceIn(0f, 1f)
        applyVolume()
    }

    private fun applyVolume() {
        runCatching { track.setVolume(if (ducked) 0f else volume) }
    }

    /** Audio queued in the track that hasn't played yet. */
    private fun queuedMs(): Long =
        ((framesWritten - track.playbackHeadPosition.toLong()) * 1000 / RATE).coerceAtLeast(0)

    /**
     * One packet from the PC. [videoDelayUs]: the video's current arrival-minus-capture time on
     * the same clocks, for the offset in the log (null = no video yet).
     */
    fun play(seq: Long, ptsUs: Long, data: ByteArray, videoDelayUs: Long?) {
        rx++
        if (lastSeq >= 0 && seq > lastSeq + 1) lost += (seq - lastSeq - 1).toInt()
        if (seq <= lastSeq) return // late duplicate / reordered: too late to play
        lastSeq = seq
        val arrivalUs = SystemClock.elapsedRealtimeNanos() / 1000
        // Ran dry (the PC was silent: an idle output sends nothing, or the network stalled):
        // queue START_MS again before playing, so playback doesn't stutter packet by packet.
        if (playing && queuedMs() == 0L) { track.pause(); playing = false }
        // Decode always (keeps the decoder's state continuous), write only if there's room.
        val idx = codec.dequeueInputBuffer(5_000)
        if (idx >= 0) {
            codec.getInputBuffer(idx)!!.apply { clear(); put(data) }
            codec.queueInputBuffer(idx, 0, data.size, ptsUs, 0)
        }
        val info = MediaCodec.BufferInfo()
        while (true) {
            val out = codec.dequeueOutputBuffer(info, 2_000)
            if (out == MediaCodec.INFO_TRY_AGAIN_LATER) break
            if (out < 0) continue // format / buffers changed: the PCM is in the next call
            val buf = codec.getOutputBuffer(out)!!
            val n = minOf(info.size, pcm.size)
            buf.position(info.offset); buf.get(pcm, 0, n)
            codec.releaseOutputBuffer(out, false)
            if (playing && queuedMs() > MAX_MS) {
                dropped++
                continue
            }
            val w = track.write(pcm, 0, n, AudioTrack.WRITE_NON_BLOCKING)
            if (w > 0) framesWritten += w / FRAME_BYTES
            if (!playing && queuedMs() >= START_MS) {
                track.play(); playing = true
            }
        }
        audioDelaySumUs += arrivalUs - ptsUs
        logOnceASecond(videoDelayUs)
    }

    private fun logOnceASecond(videoDelayUs: Long?) {
        val now = SystemClock.elapsedRealtime()
        if (now - lastLog < 1000 || rx == 0) return
        val underruns = runCatching { track.underrunCount }.getOrDefault(0)
        // Output latency: frames written but not yet heard, from the track's own timestamp.
        val outMs = if (track.getTimestamp(ts)) {
            val sincePresented = (SystemClock.elapsedRealtimeNanos() - ts.nanoTime) / 1_000_000
            ((framesWritten - ts.framePosition) * 1000 / RATE - sincePresented).coerceAtLeast(0)
        } else queuedMs()
        // Sound vs picture: (arrival - capture) of audio plus its time to the speaker, minus the
        // same for video (arrival - capture; decode adds a few ms). The PC/phone clock offset
        // is in both and cancels. Positive = sound later than picture.
        val offset = videoDelayUs?.let { ((audioDelaySumUs / rx) - it) / 1000 + outMs }
        Log.i(TAG, "rx=$rx lost=$lost dropped=$dropped underruns=${underruns - lastUnderruns} " +
            "buffer=${queuedMs()}ms out=${outMs}ms av_offset=${offset?.let { "${it}ms" } ?: "-"} " +
            "vol=${if (ducked) 0f else volume}")
        lastUnderruns = underruns
        rx = 0; lost = 0; dropped = 0; audioDelaySumUs = 0; lastLog = now
    }

    fun release() {
        runCatching { audioManager.abandonAudioFocusRequest(focus) }
        runCatching { track.stop() }; runCatching { track.release() }
        runCatching { codec.stop() }; runCatching { codec.release() }
        Log.i(TAG, "audio player released")
    }
}
