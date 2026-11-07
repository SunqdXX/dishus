//! Desktop notifications (org.freedesktop.Notifications over D-Bus). Fire-and-forget: a
//! missing notification daemon must never break a session. Everything is also logged.

use std::collections::HashMap;

use tokio::sync::mpsc;

#[derive(Clone)]
pub struct Notifier {
    tx: Option<mpsc::Sender<(String, String)>>,
}

impl Notifier {
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    /// Spawn the background sender. Must be called inside a tokio runtime.
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::channel::<(String, String)>(32);
        tokio::spawn(async move {
            let mut conn: Option<zbus::Connection> = None;
            while let Some((summary, body)) = rx.recv().await {
                if conn.is_none() {
                    match zbus::Connection::session().await {
                        Ok(c) => conn = Some(c),
                        Err(e) => {
                            tracing::debug!("no session bus for notifications: {e}");
                            continue;
                        }
                    }
                }
                let Some(c) = conn.as_ref() else { continue };
                let hints: HashMap<&str, zbus::Variant<'_>> =
                    HashMap::from([("urgency", zbus::Variant::U8(2))]);
                let res = c
                    .call_method(
                        Some("org.freedesktop.Notifications"),
                        "/org/freedesktop/Notifications",
                        Some("org.freedesktop.Notifications"),
                        "Notify",
                        &("DiShus", 0u32, "preferences-desktop-remote-desktop", &summary, &body, Vec::<&str>::new(), hints, 8000i32),
                    )
                    .await;
                if let Err(e) = res {
                    tracing::debug!("notification failed: {e}");
                    conn = None; // reconnect next time
                }
            }
        });
        Self { tx: Some(tx) }
    }

    pub fn notify(&self, summary: &str, body: &str) {
        tracing::info!("{summary}: {body}");
        if let Some(tx) = &self.tx {
            let _ = tx.try_send((summary.to_owned(), body.to_owned()));
        }
    }
}
