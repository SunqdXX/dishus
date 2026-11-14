package org.dishus.app

import android.app.Application
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.os.Handler
import android.os.Looper
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.dishus.app.data.Prefs
import org.dishus.app.data.VaultStore
import uniffi.dishus_core.Core
import uniffi.dishus_core.RelaySettings

class DishusApp : Application() {
    lateinit var core: Core
        private set
    lateinit var vault: VaultStore
        private set
    lateinit var prefs: Prefs
        private set

    private val _locked = MutableStateFlow(true)
    /** True whenever the Rust core holds no secrets. UI navigates to the lock screen. */
    val locked: StateFlow<Boolean> = _locked

    /** Mirrors [Prefs.allowScreenshots] so the activity can toggle FLAG_SECURE live. */
    val screenshotsAllowed = MutableStateFlow(false)

    /** Set while the app is in the background; consumers close live sessions. */
    val backgrounded = MutableStateFlow(false)

    private val handler = Handler(Looper.getMainLooper())
    private val lockRunnable = Runnable { lockNow() }

    /**
     * The system file picker is another app: opening it stops ours, which would drop the session
     * and maybe lock. While it is open (at most [PICKER_GRACE_MS]) leaving doesn't count; if the
     * user goes elsewhere from the picker, the normal rules apply when the grace runs out.
     */
    @Volatile private var pickerUntil = 0L
    private var stopped = false
    private val pickerExpired = Runnable { if (stopped) wentToBackground() }

    fun pickerOpening() {
        pickerUntil = android.os.SystemClock.elapsedRealtime() + PICKER_GRACE_MS
    }

    fun pickerClosed() {
        pickerUntil = 0L
        handler.removeCallbacks(pickerExpired)
    }

    private fun wentToBackground() {
        backgrounded.value = true
        val minutes = prefs.autoLockMinutes
        if (minutes == 0) lockNow() else handler.postDelayed(lockRunnable, minutes * 60_000L)
    }

    override fun onCreate() {
        super.onCreate()
        System.setProperty("jna.nosys", "true")
        NativeInit.init(applicationContext) // must precede any networking in the core
        core = Core()
        vault = VaultStore(this)
        prefs = Prefs(this)
        screenshotsAllowed.value = prefs.allowScreenshots
        runCatching { core.setRelays(RelaySettings(prefs.relays, prefs.disableRelays)) }
        watchNetwork()
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStop(owner: LifecycleOwner) {
                stopped = true
                val left = pickerUntil - android.os.SystemClock.elapsedRealtime()
                if (left > 0) handler.postDelayed(pickerExpired, left) else wentToBackground()
            }

            override fun onStart(owner: LifecycleOwner) {
                stopped = false
                handler.removeCallbacks(pickerExpired)
                backgrounded.value = false
                handler.removeCallbacks(lockRunnable)
            }
        })
    }

    /**
     * iroh can't see Android network changes from native code; without this a Wi-Fi <-> mobile
     * switch stalled the session until timeouts fired (15-40 s on a Galaxy Z Fold7).
     */
    private fun watchNetwork() {
        val cm = getSystemService(ConnectivityManager::class.java) ?: return
        val notify = Runnable { runCatching { core.networkChanged() } }
        cm.registerDefaultNetworkCallback(object : ConnectivityManager.NetworkCallback() {
            // Callbacks arrive in bursts; coalesce them.
            private fun changed() {
                handler.removeCallbacks(notify)
                handler.postDelayed(notify, 200)
            }
            override fun onAvailable(network: Network) = changed()
            override fun onLost(network: Network) = changed()
            override fun onLinkPropertiesChanged(network: Network, lp: LinkProperties) = changed()
        })
    }

    fun markUnlocked() {
        _locked.value = !core.isUnlocked()
    }

    fun lockNow() {
        handler.removeCallbacks(lockRunnable)
        core.lock()
        _locked.value = true
    }

    /** Re-seal the vault after a change (pairing, forget, renewal) and persist it. */
    fun persistVault() {
        vault.save(core.exportVault())
    }
}

/** How long the file picker may stay open before leaving the app counts as leaving it. */
private const val PICKER_GRACE_MS = 120_000L
