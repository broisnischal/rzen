//! Profiles gate, AI panel, media controls and the AI-agent API — all on `Browser`.

use std::time::Instant;

use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use serde_json::{Value, json};

use super::*;

/// Encrypted-DNS (DNS over HTTPS) resolvers: (id, name, DoH URL, bootstrap addresses).
pub(super) const DNS_PROVIDERS: &[(&str, &str, &str, &[&str])] = &[
    ("cloudflare", "Cloudflare", "https://cloudflare-dns.com/dns-query", &["1.1.1.1", "1.0.0.1", "2606:4700:4700::1111", "2606:4700:4700::1001"]),
    ("cloudflare-security", "Cloudflare, block malware", "https://security.cloudflare-dns.com/dns-query", &["1.1.1.2", "1.0.0.2"]),
    ("google", "Google", "https://dns.google/dns-query", &["8.8.8.8", "8.8.4.4", "2001:4860:4860::8888", "2001:4860:4860::8844"]),
    ("quad9", "Quad9", "https://dns.quad9.net/dns-query", &["9.9.9.9", "149.112.112.112", "2620:fe::fe", "2620:fe::9"]),
    ("adguard", "AdGuard, block ads", "https://dns.adguard-dns.com/dns-query", &["94.140.14.14", "94.140.15.15"]),
    ("mullvad", "Mullvad", "https://dns.mullvad.net/dns-query", &["194.242.2.2"]),
    ("custom", "Custom (DoH URL, e.g. NextDNS)", "", &[]),
];

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn uuid() -> String {
    // RFC 4122 version 4: macOS keys the network configuration by this UUID.
    let mut b = profiles::random_bytes::<16>();
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h = profiles::hex(&b).to_uppercase();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

impl Browser {
    // ---------- profile ----------

    /// Leaves the sign-up / lock screen: restores tabs and panels.
    pub(super) fn enter(&mut self) {
        if let Some(a) = self.ws().active {
            self.show(a);
        }
        if self.ai_open {
            self.ensure_ai();
        }
        if self.chrome_ready && std::mem::take(&mut self.palette_on_ready) {
            self.open_palette("new");
        }
        self.layout();
        self.dirty();
    }

    /// Locks a password-protected profile: every page is torn down until unlocked.
    pub(super) fn lock(&mut self) {
        let protected = profiles::Registry::load().get(&self.profile.id).is_some_and(|p| p.password.is_some());
        if !protected {
            self.open_settings();
            return self.set_toast("Set a password in Settings to lock this profile", 4);
        }
        self.save_session();
        let live: Vec<Id> = self.tabs.iter().filter(|t| t.view.is_some()).map(|t| t.d.id).collect();
        live.into_iter().for_each(|id| self.unload(id));
        if let Some(v) = self.ai_view.take() {
            platform::release(&v);
        }
        self.set_overlay(false);
        self.gate = Some("lock");
        self.layout();
        self.dirty();
    }

    /// Profiles are fully separate processes: relaunch into the other one.
    pub(super) fn switch_profile(&mut self, id: &str) {
        let mut reg = profiles::Registry::load();
        reg.last = Some(id.to_string());
        reg.save();
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("--profile").arg(id).spawn();
        }
        self.quit = true;
    }

    pub(super) fn profile_menu(&self, x: f64, y: f64) {
        let reg = profiles::Registry::load();
        let m = Menu::new();
        for p in &reg.profiles {
            let name = if p.name.is_empty() { "Unnamed" } else { &p.name };
            let _ = m.append(&CheckMenuItem::with_id(format!("profile.switch:{}", p.id), name, true, p.id == self.profile.id, None));
        }
        let _ = m.append_items(&[
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("new_profile", "New Profile…", true, None),
            &MenuItem::with_id("lock", "Lock Profile", true, None),
            &MenuItem::with_id("settings", "Profile Settings…", true, None),
        ]);
        platform::show_menu(&self.sh.window, &m, x, y);
    }

    pub(super) fn profile_json(&self) -> (Value, Vec<Value>) {
        let reg = profiles::Registry::load();
        let me = reg.get(&self.profile.id).cloned().unwrap_or_default();
        let current = json!({ "id": me.id, "name": me.name, "initial": me.initial(), "locked": me.password.is_some() });
        let all = reg.profiles.iter().map(|p| json!({ "id": p.id, "name": p.name, "initial": p.initial() })).collect();
        (current, all)
    }

    pub(super) fn mcp_command(&self) -> String {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "rzen".into());
        let profile = if self.profile.id == profiles::DEFAULT_ID { String::new() } else { format!(" --profile {}", self.profile.id) };
        format!("claude mcp add rzen -- \"{exe}\" mcp{profile}")
    }

    // ---------- AI panel ----------

    fn provider(&self) -> &'static (&'static str, &'static str, &'static str, Option<&'static str>) {
        AI_PROVIDERS.iter().find(|p| p.0 == self.settings.ai_provider).unwrap_or(&AI_PROVIDERS[0])
    }

    pub(super) fn ensure_ai(&mut self) {
        // rzen AI is drawn by the chrome itself: no web page, no extra process.
        if self.ai_view.is_some() || self.provider().0 == "assistant" {
            return;
        }
        let store = profiles::store_id(&self.profile.id, None);
        match build_tab_view(&self.sh, AI_ID, Some(self.provider().2), store, |b| b) {
            Ok(v) => {
                let _ = v.set_bounds(self.ai_rect());
                let _ = v.set_visible(true);
                self.ai_view = Some(v);
            }
            Err(e) => eprintln!("rzen: AI panel failed: {e}"),
        }
    }

    /// Closing drops the panel's page (and its process); chats live in your account.
    pub(super) fn set_ai(&mut self, open: bool) {
        if self.gate.is_some() {
            return;
        }
        self.ai_open = open;
        if open {
            self.ensure_ai();
        } else if let Some(v) = self.ai_view.take() {
            platform::release(&v);
        }
        self.layout();
        self.dirty();
    }

    /// "Ask about this page": opens the provider with a prompt about the visible tab.
    pub(super) fn ai_ask(&mut self, selection: &str) {
        let Some(t) = self.active_id().and_then(|id| self.tab(id)) else { return };
        let mut prompt = format!("Help me with this page: \"{}\" — {}", t.d.title, t.current_url());
        if !selection.trim().is_empty() {
            prompt.push_str(&format!("\n\nSelected text:\n{}", selection.trim()));
        }
        let provider = self.provider();
        self.set_ai(true);
        if provider.0 == "assistant" {
            return self.js("__rzen.aiPrompt('Summarize this page and point out anything important.')");
        }
        match provider.3 {
            Some(template) => {
                if let Some(v) = &self.ai_view {
                    let _ = v.load_url(&util::search_url(&prompt, template));
                }
            }
            None => {
                platform::copy_text(&prompt);
                self.set_toast(format!("Prompt copied. Paste it into {}.", provider.1), 4);
            }
        }
    }

    /// Address-bar context menu.
    pub(super) fn url_menu(&self, x: f64, y: f64) {
        let has_page = self.active_id().and_then(|i| self.tab(i)).is_some_and(|t| t.d.url.starts_with("http"));
        let it = |id: &str, label: &str, on: bool| MenuItem::with_id(id, label, on, None);
        let sep = PredefinedMenuItem::separator();
        let m = Menu::new();
        let _ = m.append_items(&[
            &it("copy_url", "Copy Link", has_page),
            &it("url.copy_md", "Copy as Markdown", has_page),
        ]);
        if platform::SHARE {
            let _ = m.append(&it("url.share", "Share…", has_page));
        }
        let _ = m.append_items(&[
            &sep,
            &it("open_location", "Edit Address…", true),
            &it("reload", "Reload Page", has_page),
            &it("url.safari", if cfg!(target_os = "macos") { "Open in Safari" } else { "Open in Default Browser" }, has_page),
            &sep,
            &it("ai.ask", "Ask AI About This Page", has_page),
        ]);
        platform::show_menu(&self.sh.window, &m, x, y);
    }

    pub(super) fn share_page(&self, rect: (f64, f64, f64, f64)) {
        if let Some(url) = self.active_id().and_then(|i| self.tab(i)).map(Tab::current_url).filter(|u| u.starts_with("http")) {
            platform::share(&self.sh.window, &url, rect);
        }
    }

    pub(super) fn ai_menu(&self, x: f64, y: f64) {
        let m = Menu::new();
        for (i, p) in AI_PROVIDERS.iter().enumerate() {
            if i == 1 {
                let _ = m.append(&PredefinedMenuItem::separator());
            }
            let _ = m.append(&CheckMenuItem::with_id(format!("ai.provider:{}", p.0), p.1, true, p.0 == self.settings.ai_provider, None));
        }
        let _ = m.append_items(&[&PredefinedMenuItem::separator(), &MenuItem::with_id("settings", "AI Settings…", true, None)]);
        platform::show_menu(&self.sh.window, &m, x, y);
    }

    /// rzen AI's tool calls run through the same browser API agents use.
    pub(super) fn assistant_tool(&mut self, call: String, method: String, params: Value) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.on_agent(AgentCall { method, params, reply: tx });
        let proxy = self.sh.proxy.clone();
        std::thread::spawn(move || {
            let result = rx.recv_timeout(std::time::Duration::from_secs(70)).unwrap_or_else(|_| Err("timed out".into()));
            let _ = proxy.send_event(UserEvent::ToolResult(call, result));
        });
    }

    // ---------- command palette ----------

    /// Runs a command-palette entry. Most ids are menu ids; a few are palette-only.
    pub(super) fn run_command(&mut self, id: &str) {
        if let Some(ws) = id.strip_prefix("ws:").and_then(|s| s.parse::<Id>().ok()) {
            return self.switch_ws(ws);
        }
        if let Some(section) = id.strip_prefix("settings:") {
            self.settings_focus = Some(section.to_string());
            self.open_settings();
            return self.focus_settings(section);
        }
        if let Some(page) = id.strip_prefix("page:") {
            return self.open_internal(&format!("rzen://{page}"));
        }
        if let Some(theme) = id.strip_prefix("theme:") {
            self.settings.theme = theme.to_string();
            self.save_settings();
            self.apply_settings();
            return self.set_toast(format!("Theme: {theme}"), 2);
        }
        match id {
            "boost_site" => {
                let host = self.active_id().and_then(|id| self.tab(id)).filter(|t| !t.is_internal()).and_then(|t| crate::boosts::clean_host(&t.current_url()));
                let open = self.tabs.iter().any(|t| t.d.url == BOOSTS_URL && t.view.is_some());
                self.open_internal(BOOSTS_URL);
                if let Some(host) = host {
                    if open {
                        self.internal_js(&format!("window.__boosts&&__boosts.create({})", json!(host)));
                    } else {
                        self.boost_intent = Some(host);
                    }
                }
            }
            "toggle_mini_player" => {
                self.settings.mini_player = !self.settings.mini_player;
                self.save_settings();
                self.sync_mini();
                self.ui_dirty = true;
                self.set_toast(if self.settings.mini_player { "Mini player on" } else { "Mini player off" }, 2);
            }
            "toggle_trackers" => {
                self.settings.block_trackers = !self.settings.block_trackers;
                self.save_settings();
                self.apply_settings();
                let on = self.settings.block_trackers;
                self.set_toast(if on { "Tracker blocking on" } else { "Tracker blocking off" }, 2);
            }
            "clear_history" => {
                self.history.clear();
                self.set_toast("History cleared", 2);
            }
            "clear_cache" => {
                let p = self.sh.proxy.clone();
                platform::clear_cache(profiles::store_id(&self.profile.id, None), move || {
                    let _ = p.send_event(UserEvent::NetDone("Page cache cleared".into()));
                });
            }
            "speed_test" => self.open_internal(SPEED_URL),
            "flush_dns" => self.flush_dns(),
            "url.share" => {
                // Anchor the share sheet under the address bar.
                self.share_anchor = (8.0, 46.0, self.sidebar_w - 16.0, 34.0);
                self.on_menu(id);
            }
            other => self.on_menu(other),
        }
    }

    // ---------- private windows ----------

    /// Opens a private window: a separate rzen with WebKit's in-memory data store (no
    /// cookies, cache or storage on disk), no history or icons kept, and a throwaway folder
    /// that's deleted when it closes. Your settings and Boosts come along.
    pub(super) fn open_private_window(&mut self) {
        let dir = std::env::temp_dir().join(format!("rzen-private-{}", profiles::hex(&profiles::random_bytes::<6>())));
        if std::fs::create_dir_all(&dir).is_err() {
            return self.set_toast("Couldn't open a private window", 3);
        }
        for f in ["settings.json", "boosts.json"] {
            let _ = std::fs::copy(self.dir.join(f), dir.join(f));
        }
        let spawned = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg("--private-dir").arg(&dir).env("RZEN_PRIVATE", "1").spawn());
        if spawned.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            self.set_toast("Couldn't open a private window", 3);
        }
    }

    // ---------- boosts ----------

    /// Saves the Boosts and puts them on every open page: styles change right away, code
    /// on each page's next load.
    pub(super) fn boosts_changed(&mut self) {
        crate::boosts::save(&self.dir, &self.boosts);
        platform::set_page_scripts(crate::boosts::scripts(&self.boosts));
        let restyle = crate::boosts::restyle_js(&self.boosts);
        for v in self.tabs.iter().filter(|t| !t.is_internal()).filter_map(|t| t.view.as_ref()) {
            platform::apply_page_scripts(v);
            let _ = v.evaluate_script(&restyle);
        }
    }

    fn boosted_tabs(&self, id: u64) -> Vec<Id> {
        let Some(b) = self.boosts.iter().find(|b| b.id == id) else { return Vec::new() };
        let host_of = |u: &str| url::Url::parse(u).ok().and_then(|u| u.host_str().map(str::to_string));
        self.tabs.iter().filter(|t| !t.is_internal() && host_of(&t.current_url()).is_some_and(|h| b.matches(&h))).map(|t| t.d.id).collect()
    }

    /// Reloads the open pages a boost applies to, so its code runs.
    pub(super) fn reload_boosted(&mut self, id: u64) {
        let ids = self.boosted_tabs(id);
        for v in ids.iter().filter_map(|&t| self.tab(t)).filter_map(|t| t.view.as_ref()) {
            let _ = v.evaluate_script("location.reload()");
        }
        self.set_toast(if ids.is_empty() { "No open tabs for that site".to_string() } else { format!("Reloaded {} tab{}", ids.len(), if ids.len() == 1 { "" } else { "s" }) }, 3);
    }

    /// Shows a tab on the boost's site, or opens one.
    pub(super) fn open_boosted(&mut self, id: u64) {
        let Some(host) = self.boosts.iter().find(|b| b.id == id).map(|b| b.host.clone()) else { return };
        match self.boosted_tabs(id).into_iter().max_by_key(|&t| self.tab(t).map(|t| t.last_active)) {
            Some(t) => self.show(t),
            None => {
                self.open_tab(format!("https://{host}/"), None, true);
            }
        }
    }

    pub(super) fn focus_settings(&self, section: &str) {
        if let Some(v) = self.tabs.iter().find(|t| t.is_settings()).and_then(|t| t.view.as_ref()) {
            let _ = v.evaluate_script(&format!("__settings.focus&&__settings.focus({})", json!(section)));
        }
    }

    // ---------- docked video mini player ----------

    /// Docks the playing video's own page into the sidebar's video slot while you're on
    /// another tab (live, no copies), and undocks it when you return.
    pub(super) fn sync_mini(&mut self) {
        let target = self.media_tab.filter(|&id| {
            Some(id) != self.visible
                && self.settings.mini_player
                && self.sidebar_open
                && self.gate.is_none()
                && self.mini_rect.is_some()
                && self.tab(id).is_some_and(|t| t.view.is_some() && t.media.as_ref().is_some_and(|m| m["video"].as_bool() == Some(true)))
        });
        if self.mini_tab != target {
            if let Some(old) = self.mini_tab.take() {
                if let Some(v) = self.tab(old).and_then(|t| t.view.as_ref()) {
                    let _ = v.evaluate_script(MINI_OFF_JS);
                    if self.visible != Some(old) {
                        let _ = v.set_visible(false);
                    }
                }
            }
            if let Some(v) = target.and_then(|id| self.tab(id)).and_then(|t| t.view.as_ref()) {
                let _ = v.evaluate_script(MINI_ON_JS);
            }
            self.mini_tab = target;
        }
        if debug() {
            eprintln!("mini: media={:?} visible={:?} sidebar={} rect={:?} -> docked={:?}", self.media_tab, self.visible, self.sidebar_open, self.mini_rect, self.mini_tab);
        }
        if let (Some(v), Some((x, y, w, h))) = (self.mini_tab.and_then(|id| self.tab(id)).and_then(|t| t.view.as_ref()), self.mini_rect) {
            let _ = v.set_bounds(rect(x, y, w, h));
            let _ = v.set_visible(true);
        }
    }

    // ---------- network ----------

    /// New proxy/VPN route: rebuild pages so every request uses it.
    pub(super) fn apply_proxy(&mut self) {
        let wanted = self.settings.proxy.clone();
        if *self.sh.proxy_url.borrow() == wanted {
            return;
        }
        *self.sh.proxy_url.borrow_mut() = wanted.clone();
        if wanted.is_empty() {
            platform::clear_proxy(profiles::store_id(&self.profile.id, None));
        }
        let shown = self.visible;
        let live: Vec<Id> = self.tabs.iter().filter(|t| t.view.is_some()).map(|t| t.d.id).collect();
        live.into_iter().for_each(|id| self.unload(id));
        if let Some(v) = self.ai_view.take() {
            platform::release(&v);
            self.ensure_ai();
        }
        if let Some(id) = shown {
            self.show(id);
        }
        if !self.exiting && self.chrome_ready {
            self.set_toast(if wanted.is_empty() { "Proxy off: connecting directly" } else { "Pages now go through your proxy" }, 3);
        }
    }

    /// Apple's own network test (`networkQuality`): throughput both ways and responsiveness.
    pub(super) fn speed_test(&self) {
        let p = self.sh.proxy.clone();
        std::thread::spawn(move || {
            let res = std::process::Command::new("/usr/bin/networkQuality").arg("-c").output().map_err(|e| e.to_string()).and_then(|o| {
                if o.status.success() {
                    serde_json::from_slice::<Value>(&o.stdout).map_err(|e| e.to_string())
                } else {
                    Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
                }
            });
            let _ = p.send_event(UserEvent::Speed(res));
        });
    }

    /// Flushes the system DNS cache (macOS asks for an administrator password).
    pub(super) fn flush_dns(&self) {
        let p = self.sh.proxy.clone();
        std::thread::spawn(move || {
            #[cfg(target_os = "macos")]
            let ok = std::process::Command::new("osascript")
                .args(["-e", r#"do shell script "dscacheutil -flushcache; killall -HUP mDNSResponder" with administrator privileges"#])
                .output()
                .is_ok_and(|o| o.status.success());
            #[cfg(not(target_os = "macos"))]
            let ok = std::process::Command::new("ipconfig").arg("/flushdns").output().is_ok_and(|o| o.status.success());
            let _ = p.send_event(UserEvent::NetDone(if ok { "DNS cache cleared".into() } else { "DNS cache wasn't cleared".into() }));
        });
    }

    /// Points this Mac's DNS at a provider (or back to automatic) on every hardware network
    /// service, after the macOS password prompt. Plain DNS, but it always works, unlike
    /// configuration profiles, which macOS 26 often refuses ("VPN Service" error).
    pub(super) fn set_system_dns(&mut self, provider: &str) {
        let (name, ips): (&str, &[&str]) = match DNS_PROVIDERS.iter().find(|d| d.0 == provider) {
            _ if provider == "automatic" => ("automatic", &[]),
            Some(d) if !d.3.is_empty() => (d.1, d.3),
            _ => return self.set_toast("That provider has no server addresses to set", 4),
        };
        if !cfg!(target_os = "macos") {
            return self.set_toast("Setting the system DNS is only available on macOS", 4);
        }
        let servers = if ips.is_empty() { "Empty".to_string() } else { ips.join(" ") };
        // Only real interfaces (Wi-Fi, Ethernet, Thunderbolt…): VPN services keep their own DNS.
        let script = format!(
            r#"ports=$(networksetup -listallhardwareports | sed -n 's/^Hardware Port: //p'); networksetup -listallnetworkservices | tail -n +2 | grep -v '^\*' | while IFS= read -r s; do printf '%s\n' "$ports" | grep -qxF "$s" && networksetup -setdnsservers "$s" {servers}; done; dscacheutil -flushcache; killall -HUP mDNSResponder"#
        );
        let apple = format!("do shell script \"{}\" with administrator privileges", script.replace('\\', "\\\\").replace('"', "\\\""));
        let msg = if ips.is_empty() { "DNS set back to automatic".to_string() } else { format!("This Mac now uses {name} DNS") };
        let p = self.sh.proxy.clone();
        std::thread::spawn(move || {
            let ok = std::process::Command::new("osascript").args(["-e", &apple]).output().is_ok_and(|o| o.status.success());
            let _ = p.send_event(UserEvent::NetDone(if ok { msg } else { "DNS wasn't changed".into() }));
        });
    }

    /// Encrypted DNS for the whole Mac: a DNS-over-HTTPS configuration profile the user
    /// approves once in System Settings (Apple only allows resolver choice system-wide).
    pub(super) fn install_dns(&mut self, provider: &str, custom: Option<&str>) {
        let Some(&(id, name, url, ips)) = DNS_PROVIDERS.iter().find(|d| d.0 == provider) else { return };
        let url = if id == "custom" { custom.unwrap_or_default().trim() } else { url };
        if !url.starts_with("https://") {
            return self.set_toast("Enter a DNS-over-HTTPS address starting with https://", 4);
        }
        let name = if id == "custom" { util::short_host(url).to_string() } else { name.to_string() };
        let addresses = if ips.is_empty() {
            String::new()
        } else {
            format!("<key>ServerAddresses</key><array>{}</array>", ips.iter().map(|ip| format!("<string>{ip}</string>")).collect::<String>())
        };
        let (n, u) = (xml(&name), xml(url));
        let profile = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>PayloadContent</key><array><dict>
    <key>DNSSettings</key><dict><key>DNSProtocol</key><string>HTTPS</string><key>ServerURL</key><string>{u}</string>{addresses}</dict>
    <key>PayloadDisplayName</key><string>{n} encrypted DNS</string>
    <key>PayloadIdentifier</key><string>app.rzen.dns.settings</string>
    <key>PayloadType</key><string>com.apple.dnsSettings.managed</string>
    <key>PayloadUUID</key><string>{}</string><key>PayloadVersion</key><integer>1</integer>
  </dict></array>
  <key>PayloadDescription</key><string>Encrypts this Mac's DNS lookups with {n} (DNS over HTTPS). Created by rzen. Remove it any time in System Settings.</string>
  <key>PayloadDisplayName</key><string>Encrypted DNS: {n}</string>
  <key>PayloadIdentifier</key><string>app.rzen.dns</string>
  <key>PayloadRemovalDisallowed</key><false/>
  <key>PayloadScope</key><string>System</string>
  <key>PayloadType</key><string>Configuration</string>
  <key>PayloadUUID</key><string>{}</string><key>PayloadVersion</key><integer>1</integer>
</dict></plist>
"#,
            uuid(),
            uuid()
        );
        let path = self.dir.join("encrypted-dns.mobileconfig");
        if std::fs::write(&path, profile).is_err() {
            return self.set_toast("Couldn't create the DNS profile", 4);
        }
        util::open_with_os(&path.to_string_lossy());
        #[cfg(target_os = "macos")]
        util::open_with_os("x-apple.systempreferences:com.apple.Profiles-Settings.extension");
        self.set_toast(format!("Approve \"Encrypted DNS: {name}\" in System Settings"), 6);
    }

    /// Current resolvers and VPN state, as macOS reports them.
    pub(super) fn net_info(&self) {
        let p = self.sh.proxy.clone();
        let proxy = self.settings.proxy.clone();
        std::thread::spawn(move || {
            let run = |cmd: &str, args: &[&str]| {
                std::process::Command::new(cmd).args(args).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
            };
            let dns_out = run("/usr/sbin/scutil", &["--dns"]);
            let mut dns: Vec<String> = Vec::new();
            for line in dns_out.lines().map(str::trim).filter(|l| l.starts_with("nameserver[")) {
                if let Some((_, ip)) = line.split_once(':') {
                    let ip = ip.trim().to_string();
                    if !dns.contains(&ip) {
                        dns.push(ip);
                    }
                }
                if dns.len() >= 3 {
                    break;
                }
            }
            let encrypted = run("/usr/sbin/scutil", &["--dns"]).contains("DNS over HTTPS") || dns_out.contains("doh");
            let vpn = run("/usr/sbin/scutil", &["--nc", "list"])
                .lines()
                .find(|l| l.contains("(Connected)"))
                .and_then(|l| l.split('"').nth(1).map(str::to_string));
            let _ = p.send_event(UserEvent::NetInfo(json!({ "dns": dns, "encrypted": encrypted, "vpn": vpn, "proxy": proxy })));
        });
    }

    // ---------- media ----------

    /// Cheap (no JavaScript): reads WebKit's own "playing audio" state for live tabs.
    pub(super) fn poll_media(&mut self) {
        let mut changed = false;
        for i in 0..self.tabs.len() {
            let Some(audible) = self.tabs[i].view.as_ref().map(platform::is_playing_audio) else { continue };
            if audible == self.tabs[i].audible {
                continue;
            }
            changed = true;
            self.tabs[i].audible = audible;
            if !audible {
                continue;
            }
            let id = self.tabs[i].d.id;
            self.media_tab = Some(id);
            let p = self.sh.proxy.clone();
            if let Some(v) = &self.tabs[i].view {
                let _ = v.evaluate_script_with_callback(MEDIA_INFO_JS, move |r| {
                    if let Ok(info) = serde_json::from_str::<Value>(&r) {
                        let _ = p.send_event(UserEvent::MediaInfo(id, info));
                    }
                });
            }
        }
        if changed {
            self.ui_dirty = true;
            self.sync_mini();
        }
        // Playback position for the mini player's progress bar (one tiny script, 1×/s).
        if let Some(id) = self.media_tab {
            if let Some(v) = self.tab(id).and_then(|t| t.view.as_ref()) {
                let p = self.sh.proxy.clone();
                let _ = v.evaluate_script_with_callback(MEDIA_TIME_JS, move |r| {
                    if let Some(t) = serde_json::from_str::<Value>(&r).ok().filter(Value::is_array) {
                        let _ = p.send_event(UserEvent::MediaTime(id, t));
                    }
                });
            }
        }
    }

    pub(super) fn media_js(&mut self, script: &str) {
        if let Some(v) = self.media_tab.and_then(|id| self.tab(id)).and_then(|t| t.view.as_ref()) {
            let _ = v.evaluate_script(script);
        }
        self.media_next = Instant::now();
    }

    pub(super) fn toggle_mute(&mut self, id: Id) {
        let Some(i) = self.idx(id) else { return };
        let t = &mut self.tabs[i];
        let Some(v) = &t.view else { return };
        t.muted = !t.muted;
        if !platform::set_muted(v, t.muted) {
            let _ = v.evaluate_script(&format!("{MEDIA_MUTE_JS}({})", t.muted));
        }
        self.ui_dirty = true;
    }

    pub(super) fn media_json(&self) -> Value {
        let Some(t) = self.media_tab.filter(|_| self.settings.mini_player).and_then(|id| self.tab(id)).filter(|t| t.view.is_some()) else { return Value::Null };
        let info = t.media.clone().unwrap_or(Value::Null);
        json!({
            "id": t.d.id,
            "title": info["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(&t.d.title),
            "artist": info["artist"].as_str().filter(|s| !s.is_empty()).map_or_else(|| util::short_host(&t.d.url).to_string(), str::to_string),
            "art": info["art"].as_str().filter(|s| !s.is_empty()).or(t.d.favicon.as_deref()),
            "playing": t.audible,
            "muted": t.muted,
            "video": info["video"].as_bool().unwrap_or(false),
            "background": self.visible != Some(t.d.id),
        })
    }

    // ---------- AI-agent API ----------

    pub(super) fn on_agent(&mut self, call: AgentCall) {
        // A locked (or not yet set up) profile exposes nothing to agents.
        if self.gate.is_some() {
            let _ = call.reply.send(Err("rzen is locked. Unlock the profile first.".into()));
            return;
        }
        self.agent_seen = Some(Instant::now());
        self.ui_dirty = true;
        let AgentCall { method, params: p, reply } = call;
        let target = p.get("tab_id").and_then(Value::as_u64).or(self.active_id());
        let url = |s: &Self| p["url"].as_str().map(|u| util::resolve_input(u, s.settings.search_template()));
        let res: Result<Value, String> = match method.as_str() {
            "tabs.list" => Ok(json!(self
                .tabs
                .iter()
                .map(|t| json!({
                    "tab_id": t.d.id, "title": t.d.title, "url": t.current_url(),
                    "visible": self.visible == Some(t.d.id), "loaded": t.view.is_some(),
                    "pinned": t.d.pinned, "essential": t.d.essential,
                }))
                .collect::<Vec<_>>())),
            "tabs.open" => match url(self) {
                Some(u) => {
                    let bg = p["background"].as_bool().unwrap_or(false);
                    Ok(json!({ "tab_id": self.open_tab(u, None, !bg) }))
                }
                None => Err("url is required".into()),
            },
            "tabs.activate" | "tabs.close" => match target.filter(|&id| self.idx(id).is_some()) {
                Some(id) => {
                    if method == "tabs.close" { self.close(id) } else { self.show(id) }
                    Ok(json!({ "ok": true }))
                }
                None => Err("no such tab".into()),
            },
            "page.navigate" => match (target.filter(|&id| self.idx(id).is_some()), url(self)) {
                (Some(id), Some(u)) => {
                    self.show(id);
                    self.navigate(id, u);
                    Ok(json!({ "tab_id": id }))
                }
                (None, Some(u)) => Ok(json!({ "tab_id": self.open_tab(u, None, true) })),
                _ => Err("url is required".into()),
            },
            "page.history" => match self.agent_view(target) {
                Some(v) => {
                    let _ = match p["action"].as_str().unwrap_or_default() {
                        "back" => v.go_back(),
                        "forward" => v.go_forward(),
                        _ => v.reload(),
                    };
                    Ok(json!({ "ok": true }))
                }
                None => Err("no such tab".into()),
            },
            "page.wait" => {
                let Some(id) = target else { return drop(reply.send(Err("no tab".into()))) };
                let ms = p["timeout_ms"].as_u64().unwrap_or(15_000).min(60_000);
                let now = Instant::now();
                self.waiters.push((id, now, now + std::time::Duration::from_millis(ms), reply));
                return;
            }
            "page.screenshot" => {
                let Some(v) = self.agent_view(target) else { return drop(reply.send(Err("no such tab".into()))) };
                platform::snapshot(v, move |png| {
                    let _ = reply.send(png.map(|b| json!({ "png_base64": util::base64(&b) })).ok_or_else(|| "screenshot failed".to_string()));
                });
                return;
            }
            "page.text" | "page.links" | "page.click" | "page.type" | "page.eval" => {
                let script = agent_script(&method, &p);
                let Some(v) = self.agent_view(target) else { return drop(reply.send(Err("no such tab".into()))) };
                let _ = v.evaluate_script_with_callback(&script, move |r| {
                    let value: Value = serde_json::from_str(&r).unwrap_or(Value::Null);
                    let out = match value.get("error").and_then(Value::as_str) {
                        Some(e) => Err(e.to_string()),
                        None => Ok(value),
                    };
                    let _ = reply.send(out);
                });
                return;
            }
            other => Err(format!("unknown method {other}")),
        };
        let _ = reply.send(res);
    }

    /// The tab's page, shown first: WebKit suspends hidden pages, so scripts there would
    /// never answer (and the user should see what an agent is doing anyway).
    fn agent_view(&mut self, target: Option<Id>) -> Option<&WebView> {
        let id = target.filter(|&id| self.idx(id).is_some())?;
        if self.visible != Some(id) {
            self.show(id);
        }
        self.tab(id).and_then(|t| t.view.as_ref())
    }

    pub(super) fn check_waiters(&mut self, now: Instant) {
        if self.waiters.is_empty() {
            return;
        }
        let tabs = &self.tabs;
        self.waiters.retain(|(id, started, deadline, reply)| {
            let t = tabs.iter().find(|t| t.d.id == *id);
            // A click/navigate issued just before the wait hasn't reported "started" yet:
            // give it a moment before trusting "not loading".
            let settled = now.duration_since(*started) >= std::time::Duration::from_millis(400);
            let done = settled && t.is_none_or(|t| !t.loading);
            if done || now >= *deadline {
                let state = json!({ "loaded": done, "url": t.map(Tab::current_url), "title": t.map(|t| t.d.title.clone()) });
                let _ = reply.send(Ok(state));
                false
            } else {
                true
            }
        });
    }
}

/// Page-side scripts for agent calls. Every result is JSON; failures come back as `{error}`.
fn agent_script(method: &str, p: &Value) -> String {
    let arg = |k: &str| json!(p.get(k).cloned().unwrap_or(Value::Null)).to_string();
    match method {
        "page.text" => format!(
            "(()=>({{title:document.title,url:location.href,text:(document.body?document.body.innerText:'').slice(0,{})}}))()",
            p["max_chars"].as_u64().unwrap_or(40_000)
        ),
        "page.links" => "[...document.links].slice(0,500).map(a=>({text:(a.innerText||a.title||'').trim().slice(0,120),href:a.href}))".into(),
        "page.click" => format!(
            r#"((sel,text)=>{{let el=sel?document.querySelector(sel):null;if(!el&&text){{const t=text.toLowerCase();el=[...document.querySelectorAll('a,button,[role=button],input[type=submit],input[type=button],[onclick]')].find(e=>((e.innerText||e.value||e.getAttribute('aria-label')||'').trim().toLowerCase()).includes(t))}}if(!el)return {{error:'element not found'}};el.scrollIntoView({{block:'center'}});el.click();return {{clicked:(el.innerText||el.value||el.tagName).trim().slice(0,80)}}}})({},{})"#,
            arg("selector"),
            arg("text")
        ),
        "page.type" => format!(
            r#"((sel,text,submit)=>{{const el=document.querySelector(sel);if(!el)return {{error:'element not found'}};el.focus();if(el.isContentEditable){{document.execCommand('insertText',false,text)}}else{{const d=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el),'value');const v=(el.value||'')+text;d&&d.set?d.set.call(el,v):el.value=v;el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}))}}if(submit){{const f=el.form;if(f){{f.requestSubmit?f.requestSubmit():f.submit()}}else{{for(const t of ['keydown','keypress','keyup'])el.dispatchEvent(new KeyboardEvent(t,{{key:'Enter',code:'Enter',keyCode:13,which:13,bubbles:true}}))}}}}return {{typed:text.length}}}})({},{},{})"#,
            arg("selector"),
            arg("text"),
            p["submit"].as_bool().unwrap_or(false)
        ),
        _ => format!(
            "(()=>{{try{{const r=(0,eval)({});return r===undefined?null:JSON.parse(JSON.stringify(r))}}catch(e){{return {{error:String(e)}}}}}})()",
            arg("script")
        ),
    }
}
