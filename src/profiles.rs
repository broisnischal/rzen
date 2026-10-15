//! Local browser profiles: separate people or identities, each with its own tabs, history,
//! settings and website data (cookies, logins, storage). Optional password lock.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{self, root_dir};

/// The original single-profile install lives in the root dir and keeps WebKit's default
/// data store, so existing logins survive the upgrade to profiles.
pub const DEFAULT_ID: &str = "default";
const HASH_ROUNDS: u32 = 200_000;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// `salt$hash`, both hex. None = no lock.
    pub password: Option<String>,
    pub created: u64,
}

impl Default for Profile {
    fn default() -> Self {
        Self { id: DEFAULT_ID.into(), name: String::new(), password: None, created: now() }
    }
}

impl Profile {
    pub fn initial(&self) -> String {
        self.name.trim().chars().next().map_or_else(|| "•".into(), |c| c.to_uppercase().collect())
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Registry {
    pub profiles: Vec<Profile>,
    pub last: Option<String>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn registry_path() -> PathBuf {
    root_dir().join("profiles.json")
}

impl Registry {
    pub fn load() -> Self {
        model::load_json(&registry_path())
    }

    pub fn save(&self) {
        model::save_json(&registry_path(), self, true);
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    pub fn create(&mut self, name: &str, password: Option<&str>) -> String {
        let slug: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect::<String>().to_lowercase();
        let id = format!("{}-{}", if slug.is_empty() { "profile" } else { &slug }, hex(&random_bytes::<4>()));
        self.profiles.push(Profile {
            id: id.clone(),
            name: name.trim().chars().take(40).collect(),
            password: password.filter(|p| !p.is_empty()).map(hash_password),
            created: now(),
        });
        self.save();
        id
    }

}

pub fn dir(id: &str) -> PathBuf {
    let d = if id == DEFAULT_ID { root_dir() } else { root_dir().join("profiles").join(id) };
    let _ = fs::create_dir_all(&d);
    d
}

/// WebKit data-store identifier for a profile (and optionally one of its isolated spaces).
/// None = WebKit's default store (the default profile's shared spaces).
pub fn store_id(profile: &str, space: Option<u64>) -> Option<[u8; 16]> {
    match (profile == DEFAULT_ID, space) {
        (true, None) => None,
        // Keep the identifier existing isolated spaces already use.
        (true, Some(ws)) => {
            let mut id = *b"rzen-ws-\0\0\0\0\0\0\0\0";
            id[8..].copy_from_slice(&ws.to_be_bytes());
            Some(id)
        }
        (false, space) => {
            let digest = Sha256::digest(format!("rzen-profile/{profile}/{}", space.unwrap_or(0)));
            let mut id = [0u8; 16];
            id.copy_from_slice(&digest[..16]);
            Some(id)
        }
    }
}

pub fn hash_password(password: &str) -> String {
    let salt = random_bytes::<16>();
    format!("{}${}", hex(&salt), hex(&stretch(password, &salt)))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let Some((salt_hex, hash_hex)) = stored.split_once('$') else { return false };
    let Some(salt) = unhex(salt_hex) else { return false };
    let got = hex(&stretch(password, &salt));
    // Constant-time compare.
    got.len() == hash_hex.len() && got.bytes().zip(hash_hex.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Iterated, salted SHA-256: slows offline guessing of a stolen profiles.json.
fn stretch(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut h: [u8; 32] = Sha256::new().chain_update(salt).chain_update(password.as_bytes()).finalize().into();
    for _ in 0..HASH_ROUNDS {
        h = Sha256::new().chain_update(h).chain_update(salt).finalize().into();
    }
    h
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    #[cfg(unix)]
    {
        use std::io::Read;
        if fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok() {
            return buf;
        }
    }
    // Fallback: OS-seeded SipHash keys mixed with time and pid.
    use std::hash::{BuildHasher, Hasher};
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.write_usize(i ^ std::process::id() as usize);
        chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
    }
    buf
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_round_trip() {
        let stored = hash_password("correct horse");
        assert!(verify_password("correct horse", &stored));
        assert!(!verify_password("wrong", &stored));
        assert_ne!(stored, hash_password("correct horse"), "salted");
    }

    #[test]
    fn store_ids_are_stable_and_distinct() {
        assert_eq!(store_id(DEFAULT_ID, None), None);
        assert_eq!(store_id("a", None), store_id("a", None));
        assert_ne!(store_id("a", None), store_id("b", None));
        assert_ne!(store_id("a", None), store_id("a", Some(3)));
    }
}
