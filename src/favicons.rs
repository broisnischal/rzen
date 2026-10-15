//! Site icons kept on disk. Each is downloaded once and shown from a local `rzenicon://`
//! URL, so icons appear instantly, work offline and never cost the site another request.

use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
};

use sha2::{Digest, Sha256};

pub const SCHEME: &str = "rzenicon";
const MAX_BYTES: &str = "1000000";

fn dir() -> PathBuf {
    crate::model::root_dir().join("favicons")
}

fn key(url: &str) -> String {
    crate::profiles::hex(&Sha256::digest(url.as_bytes())[..12])
}

/// Keys already on disk, read once.
fn cached() -> &'static Mutex<HashSet<String>> {
    static C: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    C.get_or_init(|| {
        let names = fs::read_dir(dir()).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok());
        Mutex::new(names.collect())
    })
}

fn pending() -> &'static Mutex<HashSet<String>> {
    static P: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// The URL the sidebar should load: the local copy once there is one.
pub fn local(url: &str) -> String {
    if url.starts_with("http") && cached().lock().is_ok_and(|c| c.contains(&key(url))) {
        format!("{SCHEME}://i/{}", key(url))
    } else {
        url.to_string()
    }
}

/// Downloads an icon in the background unless it's already stored; `done` runs once it is.
pub fn ensure(url: &str, done: impl FnOnce() + Send + 'static) {
    if !url.starts_with("http") {
        return;
    }
    let k = key(url);
    if cached().lock().is_ok_and(|c| c.contains(&k)) || !pending().lock().is_ok_and(|mut p| p.insert(k.clone())) {
        return;
    }
    let url = url.to_string();
    std::thread::spawn(move || {
        let curl = if cfg!(windows) { "curl.exe" } else { "/usr/bin/curl" };
        let out = Command::new(curl).args(["-sSfL", "--max-time", "8", "--max-filesize", MAX_BYTES, "--", &url]).output();
        let bytes = out.ok().filter(|o| o.status.success()).map(|o| o.stdout).unwrap_or_default();
        let stored = sniff(&bytes).is_some() && fs::create_dir_all(dir()).is_ok() && fs::write(dir().join(&k), &bytes).is_ok();
        if let Ok(mut p) = pending().lock() {
            p.remove(&k);
        }
        if stored {
            if let Ok(mut c) = cached().lock() {
                c.insert(k);
            }
            done();
        }
    });
}

/// Serves `rzenicon://i/<key>`: the stored bytes and their image type.
pub fn serve(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let k = path.trim_matches('/');
    if k.is_empty() || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = fs::read(dir().join(k)).ok()?;
    let mime = sniff(&bytes)?;
    Some((bytes, mime))
}

/// Only real images are kept (sites often answer missing icons with an HTML page).
fn sniff(b: &[u8]) -> Option<&'static str> {
    let head = String::from_utf8_lossy(&b[..b.len().min(512)]).to_ascii_lowercase();
    match b {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0, 0, 1, 0, ..] => Some("image/x-icon"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ if head.contains("<svg") => Some("image/svg+xml"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_images_and_rejects_html() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(&[0, 0, 1, 0, 1, 0]), Some("image/x-icon"));
        assert_eq!(sniff(b"<?xml version=\"1.0\"?><svg xmlns=\"\"></svg>"), Some("image/svg+xml"));
        assert_eq!(sniff(b"<!doctype html><title>404</title>"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn serve_rejects_paths_outside_the_cache() {
        assert!(serve("/../../etc/passwd").is_none());
        assert!(serve("/").is_none());
    }
}
