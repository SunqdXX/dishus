//! xdg-desktop-portal ScreenCast → PipeWire. The first (interactive) run stores a restore token
//! (`persist_mode = until revoked`); later runs pass it back so no dialog is shown.

use std::os::fd::OwnedFd;

use anyhow::{Context, anyhow};
use ashpd::desktop::{
    PersistMode, Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};

use dishus_proto::MonitorInfo;

use super::{WorkerOptions, pipeline::Source};
use crate::paths::{read_private, write_private};

pub enum PortalError {
    /// No portal / no ScreenCast interface / no monitor support: try another backend.
    Unavailable(String),
    Other(anyhow::Error),
}

impl From<ashpd::Error> for PortalError {
    fn from(e: ashpd::Error) -> Self {
        PortalError::Other(anyhow!("portal: {e}"))
    }
}

struct PwStream {
    node: u32,
    info: MonitorInfo,
}

pub struct PortalSession {
    /// Whether the portal returned a restore token and it was saved (so later sessions can
    /// start capture without anyone at the PC).
    pub token_stored: bool,
    session: Option<Session<Screencast>>,
    fd: OwnedFd,
    streams: Vec<PwStream>,
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        if let Some(s) = self.session.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move {
                let _ = s.close().await;
            });
        }
    }
}

/// `XDG_CURRENT_DESKTOP` as the systemd user manager sees it *now*. The daemon starts at login,
/// often before the desktop exports its environment, so its own copy is usually empty.
async fn current_desktop() -> Option<String> {
    async fn from_manager() -> Option<String> {
        let conn = zbus::Connection::session().await.ok()?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.systemd1.Manager", "Environment"),
            )
            .await
            .ok()?;
        let v: zbus::OwnedVariant = reply.body().deserialize().ok()?;
        let env: Vec<String> = v.try_into().ok()?;
        env.into_iter()
            .find_map(|e| e.strip_prefix("XDG_CURRENT_DESKTOP=").map(str::to_owned))
    }
    let raw = match tokio::time::timeout(std::time::Duration::from_secs(2), from_manager()).await {
        Ok(Some(d)) => d,
        _ => std::env::var("XDG_CURRENT_DESKTOP").ok()?,
    };
    desktop_key(&raw)
}

/// File-name-safe desktop name: `KDE`, `Hyprland`, `ubuntu_GNOME`…
fn desktop_key(raw: &str) -> Option<String> {
    let k: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    (!k.trim_matches('_').is_empty()).then_some(k)
}

/// One restore token per desktop: a token is only valid for the portal backend that issued it,
/// so with a single file, using KDE once made the next Hyprland session prompt again.
fn token_file_for(base: &std::path::Path, desktop: Option<&str>) -> std::path::PathBuf {
    match desktop {
        Some(d) => {
            let mut name = base.file_name().unwrap_or_default().to_os_string();
            name.push(format!("-{d}"));
            base.with_file_name(name)
        }
        None => base.to_path_buf(),
    }
}

fn read_token(path: &std::path::Path) -> Result<Option<String>, PortalError> {
    Ok(read_private(path)
        .map_err(PortalError::Other)?
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && s.len() < 512))
}

/// While it exists, this file tells a cooperating screen-share picker (see
/// `packaging/hyprland/dishus-share-picker`) that the request comes from DiShus for a paired
/// phone, so it may answer "whole monitor" without showing a dialog nobody is there to click.
/// One-shot (the picker deletes it) and removed again on drop.
struct AutoshareSignal(Option<std::path::PathBuf>);

impl AutoshareSignal {
    fn raise() -> Self {
        let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) else {
            return Self(None);
        };
        let path = std::path::Path::new(&rt).join("dishus").join("autoshare");
        match write_private(&path, format!("{}\n", std::process::id()).as_bytes()) {
            Ok(()) => Self(Some(path)),
            Err(e) => {
                tracing::debug!("autoshare signal not written: {e:#}");
                Self(None)
            }
        }
    }
}

impl Drop for AutoshareSignal {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub async fn open(opts: &WorkerOptions, interactive: bool) -> Result<PortalSession, PortalError> {
    let desktop = current_desktop().await;
    let token_file = token_file_for(&opts.token_file, desktop.as_deref());
    // Fall back to the pre-0.1.1 single token file (it may still be this desktop's).
    let token = match read_token(&token_file)? {
        Some(t) => Some(t),
        None if token_file != opts.token_file => read_token(&opts.token_file)?,
        None => None,
    };
    tracing::debug!(
        "portal: desktop {:?}, token file {} ({})",
        desktop,
        token_file.display(),
        if token.is_some() {
            "token found"
        } else {
            "no token"
        }
    );

    let proxy = Screencast::new()
        .await
        .map_err(|e| PortalError::Unavailable(e.to_string()))?;
    let types = proxy
        .available_source_types()
        .await
        .map_err(|e| PortalError::Unavailable(e.to_string()))?;
    if !types.contains(SourceType::MONITOR) {
        return Err(PortalError::Unavailable(
            "portal cannot capture monitors".into(),
        ));
    }
    if !interactive && token.is_none() {
        return Err(PortalError::Other(anyhow!(
            "no stored screen-capture permission; at the PC run `dishus setup-capture` (or `dishus pair`)"
        )));
    }
    let cursor = match proxy.available_cursor_modes().await {
        Ok(m) if opts.show_cursor && m.contains(CursorMode::EMBEDDED) => CursorMode::EMBEDDED,
        Ok(m) if m.contains(CursorMode::HIDDEN) => CursorMode::HIDDEN,
        _ => CursorMode::EMBEDDED,
    };

    let session = proxy.create_session(Default::default()).await?;
    // Only for unattended (phone-initiated) opens; `dishus setup-capture` shows the real picker.
    let _autoshare = (!interactive).then(AutoshareSignal::raise);
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(SourceType::MONITOR)
                .set_multiple(true)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?
        .response()?;
    let resp = proxy
        .start(&session, None, Default::default())
        .await?
        .response()?;
    let token_stored = match resp.restore_token() {
        Some(t) => match write_private(&token_file, t.as_bytes()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("could not store portal restore token: {e:#}");
                false
            }
        },
        None => {
            tracing::warn!(
                "portal returned no restore token; remote sessions will need someone at the PC to \
                 approve capture (enable the picker's remember / allow-restore-token option)"
            );
            false
        }
    };
    let mut streams = Vec::new();
    for (i, s) in resp
        .streams()
        .iter()
        .enumerate()
        .take(dishus_proto::MAX_MONITORS)
    {
        let (w, h) = s
            .size()
            .map(|(w, h)| (w.max(0) as u32, h.max(0) as u32))
            .unwrap_or((0, 0));
        let pos = s
            .position()
            .map(|(x, y)| format!(" @{x},{y}"))
            .unwrap_or_default();
        streams.push(PwStream {
            node: s.pipe_wire_node_id(),
            info: MonitorInfo {
                index: i as u8,
                name: format!("Monitor {} {w}x{h}{pos}", i + 1),
                width: w,
                height: h,
            },
        });
    }
    if streams.is_empty() {
        let _ = session.close().await;
        return Err(PortalError::Other(anyhow!("portal granted no monitors")));
    }
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("opening PipeWire remote")
        .map_err(PortalError::Other)?;
    Ok(PortalSession {
        token_stored,
        session: Some(session),
        fd,
        streams,
    })
}

impl PortalSession {
    pub fn monitors(&self) -> Vec<MonitorInfo> {
        self.streams.iter().map(|s| s.info.clone()).collect()
    }

    /// PipeWire source for monitor `idx`, plus its height (for "don't upscale").
    pub fn source(&self, idx: u8) -> anyhow::Result<(Source, Option<(u32, u32)>)> {
        let s = self
            .streams
            .get(usize::from(idx))
            .context("no such monitor")?;
        Ok((
            Source::PipeWire {
                fd: self.fd.try_clone()?,
                node: s.node,
            },
            (s.info.width > 0 && s.info.height > 0).then_some((s.info.width, s.info.height)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_is_per_desktop() {
        let base = std::path::Path::new("/s/dishus/portal-restore-token");
        assert_eq!(
            token_file_for(base, Some("Hyprland")),
            std::path::Path::new("/s/dishus/portal-restore-token-Hyprland")
        );
        assert_eq!(token_file_for(base, None), base);
        assert_eq!(desktop_key("ubuntu:GNOME").as_deref(), Some("ubuntu_GNOME"));
        assert_eq!(desktop_key("KDE").as_deref(), Some("KDE"));
        assert_eq!(desktop_key("../../x").as_deref(), Some("______x"));
        assert_eq!(desktop_key(":::"), None);
        assert_eq!(desktop_key(""), None);
    }
}
