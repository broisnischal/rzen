//! Ad and tracker blocking from the community filter lists (EasyList, EasyPrivacy, Peter
//! Lowe's, uBlock's), converted to WebKit content-rule JSON. Safari's native engine compiles
//! that to bytecode once; it's memory-mapped and shared by every page, so a hundred thousand
//! rules cost almost no RAM and no JavaScript runs per request.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    process::Command,
    sync::{LazyLock, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// (id, name, url). Order matters only for ties; duplicates across lists are merged.
pub const SOURCES: &[(&str, &str, &str)] = &[
    ("easylist", "EasyList", "https://easylist.to/easylist/easylist.txt"),
    ("easyprivacy", "EasyPrivacy", "https://easylist.to/easylist/easyprivacy.txt"),
    ("pgl", "Peter Lowe's ad servers", "https://pgl.yoyo.org/adservers/serverlist.php?hostformat=adblockplus&showintro=0&mimetype=plaintext"),
    ("ubo-filters", "uBlock filters", "https://ublockorigin.github.io/uAssets/filters/filters.min.txt"),
    ("ubo-privacy", "uBlock privacy", "https://ublockorigin.github.io/uAssets/filters/privacy.min.txt"),
];
const MAX_AGE: Duration = Duration::from_secs(4 * 24 * 3600);
/// Bump when the converter's output changes, so cached lists recompile.
const CONVERTER: u32 = 1;
/// Rules per compiled list (WebKit's hard limit is 150,000); smaller lists compile with a
/// lower memory peak.
const CHUNK: usize = 40_000;
/// Generic hiding selectors joined into one rule.
const BATCH: usize = 400;
#[cfg(windows)]
const CURL: &str = "curl.exe";
#[cfg(not(windows))]
const CURL: &str = "/usr/bin/curl";

pub fn dir() -> PathBuf {
    let d = crate::model::root_dir().join("filters");
    let _ = fs::create_dir_all(&d);
    d
}

fn list_path(id: &str) -> PathBuf {
    dir().join(format!("{id}.txt"))
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ---------- status ----------

#[derive(Serialize, Clone, Default)]
pub struct Source {
    pub id: &'static str,
    pub name: &'static str,
    /// When the cached copy was last downloaded (unix seconds).
    pub updated: Option<u64>,
    pub bytes: u64,
}

#[derive(Serialize, Clone, Default)]
pub struct FilterStatus {
    pub enabled: bool,
    /// "off" | "builtin" | "downloading" | "converting" | "compiling" | "ready"
    pub state: &'static str,
    /// Compiled rule lists in use, and the rules in them.
    pub lists: usize,
    pub rules: usize,
    pub network: usize,
    pub cosmetic: usize,
    /// Filters WebKit can't express (scriptlets, regex, procedural cosmetics…).
    pub skipped: usize,
    /// Newest download among the sources (unix seconds).
    pub updated: Option<u64>,
    pub error: Option<String>,
    pub sources: Vec<Source>,
}

static STATUS: LazyLock<Mutex<FilterStatus>> = LazyLock::new(|| Mutex::new(FilterStatus { state: "off", ..Default::default() }));
static LISTENER: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// Current blocker state, with the cached sources read fresh from disk.
pub fn status() -> FilterStatus {
    let mut s = STATUS.lock().unwrap().clone();
    s.sources = SOURCES
        .iter()
        .map(|&(id, name, _)| {
            let meta = fs::metadata(list_path(id)).ok();
            Source { id, name, updated: meta.as_ref().and_then(|m| m.modified().ok()).map(unix), bytes: meta.map_or(0, |m| m.len()) }
        })
        .collect();
    s.updated = s.sources.iter().filter_map(|x| x.updated).max();
    s
}

/// Called (from any thread) whenever `status()` changes, e.g. to refresh the settings page.
pub fn set_listener(f: impl Fn() + Send + 'static) {
    *LISTENER.lock().unwrap() = Some(Box::new(f));
}

pub(crate) fn update(f: impl FnOnce(&mut FilterStatus)) {
    f(&mut STATUS.lock().unwrap());
    if let Some(l) = &*LISTENER.lock().unwrap() {
        l();
    }
}

// ---------- download ----------

fn looks_like_filters(b: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&b[..b.len().min(200_000)]);
    b.len() > 1000 && text.lines().filter(|l| l.contains("||") || l.contains("##")).take(20).count() >= 20
}

fn stale(id: &str) -> bool {
    fs::metadata(list_path(id)).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_none_or(|age| age >= MAX_AGE)
}

/// Some list is missing or older than four days.
pub fn needs_download() -> bool {
    SOURCES.iter().any(|(id, ..)| stale(id))
}

/// Fetches lists that are missing or older than four days (all of them when `force`). Keeps
/// the cached copy when offline or when a download looks wrong. Runs curl; call off the
/// main thread.
pub fn download(force: bool) {
    for &(id, _, url) in SOURCES {
        if !force && !stale(id) {
            continue;
        }
        let path = list_path(id);
        let part = path.with_extension("part");
        let ok = Command::new(CURL)
            .args(["-fsSL", "--compressed", "--connect-timeout", "15", "--max-time", "120", "-o"])
            .arg(&part)
            .arg(url)
            .status()
            .is_ok_and(|s| s.success());
        if ok && fs::read(&part).is_ok_and(|b| looks_like_filters(&b)) {
            let _ = fs::rename(&part, &path);
        } else {
            let _ = fs::remove_file(&part);
            log!("rzen: couldn't update filter list {id}");
        }
    }
}

/// Cached list texts and a version key covering their content and the converter.
pub fn load_cached() -> Option<(Vec<String>, String)> {
    let mut h = Sha256::new();
    h.update(CONVERTER.to_le_bytes());
    let texts: Vec<String> = SOURCES
        .iter()
        .filter_map(|&(id, ..)| fs::read(list_path(id)).ok())
        .map(|b| {
            h.update(&b);
            String::from_utf8_lossy(&b).into_owned()
        })
        .collect();
    (!texts.is_empty()).then(|| (texts, crate::profiles::hex(&h.finalize()[..12])))
}

/// What's compiled in WebKit's rule-list store, so startup can look lists up instead of
/// recompiling.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Manifest {
    pub version: String,
    pub ids: Vec<String>,
    pub rules: usize,
    pub network: usize,
    pub cosmetic: usize,
    pub skipped: usize,
}

impl Manifest {
    pub fn load() -> Option<Self> {
        fs::read(dir().join("compiled.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    pub fn save(&self) {
        crate::model::save_json(&dir().join("compiled.json"), self, false);
    }
}

// ---------- conversion ----------

pub struct Converted {
    /// (WebKit list identifier, rules JSON)
    pub lists: Vec<(String, String)>,
    /// Rules across all lists (exceptions repeat in every network list).
    pub rules: usize,
    pub network: usize,
    pub cosmetic: usize,
    pub skipped: usize,
}

#[derive(Serialize)]
struct Trigger<'a> {
    #[serde(rename = "url-filter")]
    url_filter: &'a str,
    #[serde(rename = "url-filter-is-case-sensitive", skip_serializing_if = "std::ops::Not::not")]
    case: bool,
    #[serde(rename = "resource-type", skip_serializing_if = "Vec::is_empty")]
    types: Vec<&'static str>,
    #[serde(rename = "load-type", skip_serializing_if = "Option::is_none")]
    load: Option<[&'static str; 1]>,
    #[serde(rename = "load-context", skip_serializing_if = "Option::is_none")]
    context: Option<[&'static str; 1]>,
    #[serde(rename = "if-domain", skip_serializing_if = "Vec::is_empty")]
    if_domain: Vec<String>,
    #[serde(rename = "unless-domain", skip_serializing_if = "Vec::is_empty")]
    unless_domain: Vec<String>,
}

impl<'a> Trigger<'a> {
    fn url(url_filter: &'a str) -> Self {
        Self { url_filter, case: false, types: Vec::new(), load: None, context: None, if_domain: Vec::new(), unless_domain: Vec::new() }
    }

    fn rule(&self, kind: &str, selector: Option<&str>) -> String {
        let mut action = serde_json::json!({ "type": kind });
        if let Some(s) = selector {
            action["selector"] = s.into();
        }
        serde_json::json!({ "trigger": self, "action": action }).to_string()
    }
}

/// Types a rule applies to when a filter names none: everything but the page itself.
const ALL_TYPES: &[&str] =
    &["document", "image", "style-sheet", "script", "font", "svg-document", "media", "popup", "ping", "fetch", "websocket", "other"];
/// After every network list: never block a page you navigate to (filters target the ads
/// inside it; blocking the page itself would just show an error).
const TOP_FRAME_ALLOW: &str = r#"{"trigger":{"url-filter":".*","resource-type":["document"],"load-context":["top-frame"]},"action":{"type":"ignore-previous-rules"}}"#;
const HOST_PREFIX: &str = r"^[^:]+://+([^:/]+\.)?";
/// ABP's `^`: anything but a letter, digit or `_-.%`.
const SEPARATOR: &str = "[^a-zA-Z0-9_.%-]";

/// A normalized domain for if-domain/unless-domain, or None if WebKit can't take it.
fn domain(d: &str) -> Option<String> {
    let d = d.trim().trim_start_matches('.').to_ascii_lowercase();
    let ok = !d.is_empty() && !d.ends_with('.') && d.contains('.') && d.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-');
    ok.then(|| format!("*{d}"))
}

/// Splits `a.com|~b.com` (or comma-separated, for cosmetic filters) into include/exclude.
fn domains(list: &str, sep: char) -> Option<(Vec<String>, Vec<String>)> {
    let (mut inc, mut exc) = (Vec::new(), Vec::new());
    for d in list.split(sep).filter(|d| !d.is_empty()) {
        // Entities (`example.*`) and regex domains have no WebKit equivalent.
        let (neg, d) = d.strip_prefix('~').map_or((false, d), |d| (true, d));
        let d = domain(d)?;
        if neg { exc.push(d) } else { inc.push(d) }
    }
    Some((inc, exc))
}

/// ABP URL pattern → WebKit url-filter (its regex subset: no alternation or counted repeats).
fn url_filter(pattern: &str) -> Option<String> {
    if !pattern.is_ascii() || pattern.contains(['{', '}']) {
        return None;
    }
    let mut s = pattern;
    let mut out = String::with_capacity(s.len() + 24);
    let host = s.starts_with("||");
    if host {
        s = &s[2..];
        out.push_str(HOST_PREFIX);
    } else if let Some(rest) = s.strip_prefix('|') {
        s = rest;
        out.push('^');
    }
    let end = s.len() > 1 && s.ends_with('|');
    if end {
        s = &s[..s.len() - 1];
    }
    if !host && !out.starts_with('^') {
        s = s.trim_start_matches('*');
    }
    if !end {
        s = s.trim_end_matches('*');
    }
    if s.is_empty() || s.contains('|') {
        return None;
    }
    let bytes = s.as_bytes();
    // Still inside the hostname of a `||host…` pattern (a `^` there means `:` or `/`).
    let mut in_host = host;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'*' => {
                if !out.ends_with(".*") {
                    out.push_str(".*");
                }
                in_host = false;
            }
            b'^' if in_host => {
                out.push_str("[/:]");
                in_host = false;
            }
            // A trailing `^` also matches the end of the address.
            b'^' if i == bytes.len() - 1 && !end => {
                out.push('(');
                out.push_str(SEPARATOR);
                out.push_str(".*)?$");
                return Some(out);
            }
            b'^' => out.push_str(SEPARATOR),
            b'.' | b'+' | b'?' | b'(' | b')' | b'[' | b']' | b'\\' | b'$' => {
                out.push('\\');
                out.push(c as char);
            }
            b'/' | b':' => {
                in_host = false;
                out.push(c as char);
            }
            _ => out.push(c as char),
        }
    }
    if end {
        out.push('$');
    }
    Some(out)
}

/// `||host^` / `||host` with nothing after the host: the filter is about a whole site.
fn host_only(pattern: &str) -> Option<&str> {
    let h = pattern.strip_prefix("||")?;
    let h = h.strip_suffix('^').unwrap_or(h);
    (!h.is_empty() && h.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')).then_some(h)
}

/// Selectors WebKit's CSS parser accepts: no uBlock/ABP procedural or action operators.
fn selector_ok(sel: &str) -> bool {
    const BAD: &[&str] = &[
        "-abp-", ":has-text", ":contains(", ":xpath", ":upward", ":remove", ":style(", ":matches-", ":min-text-length", ":watch-attr",
        ":others", ":if(", ":if-not", ":nth-ancestor", ":spath", ":shadow", ":properties(", ":matches(", "{", "}", "\\",
    ];
    if sel.is_empty() || sel.len() > 1000 || !sel.is_ascii() || sel.starts_with(['+', '^', ',']) || BAD.iter().any(|b| sel.contains(b)) {
        return false;
    }
    // Balanced brackets and quotes; no `:has()` inside `:has()` (CSS forbids it).
    let (mut paren, mut square, mut quote) = (0i32, 0i32, None::<u8>);
    let mut has_depth: Vec<i32> = Vec::new();
    for (i, c) in sel.bytes().enumerate() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(c),
            (None, b'(') => {
                if sel[..i].ends_with(":has") {
                    if !has_depth.is_empty() {
                        return false;
                    }
                    has_depth.push(paren);
                }
                paren += 1;
            }
            (None, b')') => {
                paren -= 1;
                if has_depth.last() == Some(&paren) {
                    has_depth.pop();
                }
            }
            (None, b'[') => square += 1,
            (None, b']') => square -= 1,
            _ => {}
        }
        if paren < 0 || square < 0 {
            return false;
        }
    }
    quote.is_none() && paren == 0 && square == 0
}

#[derive(Default)]
struct Builder {
    blocks: Vec<String>,
    exceptions: Vec<String>,
    seen: HashSet<String>,
    generic: Vec<String>,
    generic_seen: HashSet<String>,
    /// Generic selectors switched off everywhere (`#@#sel`) or on some sites.
    generic_off: HashSet<String>,
    generic_off_on: HashMap<String, Vec<String>>,
    /// (if-domains, unless-domains) → selectors.
    specific: HashMap<(Vec<String>, Vec<String>), Vec<String>>,
    specific_order: Vec<(Vec<String>, Vec<String>)>,
    specific_off: HashSet<(String, String)>,
    generichide: Vec<String>,
    elemhide: Vec<String>,
    skipped: usize,
}

impl Builder {
    fn push_unique(seen: &mut HashSet<String>, list: &mut Vec<String>, rule: String) {
        if seen.insert(rule.clone()) {
            list.push(rule);
        }
    }

    fn line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('!') || line.starts_with('[') {
            return;
        }
        // `#@#` first: in `site#@##id` the `##` belongs to the selector.
        let ok = match line.find("#@#").or_else(|| line.find("##")) {
            Some(i) => !line[..i].contains('/') && self.cosmetic(line, i),
            None if ["#?#", "#$#", "#@$#", "#@?#", "#%#"].iter().any(|m| line.contains(m)) => false,
            // hosts-file style comment
            None if line.starts_with('#') => return,
            None => self.network(line),
        };
        if !ok {
            self.skipped += 1;
        }
    }

    fn cosmetic(&mut self, line: &str, i: usize) -> bool {
        let exception = line[i..].starts_with("#@#");
        let sel = line[i + if exception { 3 } else { 2 }..].trim();
        if !selector_ok(sel) {
            return false;
        }
        let Some((inc, exc)) = domains(&line[..i], ',') else { return false };
        if exception {
            match (inc.is_empty(), exc.is_empty()) {
                (true, true) => {
                    self.generic_off.insert(sel.to_string());
                }
                (false, _) => {
                    self.generic_off_on.entry(sel.to_string()).or_default().extend(inc.iter().cloned());
                    for d in inc {
                        self.specific_off.insert((d, sel.to_string()));
                    }
                }
                _ => return false,
            }
            return true;
        }
        if inc.is_empty() && exc.is_empty() {
            if self.generic_seen.insert(sel.to_string()) {
                self.generic.push(sel.to_string());
            }
            return true;
        }
        // WebKit can't combine if-domain and unless-domain in one trigger.
        if !inc.is_empty() && !exc.is_empty() {
            return false;
        }
        let key = (inc, exc);
        if !self.specific.contains_key(&key) {
            self.specific_order.push(key.clone());
        }
        self.specific.entry(key).or_default().push(sel.to_string());
        true
    }

    fn network(&mut self, line: &str) -> bool {
        let (exception, body) = line.strip_prefix("@@").map_or((false, line), |b| (true, b));
        let (pattern, opts) = match body.rfind('$') {
            Some(i) if body[i + 1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '~') => (&body[..i], &body[i + 1..]),
            _ => (body, ""),
        };
        // Regex filters.
        if pattern.len() > 1 && pattern.starts_with('/') && pattern.ends_with('/') {
            return false;
        }
        let mut case = false;
        let mut load = None;
        let (mut types, mut not_types) = (Vec::new(), Vec::new());
        let mut child_frame = false;
        let (mut inc, mut exc) = (Vec::new(), Vec::new());
        let (mut document, mut elemhide, mut generichide) = (false, false, false);
        for opt in opts.split(',').filter(|o| !o.is_empty()) {
            let (neg, opt) = opt.strip_prefix('~').map_or((false, opt), |o| (true, o));
            let (name, value) = opt.split_once('=').unwrap_or((opt, ""));
            let ty = match name {
                "third-party" | "3p" => {
                    load = Some(if neg { "first-party" } else { "third-party" });
                    continue;
                }
                "first-party" | "1p" => {
                    load = Some(if neg { "third-party" } else { "first-party" });
                    continue;
                }
                "domain" | "from" => {
                    let Some(d) = domains(value, '|') else { return false };
                    (inc, exc) = d;
                    continue;
                }
                "match-case" => {
                    case = true;
                    continue;
                }
                "important" | "all" => continue,
                "document" | "doc" if exception => {
                    document = true;
                    continue;
                }
                "elemhide" | "ehide" if exception => {
                    elemhide = true;
                    continue;
                }
                "generichide" | "ghide" if exception => {
                    generichide = true;
                    continue;
                }
                "script" => "script",
                "image" => "image",
                "stylesheet" | "css" => "style-sheet",
                "font" => "font",
                "media" => "media",
                "xmlhttprequest" | "xhr" => "fetch",
                "websocket" => "websocket",
                "ping" | "beacon" => "ping",
                "other" => "other",
                "subdocument" | "frame" => {
                    if !neg {
                        child_frame = true;
                    }
                    "document"
                }
                "popup" => "popup",
                // object, redirect, csp, removeparam, badfilter, denyallow, … have no equivalent.
                _ => return false,
            };
            if neg { not_types.push(ty) } else { types.push(ty) }
        }
        if !inc.is_empty() && !exc.is_empty() {
            return false;
        }
        if !not_types.is_empty() {
            if !types.is_empty() {
                return false;
            }
            types = ALL_TYPES.iter().copied().filter(|t| !not_types.contains(t)).collect();
        }
        types.sort_unstable();
        types.dedup();

        // Site-wide exceptions.
        if document || elemhide || generichide {
            let trigger;
            let filter_owned;
            if let Some(h) = host_only(pattern) {
                let Some(d) = domain(h) else { return false };
                trigger = Trigger { if_domain: vec![d], ..Trigger::url(".*") };
            } else {
                let Some(f) = url_filter(pattern) else { return false };
                filter_owned = f;
                trigger = Trigger::url(&filter_owned);
            }
            let rule = trigger.rule("ignore-previous-rules", None);
            if document {
                Self::push_unique(&mut self.seen, &mut self.exceptions, rule.clone());
                self.elemhide.push(rule);
            } else if elemhide {
                self.elemhide.push(rule);
            } else {
                self.generichide.push(rule);
            }
            return true;
        }

        let Some(filter) = (if pattern.is_empty() || pattern == "*" { Some(".*".to_string()) } else { url_filter(pattern) }) else {
            return false;
        };
        // An unrestricted filter shorter than this would hit ordinary pages.
        let literal = filter.bytes().filter(|c| c.is_ascii_alphanumeric()).count();
        if !exception && inc.is_empty() && (filter == ".*" || literal < 3) && (types.is_empty() || load.is_none()) {
            return false;
        }
        let trigger = Trigger {
            case,
            types,
            load: load.map(|l| [l]),
            context: child_frame.then_some(["child-frame"]),
            if_domain: inc,
            unless_domain: exc,
            ..Trigger::url(&filter)
        };
        if exception {
            let rule = trigger.rule("ignore-previous-rules", None);
            Self::push_unique(&mut self.seen, &mut self.exceptions, rule);
        } else {
            let rule = trigger.rule("block", None);
            Self::push_unique(&mut self.seen, &mut self.blocks, rule);
        }
        true
    }

    fn finish(self) -> Converted {
        let network = self.blocks.len();
        let mut lists = Vec::new();
        let mut rules_total = 0;
        // Exceptions only override rules in their own list, so every chunk gets them all.
        let room = CHUNK.saturating_sub(self.exceptions.len() + 1).max(1000);
        for (n, chunk) in self.blocks.chunks(room).enumerate() {
            let mut rules: Vec<&str> = chunk.iter().map(String::as_str).collect();
            rules.extend(self.exceptions.iter().map(String::as_str));
            rules.push(TOP_FRAME_ALLOW);
            rules_total += rules.len();
            lists.push((format!("rzen-net-{n}"), format!("[{}]", rules.join(","))));
        }

        let mut cosmetic = Vec::new();
        let generic: Vec<&String> = self.generic.iter().filter(|s| !self.generic_off.contains(*s)).collect();
        let (plain, limited): (Vec<&String>, Vec<&String>) = generic.into_iter().partition(|s| !self.generic_off_on.contains_key(*s));
        for batch in plain.chunks(BATCH) {
            let sel = batch.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
            cosmetic.push(Trigger::url(".*").rule("css-display-none", Some(&sel)));
        }
        for sel in limited {
            let unless = self.generic_off_on[sel].clone();
            cosmetic.push(Trigger { unless_domain: unless, ..Trigger::url(".*") }.rule("css-display-none", Some(sel)));
        }
        cosmetic.extend(self.generichide);
        for key in &self.specific_order {
            let sels = &self.specific[key];
            let (mut inc, exc) = key.clone();
            // `site#@#sel` turns a site-specific selector off on that site.
            if !self.specific_off.is_empty() && sels.len() == 1 {
                inc.retain(|d| !self.specific_off.contains(&(d.clone(), sels[0].clone())));
                if inc.is_empty() && exc.is_empty() {
                    continue;
                }
            }
            for batch in sels.chunks(BATCH) {
                let sel = batch.join(", ");
                cosmetic.push(Trigger { if_domain: inc.clone(), unless_domain: exc.clone(), ..Trigger::url(".*") }.rule("css-display-none", Some(&sel)));
            }
        }
        cosmetic.extend(self.elemhide);
        let cosmetic_count = cosmetic.len();
        for (n, chunk) in cosmetic.chunks(CHUNK).enumerate() {
            lists.push((format!("rzen-cosmetic-{n}"), format!("[{}]", chunk.join(","))));
        }
        Converted { lists, rules: rules_total + cosmetic_count, network, cosmetic: cosmetic_count, skipped: self.skipped }
    }
}

/// Converts filter-list texts (ABP / uBlock syntax) into WebKit rule lists.
pub fn convert(texts: &[String]) -> Converted {
    let mut b = Builder::default();
    for text in texts {
        for line in text.lines() {
            b.line(line);
        }
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> serde_json::Value {
        let c = convert(&[line.to_string()]);
        let v: serde_json::Value = serde_json::from_str(&c.lists[0].1).unwrap();
        v[0].clone()
    }

    #[test]
    fn host_rules() {
        let r = one("||doubleclick.net^");
        assert_eq!(r["trigger"]["url-filter"], r"^[^:]+://+([^:/]+\.)?doubleclick\.net[/:]");
        assert_eq!(r["action"]["type"], "block");
        assert!(r["trigger"].get("resource-type").is_none());
    }

    #[test]
    fn options_map_to_triggers() {
        let r = one("||ads.example.com/banner^$script,third-party,domain=a.com|b.org");
        let t = &r["trigger"];
        assert_eq!(t["url-filter"], r"^[^:]+://+([^:/]+\.)?ads\.example\.com/banner([^a-zA-Z0-9_.%-].*)?$");
        assert_eq!(t["resource-type"], serde_json::json!(["script"]));
        assert_eq!(t["load-type"], serde_json::json!(["third-party"]));
        assert_eq!(t["if-domain"], serde_json::json!(["*a.com", "*b.org"]));
        let r = one("/adframe.$subdocument,~third-party,domain=~news.com");
        assert_eq!(r["trigger"]["url-filter"], r"/adframe\.");
        assert_eq!(r["trigger"]["load-context"], serde_json::json!(["child-frame"]));
        assert_eq!(r["trigger"]["load-type"], serde_json::json!(["first-party"]));
        assert_eq!(r["trigger"]["unless-domain"], serde_json::json!(["*news.com"]));
        let r = one("|https://cdn.example.com/*/ads.js|$~image");
        assert_eq!(r["trigger"]["url-filter"], r"^https://cdn\.example\.com/.*/ads\.js$");
        assert!(!r["trigger"]["resource-type"].as_array().unwrap().contains(&"image".into()));
    }

    #[test]
    fn skips_what_webkit_cannot_express() {
        for line in [
            "/^https?:\\/\\/ads/$script",
            "||example.com^$redirect=noopjs",
            "||example.com^$removeparam=utm",
            "example.com##+js(set-constant, x, 1)",
            "example.com#?#div:has-text(Sponsored)",
            "example.com##.ad:style(display: none)",
            "example.com##div:has(> div:has(> .adsbygoogle))",
            "||例子.com^",
            "||a.com^$domain=b.com|~c.b.com",
            "ad",
        ] {
            let c = convert(&[line.to_string()]);
            assert_eq!(c.skipped, 1, "{line}");
        }
    }

    #[test]
    fn exceptions_follow_blocks_in_every_list() {
        let c = convert(&["||ads.com^\n@@||ads.com/ok.js\n@@||good.com^$document".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&c.lists[0].1).unwrap();
        let types: Vec<&str> = v.as_array().unwrap().iter().map(|r| r["action"]["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["block", "ignore-previous-rules", "ignore-previous-rules", "ignore-previous-rules"]);
        assert_eq!(v[2]["trigger"]["if-domain"], serde_json::json!(["*good.com"]));
    }

    #[test]
    fn cosmetic_rules() {
        assert!(selector_ok("a:has(> .x), b:has(.y)"));
        let c = convert(&["##.ad-banner\n##.sponsor\n##.keep\n#@#.keep\n##.mostly\nsite.com#@#.mostly\nnews.com,blog.org##.promo".to_string()]);
        let (id, json) = c.lists.last().unwrap();
        assert!(id.starts_with("rzen-cosmetic"));
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v[0]["action"]["selector"], ".ad-banner, .sponsor");
        assert_eq!(v[1]["action"]["selector"], ".mostly");
        assert_eq!(v[1]["trigger"]["unless-domain"], serde_json::json!(["*site.com"]));
        assert_eq!(v[2]["trigger"]["if-domain"], serde_json::json!(["*news.com", "*blog.org"]));
        assert_eq!(v[2]["action"]["selector"], ".promo");
    }

    #[test]
    fn url_filters_use_webkits_regex_subset() {
        let lines = "||a.com^\n|http://x.com/ad^?\n/ads/*/banner^\n&ad_type=\n-ad-300x250.\n||b.net/p.gif|\n.com/ads/$image";
        let c = convert(&[lines.to_string()]);
        let v: serde_json::Value = serde_json::from_str(&c.lists[0].1).unwrap();
        for r in v.as_array().unwrap() {
            let f = r["trigger"]["url-filter"].as_str().unwrap();
            assert!(!f.contains('|') && !f.contains('{'), "{f}");
            let depth = f.bytes().try_fold(0i32, |d, c| match c {
                b'(' => Some(d + 1),
                b')' => (d > 0).then_some(d - 1),
                _ => Some(d),
            });
            assert!(f.contains(r"\(") || depth == Some(0), "{f}");
            assert!(f.rfind('^').is_none_or(|i| i == 0 || f[..i].ends_with('[')), "{f}");
        }
    }

    /// `RZEN_FILTERS_IN=<dir with list .txt files> RZEN_FILTERS_OUT=<dir> cargo test --release
    /// filters::tests::dump -- --ignored --nocapture` writes the converted lists for inspection.
    #[test]
    #[ignore]
    fn dump() {
        let input = PathBuf::from(std::env::var("RZEN_FILTERS_IN").unwrap());
        let out = PathBuf::from(std::env::var("RZEN_FILTERS_OUT").unwrap());
        let texts: Vec<String> = SOURCES.iter().filter_map(|(id, ..)| fs::read_to_string(input.join(format!("{id}.txt"))).ok()).collect();
        let t = std::time::Instant::now();
        let c = convert(&texts);
        println!("converted {} lists in {:?}: network {}, cosmetic {}, skipped {}", c.lists.len(), t.elapsed(), c.network, c.cosmetic, c.skipped);
        fs::create_dir_all(&out).unwrap();
        let mut b = Builder::default();
        texts.iter().flat_map(|t| t.lines()).for_each(|l| b.line(l));
        let mut sels: Vec<&String> = b.generic.iter().collect();
        sels.extend(b.specific.values().flatten());
        fs::write(out.join("selectors.txt"), sels.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")).unwrap();
        for (id, json) in &c.lists {
            let n = serde_json::from_str::<serde_json::Value>(json).unwrap().as_array().unwrap().len();
            println!("{id}: {n} rules, {} KB", json.len() / 1024);
            fs::write(out.join(format!("{id}.json")), json).unwrap();
        }
    }
}
