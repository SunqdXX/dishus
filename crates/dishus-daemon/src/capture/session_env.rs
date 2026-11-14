//! The X11 session's `DISPLAY` / `XAUTHORITY`, read from the systemd user manager each time a
//! capture worker starts.
//!
//! The service starts with the user manager (`default.target`), often before the X session has
//! exported its display to systemd. Its own environment then never has `DISPLAY`, and after a
//! log out / log in it can hold a stale `XAUTHORITY`. The manager's environment is the current
//! one, so the worker gets that instead. Only for X11 sessions: when the manager knows a Wayland
//! display nothing is changed (the portal path doesn't use these).

use std::time::Duration;

/// What to set on the capture worker for an X11 session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X11Env {
    pub display: String,
    pub xauthority: Option<String>,
}

/// From the manager's `KEY=value` list: the X11 display, if this is an X11 session.
pub fn pick_x11(vars: &[String]) -> Option<X11Env> {
    let get = |k: &str| {
        vars.iter()
            .find_map(|v| v.strip_prefix(k)?.strip_prefix('='))
            .filter(|v| !v.is_empty())
    };
    if get("WAYLAND_DISPLAY").is_some() {
        return None;
    }
    if let Some(t) = get("XDG_SESSION_TYPE")
        && t != "x11"
    {
        return None;
    }
    Some(X11Env {
        display: get("DISPLAY")?.to_string(),
        xauthority: get("XAUTHORITY").map(str::to_string),
    })
}

/// Ask the systemd user manager (2 s at most). None when it can't be reached or the session
/// isn't X11.
pub async fn x11_from_manager() -> Option<X11Env> {
    let vars = tokio::time::timeout(Duration::from_secs(2), async {
        let conn = zbus::Connection::session().await.ok()?;
        let p = zbus::Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .ok()?;
        p.get_property::<Vec<String>>("Environment").await.ok()
    })
    .await
    .ok()
    .flatten()?;
    pick_x11(&vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn x11_session_gives_display_and_cookie() {
        let e = pick_x11(&v(&[
            "HOME=/home/u",
            "DISPLAY=:0",
            "XAUTHORITY=/tmp/xauth_abc",
            "XDG_SESSION_TYPE=x11",
        ]));
        assert_eq!(
            e,
            Some(X11Env {
                display: ":0".into(),
                xauthority: Some("/tmp/xauth_abc".into())
            })
        );
        // no session type exported: still X11 if there is a display and no Wayland
        assert_eq!(pick_x11(&v(&["DISPLAY=:1"])).unwrap().xauthority, None);
    }

    #[test]
    fn wayland_or_no_display_changes_nothing() {
        // Hyprland: XWayland's DISPLAY is there too, but this is a Wayland session
        assert_eq!(
            pick_x11(&v(&["DISPLAY=:0", "WAYLAND_DISPLAY=wayland-1"])),
            None
        );
        assert_eq!(
            pick_x11(&v(&["DISPLAY=:0", "XDG_SESSION_TYPE=wayland"])),
            None
        );
        assert_eq!(pick_x11(&v(&["HOME=/home/u", "DISPLAY="])), None);
        // a key that only starts the same
        assert_eq!(pick_x11(&v(&["DISPLAYX=:0"])), None);
    }
}
