//! Browsing history with frecency-ranked search for the command bar.

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::util::short_host;

const MAX_ENTRIES: usize = 20_000;
const TRIM_TO: usize = 15_000;

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub url: String,
    pub title: String,
    pub visits: u32,
    /// Unix seconds of the last visit.
    pub last: u64,
}

impl Entry {
    fn frecency(&self, now: u64) -> f32 {
        let days = now.saturating_sub(self.last) as f32 / 86_400.0;
        (self.visits as f32 + 1.0).ln() * 2.0 + 4.0 / (1.0 + days)
    }
}

pub struct History {
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    path: PathBuf,
    pub dirty: bool,
    /// Bumped on every change so the UI only re-fetches its index when needed.
    pub version: u64,
    /// Favicon URL per site host, so suggestions show real site icons.
    icons: HashMap<String, String>,
    icons_dirty: bool,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn recordable(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

impl History {
    pub fn load(path: PathBuf) -> Self {
        let entries: Vec<Entry> = crate::model::load_json(&path);
                let icons = crate::model::load_json(&path.with_file_name("icons.json"));
        let mut h = Self { entries, index: HashMap::new(), path, dirty: false, version: 0, icons, icons_dirty: false };
        h.reindex();
        h
    }

    fn reindex(&mut self) {
        self.index = self.entries.iter().enumerate().map(|(i, e)| (e.url.clone(), i)).collect();
    }

    pub fn visit(&mut self, url: &str, title: &str) {
        if !recordable(url) {
            return;
        }
        let t = now();
        match self.index.get(url) {
            Some(&i) => {
                let e = &mut self.entries[i];
                e.visits += 1;
                e.last = t;
                if !title.is_empty() && e.title != title {
                    e.title = title.to_string();
                }
            }
            None => {
                let e = Entry { url: url.to_string(), title: title.to_string(), visits: 1, last: t };
                self.index.insert(e.url.clone(), self.entries.len());
                self.entries.push(e);
                if self.entries.len() > MAX_ENTRIES {
                    self.entries.sort_unstable_by(|a, b| b.frecency(t).total_cmp(&a.frecency(t)));
                    self.entries.truncate(TRIM_TO);
                    self.reindex();
                }
            }
        }
        self.dirty = true;
        self.version += 1;
    }

    pub fn set_title(&mut self, url: &str, title: &str) {
        if let Some(&i) = self.index.get(url) {
            let e = &mut self.entries[i];
            if e.title != title {
                e.title = title.to_string();
                self.dirty = true;
        self.version += 1;
            }
        }
    }

    /// The `n` best entries by frecency as `(url, title, score)`, for the command bar's
    /// local index. Matching then runs in the UI on every keystroke with no IPC.
    pub fn top(&self, n: usize) -> Vec<(&str, &str, f32)> {
        let t = now();
        let mut v: Vec<(&str, &str, f32)> = self.entries.iter().map(|e| (e.url.as_str(), e.title.as_str(), e.frecency(t))).collect();
        if v.len() > n {
            v.select_nth_unstable_by(n - 1, |a, b| b.2.total_cmp(&a.2));
            v.truncate(n);
        }
        v.sort_unstable_by(|a, b| b.2.total_cmp(&a.2));
        v
    }

    pub fn set_icon(&mut self, page_url: &str, icon: &str) {
        if !recordable(page_url) || icon.is_empty() {
            return;
        }
        let host = short_host(page_url).to_string();
        if self.icons.get(&host).map(String::as_str) != Some(icon) {
            self.icons.insert(host, icon.to_string());
            self.icons_dirty = true;
            self.version += 1;
        }
    }

    pub fn icon_for(&self, url: &str) -> Option<&str> {
        self.icons.get(short_host(url)).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.dirty = true;
        self.version += 1;
        self.save();
    }

    pub fn save(&mut self) {
        if self.icons_dirty {
            crate::model::save_json(&self.path.with_file_name("icons.json"), &self.icons, false);
            self.icons_dirty = false;
        }
        if self.dirty {
            crate::model::save_json(&self.path, &self.entries, false);
            self.dirty = false;
        }
    }
}
