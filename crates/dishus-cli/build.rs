//! Release builds for Windows carry the daemon inside `dishus.exe`, so one downloaded file is
//! enough: set `DISHUS_EMBED_DAEMON` to a built `dishus-daemon.exe` (packaging/windows/build.ps1
//! does). Without it nothing is embedded and nothing changes.

fn main() {
    println!("cargo::rerun-if-env-changed=DISHUS_EMBED_DAEMON");
    println!("cargo::rustc-check-cfg=cfg(dishus_embed_daemon)");
    let Some(src) = std::env::var_os("DISHUS_EMBED_DAEMON").filter(|v| !v.is_empty()) else {
        return;
    };
    let src = std::path::PathBuf::from(src);
    println!("cargo::rerun-if-changed={}", src.display());
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
        .join("dishus-daemon.exe");
    std::fs::copy(&src, &out)
        .unwrap_or_else(|e| panic!("DISHUS_EMBED_DAEMON={}: {e}", src.display()));
    println!("cargo::rustc-cfg=dishus_embed_daemon");
}
