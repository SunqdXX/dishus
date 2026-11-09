//! `dishus-daemon --doctor` (and `dishus doctor`): can this PC stream?
//!
//! Checks the GStreamer pieces, then *really* encodes a test pattern with every H.264 encoder
//! (an installed plugin proves nothing: a VA driver may lack H.264 encode, NVENC needs the
//! proprietary driver). Prints what works and, for what doesn't, the exact packages to
//! install for this distro and GPU. Exit code 0 = video will work.

use std::collections::BTreeSet;

use crate::capture::pipeline::{self, Encoder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Arch,
    Debian,
    Fedora,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gpu {
    Nvidia,
    Amd,
    Intel,
}

impl Gpu {
    fn label(self) -> &'static str {
        match self {
            Gpu::Nvidia => "NVIDIA",
            Gpu::Amd => "AMD",
            Gpu::Intel => "Intel",
        }
    }
    /// The hardware encoders that serve this GPU.
    fn encoders(self) -> &'static [Encoder] {
        match self {
            Gpu::Nvidia => &[Encoder::Nvenc],
            Gpu::Amd | Gpu::Intel => &[Encoder::Va, Encoder::VaLowPower],
        }
    }
}

/// `/etc/os-release` → distro family (by `ID`, then `ID_LIKE`).
pub fn parse_os_release(text: &str) -> (Distro, String) {
    let field = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .unwrap_or_default()
    };
    let name = Some(field("PRETTY_NAME"))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| field("ID"));
    let ids = format!("{} {}", field("ID"), field("ID_LIKE"));
    let family = ids
        .split_whitespace()
        .find_map(|id| match id {
            "arch" => Some(Distro::Arch),
            "debian" | "ubuntu" => Some(Distro::Debian),
            "fedora" | "rhel" => Some(Distro::Fedora),
            _ => None,
        })
        .unwrap_or(Distro::Other);
    (family, name)
}

/// PCI vendor id (`/sys/class/drm/cardN/device/vendor`) → GPU maker.
pub fn gpu_from_vendor(id: &str) -> Option<Gpu> {
    match id.trim() {
        "0x10de" => Some(Gpu::Nvidia),
        "0x1002" => Some(Gpu::Amd),
        "0x8086" => Some(Gpu::Intel),
        _ => None,
    }
}

fn detect_gpus() -> BTreeSet<Gpu> {
    let Ok(rd) = std::fs::read_dir("/sys/class/drm") else {
        return BTreeSet::new();
    };
    rd.flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.strip_prefix("card")
                .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|e| std::fs::read_to_string(e.path().join("device/vendor")).ok())
        .filter_map(|v| gpu_from_vendor(&v))
        .collect()
}

/// Something missing or broken, mapped to packages per distro.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    PipeWire,
    Parser,
    X11Capture,
    GpuEncoder(Gpu),
    CpuEncoder,
}

/// Packages that provide `need` on `distro` (empty: no known package).
pub fn packages(distro: Distro, need: Need) -> &'static [&'static str] {
    use {Distro::*, Gpu::*, Need::*};
    match (distro, need) {
        (Arch, PipeWire) => &["gst-plugin-pipewire"],
        (Arch, Parser) | (Arch, GpuEncoder(Nvidia)) => &["gst-plugins-bad-libs"],
        (Arch, X11Capture) => &["gst-plugins-good"],
        // The AMD VA driver ships inside mesa.
        (Arch, GpuEncoder(Amd)) => &["gst-plugin-va", "mesa"],
        (Arch, GpuEncoder(Intel)) => &["gst-plugin-va", "intel-media-driver"],
        (Arch, CpuEncoder) => &["gst-plugins-ugly"],

        (Debian, PipeWire) => &["gstreamer1.0-pipewire"],
        (Debian, Parser) | (Debian, GpuEncoder(Nvidia)) => &["gstreamer1.0-plugins-bad"],
        (Debian, X11Capture) => &["gstreamer1.0-plugins-good"],
        (Debian, GpuEncoder(Amd)) => &["gstreamer1.0-plugins-bad", "mesa-va-drivers"],
        (Debian, GpuEncoder(Intel)) => {
            &["gstreamer1.0-plugins-bad", "intel-media-va-driver-non-free"]
        }
        (Debian, CpuEncoder) => &["gstreamer1.0-plugins-ugly"],

        (Fedora, PipeWire) => &["pipewire-gstreamer"],
        (Fedora, Parser) | (Fedora, GpuEncoder(Nvidia)) => &["gstreamer1-plugins-bad-free"],
        (Fedora, X11Capture) => &["gstreamer1-plugins-good"],
        // Fedora's own mesa has H.264 encode disabled; the RPM Fusion build has it.
        (Fedora, GpuEncoder(Amd)) => &["gstreamer1-plugins-bad-free", "mesa-va-drivers-freeworld"],
        (Fedora, GpuEncoder(Intel)) => &["gstreamer1-plugins-bad-free", "intel-media-driver"],
        (Fedora, CpuEncoder) => &["gstreamer1-plugin-openh264"],

        (Other, _) => &[],
    }
}

/// One install command covering every need, or None if nothing is needed / distro unknown.
pub fn install_command(distro: Distro, needs: &BTreeSet<Need>) -> Option<String> {
    let mut pkgs: Vec<&str> = Vec::new();
    for n in needs {
        for p in packages(distro, *n) {
            if !pkgs.contains(p) {
                pkgs.push(p);
            }
        }
    }
    if pkgs.is_empty() {
        return None;
    }
    let pm = match distro {
        Distro::Arch => "sudo pacman -S --needed",
        Distro::Debian => "sudo apt install",
        Distro::Fedora => "sudo dnf install",
        Distro::Other => return None,
    };
    Some(format!("{pm} {}", pkgs.join(" ")))
}

fn have(factory: &str) -> bool {
    gstreamer::ElementFactory::find(factory).is_some()
}

/// Runs every check and prints the report. Returns the process exit code.
pub fn run() -> i32 {
    let mut ok_video = true;
    println!("DiShus doctor {}", env!("CARGO_PKG_VERSION"));
    if let Err(e) = pipeline::init() {
        println!("  ✗ GStreamer does not start: {e:#}");
        return 1;
    }
    let (major, minor, micro, _) = gstreamer::version();
    let (distro, distro_name) = std::fs::read_to_string("/etc/os-release")
        .map(|t| parse_os_release(&t))
        .unwrap_or((Distro::Other, "unknown Linux".into()));
    let gpus = detect_gpus();
    let gpu_list = if gpus.is_empty() {
        "none found".to_string()
    } else {
        gpus.iter()
            .map(|g| g.label())
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "?".into());
    println!("  system:    {distro_name} · GPU: {gpu_list} · session: {session}");
    println!("  GStreamer: {major}.{minor}.{micro}");

    let mut needs = BTreeSet::new();
    println!("\nCapture");
    let pw = have("pipewiresrc");
    let x11 = have("ximagesrc");
    println!(
        "  {} Wayland/portal capture (pipewiresrc)",
        if pw { "✓" } else { "✗" }
    );
    println!(
        "  {} X11 capture (ximagesrc){}",
        if x11 { "✓" } else { "·" },
        if x11 { "" } else { " — only needed on X11" }
    );
    if !pw {
        needs.insert(Need::PipeWire);
    }
    if !x11 && session == "x11" {
        needs.insert(Need::X11Capture);
    }
    if !pw && !x11 {
        ok_video = false;
    }
    let parser = have("h264parse");
    println!(
        "  {} H.264 parser (h264parse)",
        if parser { "✓" } else { "✗" }
    );
    if !parser {
        needs.insert(Need::Parser);
        ok_video = false;
    }

    println!("\nEncoders (DiShus uses the first that works)");
    let mut first_ok: Option<Encoder> = None;
    let mut works = Vec::new();
    for enc in Encoder::ALL {
        let missing = enc.missing();
        let line = if !missing.is_empty() {
            format!(
                "·  {:<9} not available ({} missing)",
                enc.name(),
                missing.join(", ")
            )
        } else if !parser {
            format!("?  {:<9} installed, untested (needs h264parse)", enc.name())
        } else {
            match pipeline::probe(enc, 720, 30) {
                Ok(fps) => {
                    works.push(enc);
                    first_ok.get_or_insert(enc);
                    format!(
                        "✓  {:<9} works ({fps:.0} fps test encode at 720p)",
                        enc.name()
                    )
                }
                Err(e) => format!(
                    "✗  {:<9} installed but FAILED: {}",
                    enc.name(),
                    format!("{e:#}").lines().next().unwrap_or("")
                ),
            }
        };
        println!("  {line}");
    }
    for g in &gpus {
        if !g.encoders().iter().any(|e| works.contains(e)) {
            needs.insert(Need::GpuEncoder(*g));
        }
    }
    if !works.iter().any(|e| e.is_software()) {
        needs.insert(Need::CpuEncoder);
    }
    if first_ok.is_none() {
        ok_video = false;
    }

    println!("\nKeyboard / mouse");
    match std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => println!("  ✓ /dev/uinput writable"),
        Err(e) => println!(
            "  ✗ /dev/uinput: {e} — the phone could watch but not type; see README \"Keyboard and mouse access\""
        ),
    }

    println!();
    match first_ok {
        Some(e) if ok_video => println!(
            "RESULT: video works, using {}{}",
            e.name(),
            if e.is_software() {
                " (on the CPU: fine for 720p/30, a GPU encoder is lighter)"
            } else {
                " (GPU)"
            }
        ),
        _ => println!("RESULT: video will NOT work yet"),
    }
    if !needs.is_empty() {
        let what = if ok_video {
            "Optional, to improve"
        } else {
            "To fix"
        };
        match install_command(distro, &needs) {
            Some(cmd) => {
                println!("{what}:\n  {cmd}");
                if needs.contains(&Need::GpuEncoder(Gpu::Nvidia)) {
                    println!(
                        "  (NVIDIA also needs the proprietary driver, nvidia or nvidia-open; nouveau can't encode)"
                    );
                }
                if distro == Distro::Fedora
                    && needs
                        .iter()
                        .any(|n| matches!(n, Need::GpuEncoder(Gpu::Amd)))
                {
                    println!(
                        "  (mesa-va-drivers-freeworld is in RPM Fusion: https://rpmfusion.org/Configuration)"
                    );
                }
                println!("  then run `dishus doctor` again.");
            }
            None => println!(
                "{what}: install your distro's GStreamer \"bad\" plugins (h264parse, VA-API/NVENC), \
                 its PipeWire GStreamer plugin and x264 (\"ugly\" plugins), then run `dishus doctor` again."
            ),
        }
    }
    if ok_video { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_families() {
        let arch = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n";
        assert_eq!(parse_os_release(arch), (Distro::Arch, "Arch Linux".into()));
        let endeavour = "PRETTY_NAME=\"EndeavourOS\"\nID=\"endeavouros\"\nID_LIKE=\"arch\"\n";
        assert_eq!(parse_os_release(endeavour).0, Distro::Arch);
        let cachy = "ID=cachyos\nID_LIKE=arch\n";
        assert_eq!(parse_os_release(cachy).0, Distro::Arch);
        let mint = "PRETTY_NAME=\"Linux Mint 22\"\nID=linuxmint\nID_LIKE=\"ubuntu debian\"\n";
        assert_eq!(
            parse_os_release(mint),
            (Distro::Debian, "Linux Mint 22".into())
        );
        let fedora = "ID=fedora\nPRETTY_NAME=\"Fedora Linux 43\"\n";
        assert_eq!(parse_os_release(fedora).0, Distro::Fedora);
        assert_eq!(parse_os_release("ID=nixos\n").0, Distro::Other);
        assert_eq!(parse_os_release("").0, Distro::Other);
    }

    #[test]
    fn vendor_ids() {
        assert_eq!(gpu_from_vendor("0x8086\n"), Some(Gpu::Intel));
        assert_eq!(gpu_from_vendor("0x1002"), Some(Gpu::Amd));
        assert_eq!(gpu_from_vendor("0x10de"), Some(Gpu::Nvidia));
        assert_eq!(gpu_from_vendor("0x1af4"), None); // virtio
    }

    #[test]
    fn arch_intel_fresh_install_gets_one_exact_command() {
        // The case that bit a friend: Arch, Intel iGPU, no encoder plugins at all.
        let needs = BTreeSet::from([Need::GpuEncoder(Gpu::Intel), Need::CpuEncoder]);
        assert_eq!(
            install_command(Distro::Arch, &needs).unwrap(),
            "sudo pacman -S --needed gst-plugin-va intel-media-driver gst-plugins-ugly"
        );
    }

    #[test]
    fn commands_dedupe_and_cover_every_family() {
        let needs = BTreeSet::from([Need::Parser, Need::GpuEncoder(Gpu::Nvidia)]);
        assert_eq!(
            install_command(Distro::Arch, &needs).unwrap(),
            "sudo pacman -S --needed gst-plugins-bad-libs"
        );
        let all = BTreeSet::from([
            Need::PipeWire,
            Need::Parser,
            Need::X11Capture,
            Need::GpuEncoder(Gpu::Nvidia),
            Need::GpuEncoder(Gpu::Amd),
            Need::GpuEncoder(Gpu::Intel),
            Need::CpuEncoder,
        ]);
        for d in [Distro::Arch, Distro::Debian, Distro::Fedora] {
            for n in &all {
                assert!(
                    !packages(d, *n).is_empty(),
                    "{d:?} has no package for {n:?}"
                );
            }
            assert!(install_command(d, &all).is_some());
        }
        assert_eq!(install_command(Distro::Other, &all), None);
        assert_eq!(install_command(Distro::Arch, &BTreeSet::new()), None);
    }
}
