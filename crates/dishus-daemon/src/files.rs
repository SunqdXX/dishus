//! Receiving files from the phone (session/3). The rules:
//!
//! * only into one folder (`Downloads/DiShus`), only when `files.enabled` (off by default);
//! * the phone's name is cleaned up ([`clean_name`]) the same way on every OS;
//! * nothing is ever overwritten: the data goes to a new `.part` file, which is moved to its
//!   final name with a no-replace move; a taken name becomes `name (1).ext`, `name (2).ext`...;
//! * a transfer that doesn't finish leaves nothing behind (the `.part` is deleted);
//! * the PC never opens a received file; on Linux it is not executable, on Windows it gets the
//!   "came from the internet" mark, like a browser download.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::{Connection, ReadError, RecvStream, SendStream};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::timeout,
};

use dishus_proto::{
    files::{
        CANCELLED, FileAnswer, FileOffer, FileRefusal, FileResult, MAX_FILE_FRAME, MAX_SAVED_NAME,
    },
    read_frame, write_frame,
};

use crate::state::AppState;

/// Keep at least this much disk free after a file.
pub const SPARE_BYTES: u64 = 1 << 30;
/// Longest name we save (bytes), leaving room for " (123)" and ".part".
pub const MAX_NAME_BYTES: usize = 200;
const _: () = assert!(MAX_NAME_BYTES + 16 <= MAX_SAVED_NAME);
/// Give up on a free name after this many tries.
const MAX_NUMBER: u32 = 9999;

/// Characters left out of names: control characters, what Windows forbids, and invisible
/// characters that can disguise a name (a right-to-left override shows "evil\u{202E}gpj.exe" as
/// "evilexe.jpg").
fn unwanted(c: char) -> bool {
    c.is_control()
        || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\')
        || matches!(c,
            '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Windows device names; "CON.txt" opens the console there. Checked on every OS so a name
/// that is fine here stays fine when copied to Windows.
fn is_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end();
    let upper = base.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.chars().count() == 4
        && upper[3..]
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³')))
}

fn trim_edges(s: &str) -> &str {
    s.trim_matches(|c: char| c == '.' || c.is_whitespace())
}

/// Largest prefix of `s` that fits in `max` bytes.
fn cut(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// "stem", ".ext" (the extension is from the last dot; a leading dot is no extension).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Make a phone-supplied name safe to create on Linux and Windows.
pub fn clean_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let kept: String = last.chars().filter(|c| !unwanted(*c)).collect();
    let mut name = trim_edges(&kept).to_owned();
    if is_device_name(&name) {
        name.insert(0, '_');
    }
    if name.len() > MAX_NAME_BYTES {
        let (stem, ext) = split_ext(&name);
        name = if ext.len() <= 16 {
            let stem = trim_edges(cut(stem, MAX_NAME_BYTES - ext.len()));
            format!("{stem}{ext}")
        } else {
            trim_edges(cut(&name, MAX_NAME_BYTES)).to_owned()
        };
    }
    if name.is_empty() {
        name = "file".into();
    }
    name
}

/// `n` = 0: the name itself; otherwise "stem (n).ext".
pub fn numbered(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    let (stem, ext) = split_ext(name);
    format!("{stem} ({n}){ext}")
}

/// May a file of `size` bytes come in? `free`: free bytes where it would go.
pub fn admit(enabled: bool, max_bytes: u64, size: u64, free: u64) -> Result<(), FileRefusal> {
    if !enabled {
        Err(FileRefusal::Disabled)
    } else if size > max_bytes {
        Err(FileRefusal::TooBig { max_bytes })
    } else if free < size.saturating_add(SPARE_BYTES) {
        Err(FileRefusal::NoSpace)
    } else {
        Ok(())
    }
}

/// Free bytes for an unprivileged user on the disk holding `dir` (or its nearest existing
/// parent: the DiShus folder is only created with the first file).
pub fn free_space(dir: &Path) -> io::Result<u64> {
    let mut p = dir;
    while !p.exists() {
        p = p
            .parent()
            .ok_or_else(|| io::Error::other("no existing parent"))?;
    }
    sys::free_space(p)
}

/// A file being received. Dropped without [`Incoming::commit`]: its `.part` is deleted.
pub struct Incoming {
    dir: PathBuf,
    name: String,
    part: PathBuf,
    file: Option<File>,
}

impl Incoming {
    /// Create `dir` if needed and a new `.part` file for `name` (already cleaned).
    pub fn create(dir: &Path, name: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        for n in 0..=MAX_NUMBER {
            let candidate = numbered(name, n);
            if dir.join(&candidate).exists() {
                continue; // aim for a final name that is free now
            }
            let part = dir.join(format!("{candidate}.part"));
            match sys::create_new(&part) {
                Ok(file) => {
                    return Ok(Self {
                        dir: dir.to_owned(),
                        name: name.to_owned(),
                        part,
                        file: Some(file),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("no free file name"))
    }

    /// Where the data goes until it is complete.
    pub fn part_path(&self) -> &Path {
        &self.part
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("already closed"))?
            .write_all(data)
    }

    /// Flush and move into place under a free name. Returns the name used.
    pub fn commit(mut self) -> io::Result<String> {
        if let Some(f) = self.file.take() {
            f.sync_all()?;
        }
        for n in 0..=MAX_NUMBER {
            let candidate = numbered(&self.name, n);
            match sys::move_no_replace(&self.part, &self.dir.join(&candidate)) {
                Ok(()) => {
                    self.part = PathBuf::new(); // nothing left to clean up
                    return Ok(candidate);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("no free file name"))
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.file.take();
        if !self.part.as_os_str().is_empty()
            && let Err(e) = fs::remove_file(&self.part)
        {
            tracing::warn!("files: could not delete {}: {e}", self.part.display());
        }
    }
}

// ---------- the network side ----------

/// A phone that sends nothing for this long while a file is coming in is given up on.
const STALL: Duration = Duration::from_secs(30);
const OFFER_TIMEOUT: Duration = Duration::from_secs(10);
/// Chunks queued for the disk thread (64 KB each at most).
const DISK_QUEUE: usize = 16;

/// Serve file streams on a session/3 connection until it closes. Aborting this task (session
/// end, revoke, `dishus kill`) drops every transfer, which deletes its `.part`.
pub async fn serve(conn: Connection, state: Arc<AppState>, who: String) {
    let busy = Arc::new(AtomicBool::new(false));
    let mut running = JoinSet::new();
    loop {
        tokio::select! {
            r = conn.accept_bi() => {
                let Ok((send, recv)) = r else { break };
                let (state, busy, who) = (state.clone(), busy.clone(), who.clone());
                running.spawn(async move {
                    if let Err(e) = receive(&state, send, recv, &busy, &who).await {
                        tracing::warn!("files: from {who}: {e:#}");
                    }
                });
            }
            Some(_) = running.join_next(), if !running.is_empty() => {}
        }
    }
}

/// Clears the busy flag however a transfer ends.
struct Busy(Arc<AtomicBool>);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn answer(send: &mut SendStream, a: FileAnswer) -> Result<()> {
    write_frame(send, &a, MAX_FILE_FRAME).await?;
    Ok(())
}

async fn receive(
    state: &AppState,
    mut send: SendStream,
    mut recv: RecvStream,
    busy: &Arc<AtomicBool>,
    who: &str,
) -> Result<()> {
    let offer: FileOffer = timeout(OFFER_TIMEOUT, read_frame(&mut recv, MAX_FILE_FRAME))
        .await
        .context("no file offer")??;
    let cfg = &state.config.files;
    let dir = state.paths.files_dir.clone();
    let refuse = |r: FileRefusal| {
        tracing::info!(
            "files: refused a file from {who} ({} bytes): {r}",
            offer.size
        );
        FileAnswer::Refuse(r)
    };
    if !cfg.enabled {
        return answer(&mut send, refuse(FileRefusal::Disabled)).await;
    }
    if busy.swap(true, Ordering::SeqCst) {
        return answer(&mut send, refuse(FileRefusal::Busy)).await;
    }
    let busy = Busy(busy.clone());
    let free = free_space(&dir).unwrap_or(0);
    if let Err(r) = admit(true, cfg.max_bytes(), offer.size, free) {
        return answer(&mut send, refuse(r)).await;
    }
    let name = clean_name(&offer.name);
    let (disk, created, done) = spawn_disk_writer(dir, name.clone());
    if let Err(e) = created.await.context("disk writer")? {
        tracing::warn!("files: could not create {name}: {e}");
        return answer(&mut send, refuse(FileRefusal::Failed)).await;
    }
    answer(&mut send, FileAnswer::Accept).await?;
    tracing::info!("files: receiving {name} ({} bytes) from {who}", offer.size);

    let mut got = 0u64;
    let complete = loop {
        let chunk = match timeout(STALL, recv.read_chunk(64 * 1024)).await {
            Err(_) => break Err(anyhow::anyhow!("the phone stopped sending")),
            Ok(Err(ReadError::Reset(code))) if code.into_inner() == u64::from(CANCELLED) => {
                tracing::info!("files: {who} cancelled {name} after {got} bytes");
                return Ok(()); // dropping `disk` deletes the .part
            }
            Ok(Err(e)) => break Err(e.into()),
            Ok(Ok(None)) if got == offer.size => break Ok(()),
            Ok(Ok(None)) => {
                break Err(anyhow::anyhow!("ended after {got} of {} bytes", offer.size));
            }
            Ok(Ok(Some(c))) => c,
        };
        got += chunk.len() as u64;
        if got > offer.size {
            let _ = recv.stop(0u32.into());
            break Err(anyhow::anyhow!(
                "more than the {} bytes announced",
                offer.size
            ));
        }
        if disk.send(DiskMsg::Data(chunk)).await.is_err() {
            break Err(anyhow::anyhow!("writing failed"));
        }
    };
    let result = match complete {
        Ok(()) => {
            let _ = disk.send(DiskMsg::Commit).await;
            drop(disk);
            done.await
                .unwrap_or_else(|_| Err(io::Error::other("disk writer gone")))
                .map_err(anyhow::Error::from)
        }
        Err(e) => Err(e),
    };
    // the next file may come as soon as the phone hears about this one
    drop(busy);
    match result {
        Ok(saved) => {
            state.files_received.fetch_add(1, Ordering::Relaxed);
            tracing::info!("files: saved {saved} ({got} bytes) from {who}");
            state.notifier.notify(
                "DiShus: file received",
                &format!("{saved} from {who}, in Downloads/DiShus"),
            );
            write_frame(
                &mut send,
                &FileResult::Saved { name: saved },
                MAX_FILE_FRAME,
            )
            .await?;
            send.finish()?;
            let _ = timeout(Duration::from_secs(2), send.stopped()).await;
            Ok(())
        }
        Err(e) => {
            let _ = write_frame(&mut send, &FileResult::Failed, MAX_FILE_FRAME).await;
            let _ = send.finish();
            let _ = timeout(Duration::from_secs(2), send.stopped()).await;
            Err(e.context(format!("{name} not saved")))
        }
    }
}

enum DiskMsg {
    Data(Bytes),
    /// Everything arrived: move the file into place.
    Commit,
}

/// The file is written on a blocking thread. It is committed only after an explicit
/// [`DiskMsg::Commit`]; if the sender goes away first (cancel, error, aborted session), the
/// `.part` is deleted.
fn spawn_disk_writer(
    dir: PathBuf,
    name: String,
) -> (
    mpsc::Sender<DiskMsg>,
    oneshot::Receiver<io::Result<()>>,
    oneshot::Receiver<io::Result<String>>,
) {
    let (tx, mut rx) = mpsc::channel::<DiskMsg>(DISK_QUEUE);
    let (created_tx, created_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let mut file = match Incoming::create(&dir, &name) {
            Ok(f) => {
                let _ = created_tx.send(Ok(()));
                f
            }
            Err(e) => {
                let _ = created_tx.send(Err(e));
                return;
            }
        };
        while let Some(m) = rx.blocking_recv() {
            match m {
                DiskMsg::Data(b) => {
                    if let Err(e) = file.write(&b) {
                        let _ = done_tx.send(Err(e));
                        return;
                    }
                }
                DiskMsg::Commit => {
                    let _ = done_tx.send(file.commit());
                    return;
                }
            }
        }
    });
    (tx, created_rx, done_rx)
}

#[cfg(unix)]
mod sys {
    use std::{
        fs::{self, File},
        io,
        os::unix::fs::OpenOptionsExt,
        path::Path,
    };

    pub fn create_new(p: &Path) -> io::Result<File> {
        // never executable; O_EXCL also refuses an existing symlink
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(p)
    }

    pub fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{CWD, RenameFlags, renameat_with};
            match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
                Ok(()) => return Ok(()),
                // file systems without RENAME_NOREPLACE: fall back to a link, which also fails
                // on an existing name
                Err(e) if e == rustix::io::Errno::INVAL || e == rustix::io::Errno::NOSYS => {}
                Err(e) => return Err(e.into()),
            }
        }
        fs::hard_link(from, to)?;
        fs::remove_file(from)
    }

    pub fn free_space(p: &Path) -> io::Result<u64> {
        let s = rustix::fs::statvfs(p)?;
        Ok(s.f_bavail.saturating_mul(s.f_frsize))
    }
}

#[cfg(windows)]
mod sys {
    use std::{
        fs::{self, File},
        io,
        os::windows::ffi::OsStrExt,
        path::Path,
    };

    use windows::{
        Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, MOVEFILE_WRITE_THROUGH, MoveFileExW},
        core::PCWSTR,
    };

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn win_err(e: windows::core::Error) -> io::Error {
        io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
    }

    pub fn create_new(p: &Path) -> io::Result<File> {
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)?;
        // "Downloaded from the internet": Windows asks before running it, Office opens it in
        // protected view. The stream moves with the file. FAT drives can't hold it; fine.
        let mut motw = p.as_os_str().to_owned();
        motw.push(":Zone.Identifier");
        if let Err(e) = fs::write(&motw, b"[ZoneTransfer]\r\nZoneId=3\r\n") {
            tracing::debug!("files: no internet mark on {}: {e}", p.display());
        }
        Ok(f)
    }

    pub fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (wide(from), wide(to));
        // without MOVEFILE_REPLACE_EXISTING an existing target is an error
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe {
            MoveFileExW(
                PCWSTR(f.as_ptr()),
                PCWSTR(t.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(win_err)
    }

    pub fn free_space(p: &Path) -> io::Result<u64> {
        let w = wide(p);
        let mut free = 0u64;
        // SAFETY: NUL-terminated path; the out pointer is valid for the call.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&raw mut free), None, None) }
            .map_err(win_err)?;
        Ok(free)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_normal_names() {
        assert_eq!(clean_name("holiday.jpg"), "holiday.jpg");
        assert_eq!(clean_name("Résumé final v2.pdf"), "Résumé final v2.pdf");
        assert_eq!(clean_name("archive.tar.gz"), "archive.tar.gz");
    }

    #[test]
    fn drops_folders_and_dots() {
        assert_eq!(clean_name("../../x"), "x");
        assert_eq!(clean_name("..\\..\\x"), "x");
        assert_eq!(clean_name("/etc/passwd"), "passwd");
        assert_eq!(clean_name("C:\\Windows\\evil.dll"), "evil.dll");
        assert_eq!(clean_name("a/b\\c.txt"), "c.txt");
        assert_eq!(clean_name(".bashrc"), "bashrc");
        assert_eq!(clean_name("..."), "file");
        assert_eq!(clean_name(".."), "file");
        assert_eq!(clean_name("dir/"), "file");
        assert_eq!(clean_name("  name.txt  "), "name.txt");
        assert_eq!(clean_name("name.txt..."), "name.txt");
    }

    #[test]
    fn drops_characters_windows_or_terminals_choke_on() {
        assert_eq!(clean_name("a:b.txt"), "ab.txt");
        assert_eq!(clean_name("what?*<>|\".txt"), "what.txt");
        assert_eq!(clean_name("line\nbreak\u{7}.txt"), "linebreak.txt");
        // Right-to-left override: "evil\u{202E}gpj.exe" shows as "evilexe.jpg".
        assert_eq!(clean_name("evil\u{202E}gpj.exe"), "evilgpj.exe");
        assert_eq!(clean_name("zero\u{200B}width\u{FEFF}.txt"), "zerowidth.txt");
        assert_eq!(clean_name(""), "file");
        assert_eq!(clean_name("\u{0}"), "file");
    }

    #[test]
    fn renames_windows_device_names_everywhere() {
        for n in [
            "CON",
            "con.txt",
            "PRN.log",
            "aux",
            "NUL.tar.gz",
            "COM1",
            "lpt9.x",
        ] {
            let c = clean_name(n);
            assert_eq!(c, format!("_{n}"), "{n}");
        }
        assert_eq!(clean_name("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(clean_name("com10"), "com10");
        assert_eq!(clean_name("nul "), "_nul");
    }

    #[test]
    fn shortens_long_names_keeping_the_extension() {
        let long = format!("{}.mp4", "é".repeat(300)); // 600 bytes + ".mp4"
        let c = clean_name(&long);
        assert!(c.len() <= MAX_NAME_BYTES, "{}", c.len());
        assert!(c.ends_with(".mp4"), "{c}");
        assert!(c.starts_with("éé"));
        // a silly "extension" is not kept whole
        let c = clean_name(&format!("a.{}", "x".repeat(300)));
        assert!(c.len() <= MAX_NAME_BYTES);
        assert!(c.starts_with("a."));
    }

    #[test]
    fn numbering() {
        assert_eq!(numbered("a.txt", 0), "a.txt");
        assert_eq!(numbered("a.txt", 1), "a (1).txt");
        assert_eq!(numbered("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(numbered("README", 3), "README (3)");
    }

    #[test]
    fn admission() {
        let max = 2048 << 20;
        assert_eq!(admit(false, max, 1, u64::MAX), Err(FileRefusal::Disabled));
        assert_eq!(admit(true, max, max, u64::MAX), Ok(()));
        assert_eq!(
            admit(true, max, max + 1, u64::MAX),
            Err(FileRefusal::TooBig { max_bytes: max })
        );
        assert_eq!(
            admit(true, max, 10, SPARE_BYTES + 9),
            Err(FileRefusal::NoSpace)
        );
        assert_eq!(admit(true, max, 10, SPARE_BYTES + 10), Ok(()));
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn never_overwrites() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("DiShus");
        let save = |data: &[u8]| {
            let mut f = Incoming::create(&dir, "a.txt").unwrap();
            f.write(data).unwrap();
            f.commit().unwrap()
        };
        assert_eq!(save(b"one"), "a.txt");
        assert_eq!(save(b"two"), "a (1).txt");
        // a file that shows up while another one is coming in is not overwritten either
        let mut late = Incoming::create(&dir, "b.txt").unwrap();
        late.write(b"mine").unwrap();
        fs::write(dir.join("b.txt"), b"theirs").unwrap();
        assert_eq!(late.commit().unwrap(), "b (1).txt");
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"one");
        assert_eq!(fs::read(dir.join("a (1).txt")).unwrap(), b"two");
        assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"theirs");
        assert_eq!(fs::read(dir.join("b (1).txt")).unwrap(), b"mine");
        assert_eq!(names(&dir), ["a (1).txt", "a.txt", "b (1).txt", "b.txt"]);
    }

    #[test]
    fn a_leftover_part_file_is_not_reused_or_deleted() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().to_path_buf();
        fs::write(dir.join("a.txt.part"), b"someone else's").unwrap();
        let mut f = Incoming::create(&dir, "a.txt").unwrap();
        f.write(b"x").unwrap();
        drop(f); // cancelled
        assert_eq!(fs::read(dir.join("a.txt.part")).unwrap(), b"someone else's");
        assert_eq!(names(&dir), ["a.txt.part"]);
    }

    #[test]
    fn unfinished_files_leave_nothing() {
        let t = tempfile::tempdir().unwrap();
        let mut f = Incoming::create(t.path(), "big.bin").unwrap();
        f.write(&[7; 4096]).unwrap();
        assert_eq!(names(t.path()).len(), 1, "the .part exists while receiving");
        drop(f);
        assert!(names(t.path()).is_empty());
    }

    #[test]
    fn free_space_of_a_folder_not_made_yet() {
        let t = tempfile::tempdir().unwrap();
        assert!(free_space(&t.path().join("Downloads/DiShus")).unwrap() > 0);
    }

    #[cfg(unix)]
    #[test]
    fn received_files_are_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let mut f = Incoming::create(t.path(), "run.sh").unwrap();
        f.write(b"#!/bin/sh\n").unwrap();
        let n = f.commit().unwrap();
        let mode = fs::metadata(t.path().join(n)).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "{mode:o}");
    }
}
