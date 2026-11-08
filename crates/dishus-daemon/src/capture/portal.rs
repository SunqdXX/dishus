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

pub async fn open(opts: &WorkerOptions, interactive: bool) -> Result<PortalSession, PortalError> {
    let token = read_private(&opts.token_file)
        .map_err(PortalError::Other)?
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && s.len() < 512);

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
        Some(t) => match write_private(&opts.token_file, t.as_bytes()) {
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
