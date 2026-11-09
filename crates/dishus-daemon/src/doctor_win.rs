//! `dishus-daemon --doctor` (and `dishus doctor`) on Windows: can this PC stream?
//!
//! Prints the Windows version and edition, the GPUs, whether Desktop Duplication works on each
//! monitor, and *really* encodes a test pattern with every H.264 encoder Media Foundation offers
//! for the GPUs that drive a monitor (a listed encoder proves nothing). Exit code 0 = video will
//! work.

use crate::capture::win::{dxgi, gpupref, mf, pipeline};

const NT_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// "Windows 11 Pro 25H2 (build 26200)"; the registry still says "Windows 10" on Windows 11.
pub fn windows_version() -> (String, String) {
    let get = |v| crate::winsys::hklm_string(NT_KEY, v).unwrap_or_default();
    let build = get("CurrentBuild");
    let mut product = get("ProductName");
    if build.parse::<u32>().is_ok_and(|b| b >= 22000) {
        product = product.replace("Windows 10", "Windows 11");
    }
    let display = get("DisplayVersion");
    (
        format!("{product} {display} (build {build})")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        get("EditionID"),
    )
}

/// N and KN editions ship without Media Foundation.
pub fn is_n_edition(edition: &str) -> bool {
    edition.ends_with('N') && !edition.is_empty()
}

pub fn run() -> i32 {
    let _com = pipeline::Com::init();
    println!("DiShus doctor {}", env!("CARGO_PKG_VERSION"));
    let (version, edition) = windows_version();
    println!("  system:    {version} · edition: {edition}");
    // Before this process touches DXGI: on hybrid graphics, pick the GPU that can capture.
    let gpu_note = match gpupref::ensure() {
        Ok(gpupref::Outcome::Works) => None,
        Ok(gpupref::Outcome::Set(name)) => Some(format!(
            "  · hybrid graphics: set the Windows graphics preference of dishus-daemon.exe to\n    {name} (Settings > System > Display > Graphics), so the screen can be captured"
        )),
        Err(e) => Some(format!("  ✗ {e:#}")),
    };
    let adapters = match dxgi::adapters() {
        Ok(a) => a,
        Err(e) => {
            println!("  ✗ no graphics adapter usable: {e:#}");
            return 1;
        }
    };
    for a in &adapters {
        println!(
            "  GPU:       {} ({} monitor{})",
            a.name,
            a.outputs,
            if a.outputs == 1 { "" } else { "s" }
        );
    }

    println!("\nCapture");
    if let Some(n) = gpu_note {
        println!("{n}");
    }
    let mut ok_video = true;
    match dxgi::outputs() {
        Ok(outs) if outs.is_empty() => {
            println!("  ✗ no monitor attached to the desktop");
            ok_video = false;
        }
        Ok(outs) => {
            for o in &outs {
                let r = dxgi::create_device(&o.adapter)
                    .and_then(|(dev, _)| dxgi::Duplicator::new(&o.output, &dev));
                match r {
                    Ok(d) => println!(
                        "  ✓ {} {}x{}{}: Desktop Duplication works (on {})",
                        o.name,
                        d.width,
                        d.height,
                        if o.primary { " (main)" } else { "" },
                        o.adapter_name
                    ),
                    Err(e) => {
                        println!("  ✗ {}: Desktop Duplication FAILED: {e:#}", o.name);
                        ok_video = false;
                    }
                }
            }
        }
        Err(e) => {
            println!("  ✗ listing monitors failed: {e:#}");
            ok_video = false;
        }
    }

    println!("\nEncoders (DiShus uses the first that works)");
    let mut first_ok: Option<(String, mf::Kind)> = None;
    if let Err(e) = mf::available() {
        println!("  ✗ {e:#}");
        if is_n_edition(&edition) {
            println!("    This is an N edition of Windows, which comes without media features.");
        }
        ok_video = false;
    } else {
        for a in &adapters {
            if a.outputs == 0 {
                println!(
                    "  ·  {}: drives no monitor, so its encoder is not used (DiShus encodes on the GPU that shows the screen)",
                    a.name
                );
                continue;
            }
            let list = match mf::list(a.luid, a.vendor_id) {
                Ok(l) => l,
                Err(e) => {
                    println!("  ✗ {}: listing encoders failed: {e:#}", a.name);
                    continue;
                }
            };
            if list.is_empty() {
                println!("  ✗ {}: no H.264 encoder found", a.name);
            }
            for e in list {
                let line = match pipeline::probe(&e, a.luid, 720, 30) {
                    Ok(fps) => {
                        first_ok.get_or_insert((e.label.clone(), e.kind));
                        format!("✓  {:<9} works ({fps:.0} fps test encode at 720p)", e.label)
                    }
                    Err(err) => format!(
                        "✗  {:<9} FAILED: {}",
                        e.label,
                        format!("{err:#}").lines().next().unwrap_or("")
                    ),
                };
                println!("  {line} · {}", e.name);
            }
        }
    }
    if first_ok.is_none() {
        ok_video = false;
    }

    println!("\nKeyboard / mouse");
    println!("  ✓ SendInput (nothing to set up)");
    println!(
        "  · Windows doesn't let it reach the lock screen, UAC prompts, or programs running as administrator."
    );

    println!("\nNetwork");
    println!(
        "  · The first time DiShus runs, Windows Defender Firewall may ask whether dishus-daemon may use\n    the network. Allowing it on private networks gives the fastest direct connections. If you\n    said no, DiShus still connects (it reaches out to the phone first, or uses a relay)."
    );

    println!();
    match first_ok {
        Some((label, kind)) if ok_video => println!(
            "RESULT: video works, using {label}{}",
            if kind == mf::Kind::Software {
                " (on the CPU: fine for 720p/30, a GPU encoder is lighter)"
            } else {
                " (GPU)"
            }
        ),
        _ => println!("RESULT: video will NOT work yet"),
    }
    if ok_video { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n_editions() {
        assert!(is_n_edition("ProfessionalN"));
        assert!(is_n_edition("CoreN"));
        assert!(!is_n_edition("Professional"));
        assert!(!is_n_edition("Core"));
        assert!(!is_n_edition(""));
    }
}
