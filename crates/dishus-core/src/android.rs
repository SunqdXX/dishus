//! Android JNI bootstrap.
//!
//! iroh reads the system DNS servers (`n0-dns-resolver`) and enumerates network interfaces
//! (`netdev`) through JNI, via the `ndk-context` crate. That crate must be told the `JavaVM` and
//! an Android `Context` once, before any endpoint is created; otherwise the first network
//! operation panics ("android context was not initialized") and, in a `panic = "abort"` build,
//! kills the app.
//!
//! The app calls `org.dishus.app.NativeInit.init(applicationContext)` once from
//! `Application.onCreate`. This is the only `unsafe` code in the workspace: it deals in raw JNI
//! pointers, which cannot be expressed safely. The Context is pinned with a JNI global
//! reference that is intentionally never released (the Application context lives as long as the
//! process), satisfying `install_android_jni_context`'s "valid until the process exits".
#![allow(unsafe_code)]

use std::sync::Once;

use jni::JNIEnv;
use jni::objects::{JClass, JObject};

static INIT: Once = Once::new();

/// JNI entry point for `NativeInit.init(context: Context)` (a `@JvmStatic` on a Kotlin object).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_dishus_app_NativeInit_init(
    env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    INIT.call_once(|| {
        let (Ok(vm), Ok(global)) = (env.get_java_vm(), env.new_global_ref(&context)) else {
            tracing::error!("android: could not obtain JavaVM / global ref for the Context");
            return;
        };
        let vm_ptr = vm.get_java_vm_pointer().cast::<std::ffi::c_void>();
        let ctx_ptr = global.as_obj().as_raw().cast::<std::ffi::c_void>();
        // Leak the global ref on purpose: the pointer must stay valid for the process lifetime.
        std::mem::forget(global);
        // SAFETY: `vm_ptr` is the process-wide JavaVM; `ctx_ptr` is a JNI global reference to the
        // Application context that is never deleted. `Once` guarantees a single initialisation.
        unsafe { iroh::dns::install_android_jni_context(vm_ptr, ctx_ptr) };
    });
}
