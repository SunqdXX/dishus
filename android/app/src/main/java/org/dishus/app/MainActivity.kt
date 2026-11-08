package org.dishus.app

import android.os.Bundle
import android.view.WindowManager
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.fragment.app.FragmentActivity
import org.dishus.app.ui.DishusTheme
import org.dishus.app.ui.HomeScreen
import org.dishus.app.ui.LockScreen
import org.dishus.app.ui.RemoteScreen

/** FragmentActivity (not ComponentActivity) because BiometricPrompt needs it. */
class MainActivity : FragmentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Secure from the first frame; Root relaxes it only when unlocked AND the user allowed
        // screenshots in Settings.
        window.setFlags(WindowManager.LayoutParams.FLAG_SECURE, WindowManager.LayoutParams.FLAG_SECURE)
        enableEdgeToEdge()
        val app = application as DishusApp
        setContent {
            DishusTheme {
                // The Surface supplies the theme background AND the content colour; without it
                // plain Text falls back to black and is invisible on a dark window.
                Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) { Root(app, this) }
            }
        }
    }
}

@Composable
private fun Root(app: DishusApp, activity: FragmentActivity) {
    val locked by app.locked.collectAsState()
    val shotsAllowed by app.screenshotsAllowed.collectAsState()
    // The lock screen (password entry) is always protected; elsewhere it's the user's choice.
    val secure = locked || !shotsAllowed
    SideEffect {
        if (secure) activity.window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        else activity.window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }
    var remotePc by rememberSaveable { mutableStateOf<String?>(null) }
    var autoConnectDone by remember { mutableStateOf(false) }

    if (locked) {
        remotePc = null
        LockScreen(app, activity)
        return
    }
    // Fast connect: straight to the last PC right after unlocking.
    LaunchedEffect(Unit) {
        if (!autoConnectDone && app.prefs.autoConnect) {
            autoConnectDone = true
            val pcs = runCatching { app.core.pairedPcs() }.getOrDefault(emptyList())
            val target = pcs.firstOrNull { it.id == app.prefs.lastPcId } ?: pcs.singleOrNull()
            if (target != null) remotePc = target.id
        }
    }
    val pc = remotePc
    if (pc == null) {
        HomeScreen(app, onConnect = { id -> app.prefs.lastPcId = id; remotePc = id })
    } else {
        RemoteScreen(app, pc, onExit = { remotePc = null })
    }
}
