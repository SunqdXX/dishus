//! UniFFI surface for the Android app. Deliberately coarse: Kotlin never sees keys, tokens or
//! raw protocol types — only records, an opaque [`Core`] and opaque [`RemoteSession`]s.
//!
//! Storage split: Rust seals/opens the vault (Argon2id + XChaCha20-Poly1305) and hands Kotlin an
//! opaque blob, which Kotlin wraps again with an Android Keystore key before writing it to disk.

use std::sync::{Arc, Mutex};

use tokio::runtime::Runtime;
use zeroize::Zeroize;

use dishus_proto::{ClientMsg, MouseButton, PairingInfo, Quality, ServerMsg, StreamParams};

use crate::{
    client::{Client, NetConfig, Session},
    vault::{self, KdfParams, PairedPc, VaultData},
};

/// `flat_error`: Kotlin sees each variant with `message` = the Display text below. (Non-flat
/// errors render as `msg=…`, and unit variants with an empty message, which the app showed raw.)
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum DishusError {
    #[error("locked")]
    Locked,
    #[error("wrong password or corrupted store")]
    WrongPassword,
    #[error("{msg}")]
    InvalidInput { msg: String },
    #[error("{msg}")]
    Network { msg: String },
    #[error("{msg}")]
    Rejected { msg: String },
    #[error("{msg}")]
    Internal { msg: String },
}

impl From<vault::VaultError> for DishusError {
    fn from(e: vault::VaultError) -> Self {
        match e {
            vault::VaultError::Decrypt => DishusError::WrongPassword,
            vault::VaultError::WeakPassword => DishusError::InvalidInput { msg: e.to_string() },
            other => DishusError::Internal {
                msg: other.to_string(),
            },
        }
    }
}

/// `ControlEvent::Closed` reason / `Rejected` message for a refusal; the app matches on these.
/// `LoginLimit:<seconds until the next try>`.
fn refusal_reason(r: crate::Refusal) -> String {
    match r {
        crate::Refusal::Lockdown => "Lockdown".into(),
        crate::Refusal::Expired => "Expired".into(),
        crate::Refusal::LoginLimit { retry_secs } => format!("LoginLimit:{retry_secs}"),
        crate::Refusal::LoginBlocked => "LoginBlocked".into(),
        crate::Refusal::NotAtLoginScreen => "NotAtLoginScreen".into(),
    }
}

fn net_err(e: anyhow::Error) -> DishusError {
    if let Some(r) = crate::Refusal::find(&e) {
        return DishusError::Rejected {
            msg: refusal_reason(r),
        };
    }
    DishusError::Network {
        msg: format!("{e:#}"),
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PcInfo {
    /// Hex EndpointId of the PC.
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl From<&PairedPc> for PcInfo {
    fn from(p: &PairedPc) -> Self {
        PcInfo {
            id: data_encoding_hex(&p.endpoint_id),
            name: p.name.clone(),
            expires_at: p.expires_at,
        }
    }
}

fn data_encoding_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Monitor {
    pub index: u8,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum StreamQuality {
    Auto,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamSettings {
    pub monitor: u8,
    /// 0 = adaptive (30 → 60)
    pub fps: u8,
    pub quality: StreamQuality,
    /// 0 = native
    pub max_height: u16,
}

impl From<&StreamSettings> for StreamParams {
    fn from(s: &StreamSettings) -> Self {
        StreamParams {
            monitor: s.monitor,
            fps: s.fps,
            quality: match s.quality {
                StreamQuality::Auto => Quality::Auto,
                StreamQuality::Low => Quality::Low,
                StreamQuality::Medium => Quality::Medium,
                StreamQuality::High => Quality::High,
            },
            max_height: s.max_height,
        }
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ControlEvent {
    Welcome {
        pc_name: String,
        monitors: Vec<Monitor>,
        expires_at: u64,
    },
    StreamInfo {
        monitor: u8,
        width: u32,
        height: u32,
        fps: u8,
        bitrate_kbps: u32,
        encoder: String,
    },
    Renewed {
        expires_at: u64,
    },
    Notice {
        message: String,
    },
    Closed {
        reason: String,
    },
    /// Audio datagrams follow (Opus).
    AudioStarted {
        sample_rate: u32,
        channels: u8,
    },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct VideoFrame {
    pub seq: u64,
    pub pts_us: u64,
    pub keyframe: bool,
    /// H.264 Annex-B access unit.
    pub data: Vec<u8>,
}

/// One Opus packet: 48 kHz stereo, 20 ms.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AudioPacket {
    /// +1 per packet the PC encoded; gaps are lost or dropped packets.
    pub seq: u32,
    /// Capture time, microseconds, on the same PC clock as `VideoFrame::pts_us`.
    pub pts_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct RelaySettings {
    /// Empty = iroh's public default relays.
    pub relays: Vec<String>,
    pub disable_relays: bool,
}

struct Unlocked {
    data: VaultData,
    password: zeroize::Zeroizing<String>,
}

#[derive(uniffi::Object)]
pub struct Core {
    rt: Runtime,
    unlocked: Mutex<Option<Unlocked>>,
    net: Mutex<NetConfig>,
    client: Mutex<Option<Arc<Client>>>,
}

impl Core {
    fn with_vault<T>(
        &self,
        f: impl FnOnce(&mut Unlocked) -> Result<T, DishusError>,
    ) -> Result<T, DishusError> {
        let mut g = self.unlocked.lock().map_err(|_| DishusError::Internal {
            msg: "poisoned".into(),
        })?;
        let u = g.as_mut().ok_or(DishusError::Locked)?;
        f(u)
    }

    async fn client(&self) -> Result<Arc<Client>, DishusError> {
        if let Some(c) = self.client.lock().expect("client lock").clone() {
            return Ok(c);
        }
        let key = self.with_vault(|u| Ok(zeroize::Zeroizing::new(u.data.secret_key)))?;
        let net = self.net.lock().expect("net lock").clone();
        let c = self
            .rt
            .spawn(async move { Client::new(&key, &net).await })
            .await
            .map_err(|e| DishusError::Internal { msg: e.to_string() })?
            .map_err(net_err)?;
        let c = Arc::new(c);
        *self.client.lock().expect("client lock") = Some(c.clone());
        Ok(c)
    }

    fn drop_client(&self) {
        if let Some(c) = self.client.lock().expect("client lock").take() {
            self.rt.spawn(async move { c.close().await });
        }
    }
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, DishusError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("dishus-core")
            .enable_all()
            .build()
            .map_err(|e| DishusError::Internal { msg: e.to_string() })?;
        Ok(Arc::new(Self {
            rt,
            unlocked: Mutex::new(None),
            net: Mutex::new(NetConfig::default()),
            client: Mutex::new(None),
        }))
    }

    /// Create a new identity protected by `password`. Returns the sealed blob to persist.
    pub fn create_vault(&self, password: String) -> Result<Vec<u8>, DishusError> {
        let data = vault::new_identity()?;
        let blob = vault::seal(&password, &data, KdfParams::default())?;
        *self.unlocked.lock().expect("vault lock") = Some(Unlocked {
            data,
            password: password.into(),
        });
        Ok(blob)
    }

    /// Decrypt a previously sealed blob. Slow on purpose (Argon2id).
    pub fn unlock(&self, password: String, blob: Vec<u8>) -> Result<(), DishusError> {
        let data = vault::open(&password, &blob)?;
        *self.unlocked.lock().expect("vault lock") = Some(Unlocked {
            data,
            password: password.into(),
        });
        Ok(())
    }

    /// Forget all secrets in memory and close the network endpoint.
    pub fn lock(&self) {
        self.drop_client();
        // VaultData and the password zeroize on drop.
        self.unlocked.lock().expect("vault lock").take();
    }

    pub fn is_unlocked(&self) -> bool {
        self.unlocked.lock().expect("vault lock").is_some()
    }

    /// Re-seal the current state (after pairing / forgetting a PC). Persist the result.
    pub fn export_vault(&self) -> Result<Vec<u8>, DishusError> {
        self.with_vault(|u| Ok(vault::seal(&u.password, &u.data, KdfParams::default())?))
    }

    pub fn change_password(&self, old: String, new: String) -> Result<Vec<u8>, DishusError> {
        self.with_vault(|u| {
            if *u.password != old {
                return Err(DishusError::WrongPassword);
            }
            let blob = vault::seal(&new, &u.data, KdfParams::default())?;
            u.password = new.into();
            Ok(blob)
        })
    }

    pub fn paired_pcs(&self) -> Result<Vec<PcInfo>, DishusError> {
        self.with_vault(|u| Ok(u.data.pcs.iter().map(PcInfo::from).collect()))
    }

    pub fn forget_pc(&self, id: String) -> Result<(), DishusError> {
        self.with_vault(|u| {
            u.data
                .pcs
                .retain(|p| data_encoding_hex(&p.endpoint_id) != id);
            Ok(())
        })
    }

    /// Call on every Android connectivity change (default network available / lost / changed).
    /// No-op while locked or before the first connection.
    pub fn network_changed(&self) {
        if let Some(c) = self.client.lock().expect("client lock").clone() {
            self.rt.spawn(async move { c.network_change().await });
        }
    }

    pub fn set_relays(&self, settings: RelaySettings) -> Result<(), DishusError> {
        for r in &settings.relays {
            r.parse::<iroh::RelayUrl>()
                .map_err(|_| DishusError::InvalidInput {
                    msg: format!("bad relay URL: {r}"),
                })?;
        }
        *self.net.lock().expect("net lock") = NetConfig {
            relays: settings.relays,
            disable_relays: settings.disable_relays,
            local_test: false,
        };
        self.drop_client(); // takes effect on the next connection
        Ok(())
    }

    pub fn relays(&self) -> RelaySettings {
        let n = self.net.lock().expect("net lock");
        RelaySettings {
            relays: n.relays.clone(),
            disable_relays: n.disable_relays,
        }
    }

    /// This phone's public id (shown in the UI so the user can match it with `dishus list`).
    pub fn phone_id(&self) -> Result<String, DishusError> {
        self.with_vault(|u| {
            Ok(iroh::SecretKey::from_bytes(&u.data.secret_key)
                .public()
                .to_string())
        })
    }

    /// Redeem a `dishus://pair?...` link (from the QR code). Call `export_vault` afterwards.
    pub async fn pair(&self, link: String, device_name: String) -> Result<PcInfo, DishusError> {
        let info = PairingInfo::from_uri(&link)
            .map_err(|e| DishusError::InvalidInput { msg: e.to_string() })?;
        let client = self.client().await?;
        let pc = self
            .rt
            .spawn(async move { client.pair(&info, &device_name).await })
            .await
            .map_err(|e| DishusError::Internal { msg: e.to_string() })?
            .map_err(|e| DishusError::Rejected {
                msg: format!("{e:#}"),
            })?;
        let out = PcInfo::from(&pc);
        self.with_vault(|u| {
            u.data.pcs.retain(|p| p.endpoint_id != pc.endpoint_id);
            u.data.pcs.push(pc);
            Ok(())
        })?;
        Ok(out)
    }

    pub async fn connect(
        &self,
        pc_id: String,
        device_name: String,
        settings: StreamSettings,
    ) -> Result<Arc<RemoteSession>, DishusError> {
        let pc = self.with_vault(|u| {
            u.data
                .pcs
                .iter()
                .find(|p| data_encoding_hex(&p.endpoint_id) == pc_id)
                .cloned()
                .ok_or(DishusError::InvalidInput {
                    msg: "unknown PC".into(),
                })
        })?;
        let client = self.client().await?;
        let params = StreamParams::from(&settings);
        let session = self
            .rt
            .spawn(async move { client.connect(&pc, &device_name, params).await })
            .await
            .map_err(|e| DishusError::Internal { msg: e.to_string() })?
            .map_err(net_err)?;
        Ok(Arc::new(RemoteSession {
            session: Arc::new(session),
            handle: self.rt.handle().clone(),
        }))
    }

    /// Update the stored expiry after an in-session renewal.
    pub fn record_renewal(&self, pc_id: String, expires_at: u64) -> Result<(), DishusError> {
        self.with_vault(|u| {
            for p in u
                .data
                .pcs
                .iter_mut()
                .filter(|p| data_encoding_hex(&p.endpoint_id) == pc_id)
            {
                p.expires_at = expires_at;
            }
            Ok(())
        })
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Ok(mut g) = self.unlocked.lock()
            && let Some(mut u) = g.take()
        {
            u.data.secret_key.zeroize();
        }
    }
}

#[derive(uniffi::Object)]
pub struct RemoteSession {
    session: Arc<Session>,
    handle: tokio::runtime::Handle,
}

impl RemoteSession {
    fn send(&self, m: ClientMsg) -> Result<(), DishusError> {
        self.session.send(m).map_err(|e| DishusError::InvalidInput {
            msg: format!("{e:#}"),
        })
    }
}

#[uniffi::export]
impl RemoteSession {
    /// Linux evdev keycode.
    pub fn key(&self, code: u16, down: bool) -> Result<(), DishusError> {
        self.send(ClientMsg::Key { code, down })
    }
    pub fn mouse_move(&self, dx: i16, dy: i16) -> Result<(), DishusError> {
        self.send(ClientMsg::MouseMove { dx, dy })
    }
    /// 0 = left, 1 = right, 2 = middle
    pub fn mouse_button(&self, button: u8, down: bool) -> Result<(), DishusError> {
        let button = match button {
            0 => MouseButton::Left,
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            _ => {
                return Err(DishusError::InvalidInput {
                    msg: "button".into(),
                });
            }
        };
        self.send(ClientMsg::MouseButton { button, down })
    }
    /// 120 = one wheel notch; positive dy scrolls down.
    pub fn scroll(&self, dx: i16, dy: i16) -> Result<(), DishusError> {
        self.send(ClientMsg::Scroll { dx, dy })
    }
    pub fn release_all(&self) -> Result<(), DishusError> {
        self.send(ClientMsg::ReleaseAll)
    }
    pub fn request_keyframe(&self) -> Result<(), DishusError> {
        self.send(ClientMsg::RequestKeyframe)
    }
    pub fn set_stream(&self, settings: StreamSettings) -> Result<(), DishusError> {
        self.send(ClientMsg::SetStream(StreamParams::from(&settings)))
    }
    pub fn renew(&self, days: u16) -> Result<(), DishusError> {
        self.send(ClientMsg::RenewPairing { days })
    }
    /// The PC's DiShus can stream audio (it speaks session/2).
    pub fn audio_supported(&self) -> bool {
        self.session.audio_supported()
    }
    /// Start or stop PC audio. Errors on a PC too old for audio.
    pub fn set_audio(&self, on: bool) -> Result<(), DishusError> {
        self.send(ClientMsg::SetAudio { on })
    }

    pub async fn next_control(&self) -> Option<ControlEvent> {
        let s = self.session.clone();
        let msg = self
            .handle
            .spawn(async move { s.next_control().await })
            .await
            .ok()
            .flatten();
        Some(match msg {
            None => ControlEvent::Closed {
                reason: match self.session.refusal() {
                    Some(r) => refusal_reason(r),
                    None if self.session.timed_out() => "PC not responding".into(),
                    None => "connection closed".into(),
                },
            },
            Some(ServerMsg::Welcome {
                pc_name,
                monitors,
                expires_at,
                ..
            }) => ControlEvent::Welcome {
                pc_name,
                monitors: monitors
                    .into_iter()
                    .map(|m| Monitor {
                        index: m.index,
                        name: m.name,
                        width: m.width,
                        height: m.height,
                    })
                    .collect(),
                expires_at,
            },
            Some(ServerMsg::StreamInfo {
                monitor,
                width,
                height,
                fps,
                bitrate_kbps,
                encoder,
            }) => ControlEvent::StreamInfo {
                monitor,
                width,
                height,
                fps,
                bitrate_kbps,
                encoder,
            },
            Some(ServerMsg::Renewed { expires_at }) => ControlEvent::Renewed { expires_at },
            Some(ServerMsg::Notice { message, .. }) => ControlEvent::Notice { message },
            Some(ServerMsg::Bye { reason }) => ControlEvent::Closed {
                reason: format!("{reason:?}"),
            },
            Some(ServerMsg::AudioStarted {
                sample_rate,
                channels,
            }) => ControlEvent::AudioStarted {
                sample_rate,
                channels,
            },
            Some(ServerMsg::Pong { .. }) => return None, // never forwarded by Session
        })
    }

    pub async fn next_video(&self) -> Option<VideoFrame> {
        let s = self.session.clone();
        let p = self
            .handle
            .spawn(async move { s.next_video().await })
            .await
            .ok()
            .flatten()?;
        Some(VideoFrame {
            seq: p.header.seq,
            pts_us: p.header.pts_us,
            keyframe: p.header.is_keyframe(),
            data: p.data.to_vec(),
        })
    }

    /// Next audio packet; None once the session is closed.
    pub async fn next_audio(&self) -> Option<AudioPacket> {
        let s = self.session.clone();
        let p = self
            .handle
            .spawn(async move { s.next_audio().await })
            .await
            .ok()
            .flatten()?;
        Some(AudioPacket {
            seq: p.header.seq,
            pts_us: p.header.pts_us,
            data: p.data.to_vec(),
        })
    }

    pub fn rtt_ms(&self) -> u64 {
        self.session.rtt_ms()
    }

    pub fn path(&self) -> String {
        self.session.path()
    }

    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }

    /// Named `disconnect` because UniFFI objects already get `close()` (AutoCloseable = destroy).
    pub async fn disconnect(&self) {
        let s = self.session.clone();
        let _ = self.handle.spawn(async move { s.close().await }).await;
    }
}
