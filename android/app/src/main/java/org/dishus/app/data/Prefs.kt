package org.dishus.app.data

import android.content.Context
import uniffi.dishus_core.StreamQuality
import uniffi.dishus_core.StreamSettings

/** Non-secret preferences. Secrets live only in the vault. */
class Prefs(context: Context) {
    private val sp = context.getSharedPreferences("dishus", Context.MODE_PRIVATE)
    private val res = context.resources

    companion object {
        /** Resolution "Phone", the default: the shorter side of this phone's screen. */
        const val PHONE_HEIGHT = 0xFFFF
    }

    var lastPcId: String?
        get() = sp.getString("last_pc", null)
        set(v) = sp.edit().putString("last_pc", v).apply()

    var autoConnect: Boolean
        get() = sp.getBoolean("auto_connect", true)
        set(v) = sp.edit().putBoolean("auto_connect", v).apply()

    /** 0 = lock immediately when the app leaves the foreground. */
    var autoLockMinutes: Int
        get() = sp.getInt("auto_lock_min", 5)
        set(v) = sp.edit().putInt("auto_lock_min", v.coerceIn(0, 60)).apply()

    /**
     * Off by default (FLAG_SECURE: no screenshots, screen recording or recents thumbnail of the
     * PC screen). The user may allow it, e.g. to screenshot the PC for someone. The lock screen
     * stays protected regardless.
     */
    var allowScreenshots: Boolean
        get() = sp.getBoolean("allow_screenshots", false)
        set(v) = sp.edit().putBoolean("allow_screenshots", v).apply()

    var haptics: Boolean
        get() = sp.getBoolean("haptics", true)
        set(v) = sp.edit().putBoolean("haptics", v).apply()

    var pointerSpeed: Float
        get() = sp.getFloat("pointer_speed", 1.6f)
        set(v) = sp.edit().putFloat("pointer_speed", v.coerceIn(0.3f, 5f)).apply()

    var relays: List<String>
        get() = sp.getString("relays", "")!!.split('\n').map { it.trim() }.filter { it.isNotEmpty() }
        set(v) = sp.edit().putString("relays", v.joinToString("\n")).apply()

    var disableRelays: Boolean
        get() = sp.getBoolean("disable_relays", false)
        set(v) = sp.edit().putBoolean("disable_relays", v).apply()

    var stream: StreamSettings
        get() = StreamSettings(
            monitor = sp.getInt("monitor", 0).toUByte(),
            fps = sp.getInt("fps", 0).toUByte(),
            quality = runCatching { StreamQuality.valueOf(sp.getString("quality", "AUTO")!!) }
                .getOrDefault(StreamQuality.AUTO),
            maxHeight = sp.getInt("max_height", PHONE_HEIGHT).toUShort(),
        )
        set(v) = sp.edit()
            .putInt("monitor", v.monitor.toInt())
            .putInt("fps", v.fps.toInt())
            .putString("quality", v.quality.name)
            .putInt("max_height", v.maxHeight.toInt())
            .apply()

    /** [s] as sent to the PC: "Phone" becomes the screen's shorter side in pixels, read now so it
     *  follows a foldable being opened or closed. The PC never scales up. */
    fun toSend(s: StreamSettings): StreamSettings {
        if (s.maxHeight.toInt() != PHONE_HEIGHT) return s
        val h = res.displayMetrics.let { minOf(it.widthPixels, it.heightPixels) }
        return s.copy(maxHeight = h.coerceIn(240, 4320).toUShort())
    }
}
