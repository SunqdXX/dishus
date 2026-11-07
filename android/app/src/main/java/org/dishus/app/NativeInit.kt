package org.dishus.app

import android.content.Context

/**
 * Hands the JavaVM and the Application context to the Rust core (see
 * crates/dishus-core/src/android.rs). iroh needs them for Android DNS and network-interface
 * lookups; without this the first network operation aborts the process.
 */
object NativeInit {
    init {
        System.loadLibrary("dishus_core")
    }

    @JvmStatic
    external fun init(context: Context)
}
