package org.dishus.app

import android.os.Bundle
import android.view.WindowManager
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.fragment.app.FragmentActivity
import org.dishus.app.ui.DishusTheme
import org.dishus.app.ui.HomeScreen
import org.dishus.app.ui.LockScreen
import org.dishus.app.ui.RemoteScreen

/** FragmentActivity (not ComponentActivity) because BiometricPrompt needs it. */
class MainActivity : FragmentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // The remote screen shows the PC desktop: never in screenshots, recents thumbnails or casts.
        window.setFlags(WindowManager.LayoutParams.FLAG_SECURE, WindowManager.LayoutParams.FLAG_SECURE)
        enableEdgeToEdge()
        val app = application as DishusApp
        setContent {
            DishusTheme { Root(app, this) }
        }
    }
}

@Composable
private fun Root(app: DishusApp, activity: FragmentActivity) {
    val locked by app.locked.collectAsState()
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
