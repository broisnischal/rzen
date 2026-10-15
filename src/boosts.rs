//! Boosts: your own CSS and JavaScript for a site (a calmer YouTube, a darker forum…),
//! applied as each of its pages starts loading. Stored per profile in boosts.json.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model;

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Boost {
    pub id: u64,
    pub name: String,
    /// The site, e.g. "youtube.com"; also applies to its subdomains.
    pub host: String,
    pub enabled: bool,
    /// Built-in preset ("youtube") whose options the Boosts page shows as switches.
    pub preset: String,
    pub options: Vec<String>,
    /// Code you wrote yourself.
    pub css: String,
    pub js: String,
    /// What actually runs: the chosen preset options plus your own code, assembled by the page.
    pub run_css: String,
    pub run_js: String,
}

impl Boost {
    pub fn matches(&self, host: &str) -> bool {
        let h = host.trim_start_matches("www.");
        !self.host.is_empty() && (h == self.host || h.strip_suffix(self.host.as_str()).is_some_and(|rest| rest.ends_with('.')))
    }

    /// Document start test for this boost's site.
    fn on_site(&self) -> String {
        format!("(h=>h==={0}||h.endsWith('.'+{0}))(location.hostname.replace(/^www\\./,''))", json!(self.host))
    }
}

pub fn load(dir: &Path) -> Vec<Boost> {
    model::load_json(&dir.join("boosts.json"))
}

pub fn save(dir: &Path, list: &[Boost]) {
    model::save_json(&dir.join("boosts.json"), &list, true);
}

/// "https://www.YouTube.com/watch?v=1" → "youtube.com".
pub fn clean_host(input: &str) -> Option<String> {
    let s = input.trim().to_ascii_lowercase();
    let s = s.split_once("://").map_or(s.as_str(), |(_, rest)| rest);
    let host = s.split(['/', '?', '#', ':']).next()?.trim_start_matches("www.").trim_end_matches('.');
    let valid = host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') && (host.contains('.') || host == "localhost");
    valid.then(|| host.to_string())
}

/// Style scripts for every enabled boost. Each adds its <style> at document start and moves
/// it to the end of <head> once parsed, so it wins over the site's own sheets.
pub fn style_scripts(list: &[Boost]) -> Vec<String> {
    list.iter()
        .filter(|b| b.enabled && !b.run_css.trim().is_empty())
        .map(|b| {
            format!(
                "(()=>{{if(!{on})return;const s=document.createElement('style');s.id='rzen-boost-{id}';s.setAttribute('data-rzen-boost','');s.textContent={css};(document.head||document.documentElement).appendChild(s);document.readyState==='loading'&&addEventListener('DOMContentLoaded',()=>s.isConnected&&document.head&&document.head.appendChild(s),{{once:true}})}})()",
                on = b.on_site(),
                id = b.id,
                css = json!(b.run_css),
            )
        })
        .collect()
}

/// Every boost's document-start scripts: styles, then code. Code is a separate script per
/// boost, so a typo in one boost's JavaScript can't take anything else down with it.
pub fn scripts(list: &[Boost]) -> Vec<String> {
    let mut out = style_scripts(list);
    out.extend(list.iter().filter(|b| b.enabled && !b.run_js.trim().is_empty()).map(|b| {
        format!("(()=>{{if(!{on})return;try{{\n{js}\n}}catch(e){{console.error('rzen boost',{name},e)}}}})()", on = b.on_site(), js = b.run_js, name = json!(b.name))
    }));
    out
}

/// Restyles an already-open page right away (code changes wait for its next load).
pub fn restyle_js(list: &[Boost]) -> String {
    let mut js = String::from("document.querySelectorAll('style[data-rzen-boost]').forEach(e=>e.remove());");
    for s in style_scripts(list) {
        js.push_str(&s);
        js.push(';');
    }
    js
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yt() -> Boost {
        Boost { id: 7, name: "YouTube".into(), host: "youtube.com".into(), enabled: true, run_css: "#comments{display:none!important}".into(), run_js: "window.x=1".into(), ..Default::default() }
    }

    #[test]
    fn hosts_are_cleaned_and_matched() {
        assert_eq!(clean_host("https://www.YouTube.com/watch?v=1").as_deref(), Some("youtube.com"));
        assert_eq!(clean_host("m.youtube.com:443").as_deref(), Some("m.youtube.com"));
        assert_eq!(clean_host("not a host"), None);
        let b = yt();
        assert!(b.matches("www.youtube.com") && b.matches("music.youtube.com") && b.matches("youtube.com"));
        assert!(!b.matches("notyoutube.com") && !b.matches("youtube.com.evil.net"));
    }

    #[test]
    fn scripts_skip_disabled_and_empty() {
        let mut b = yt();
        assert_eq!(scripts(&[b.clone()]).len(), 2);
        b.run_js.clear();
        assert_eq!(scripts(&[b.clone()]).len(), 1);
        b.enabled = false;
        assert!(scripts(&[b]).is_empty());
    }

    #[test]
    fn css_is_embedded_as_a_string() {
        let mut b = yt();
        b.run_css = "a::after{content:'\"</style>'}".into();
        let s = &style_scripts(&[b])[0];
        assert!(s.contains(r#"s.textContent="a::after{content:'\"</style>'}""#));
    }
}
