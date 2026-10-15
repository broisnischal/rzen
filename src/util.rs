//! URL-bar input handling.

/// Turns what the user typed into a URL: a real URL, a bare domain, or a search.
pub fn resolve_input(input: &str, search_engine: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return "about:blank".into();
    }
    if looks_like_url(s) {
        return if has_scheme(s) { s.to_string() } else if is_local(host_of(s)) { format!("http://{s}") } else { format!("https://{s}") };
    }
    search_url(s, search_engine)
}

pub fn search_url(query: &str, search_engine: &str) -> String {
    let q: String = url::form_urlencoded::byte_serialize(query.trim().as_bytes()).collect();
    search_engine.replace("{}", &q)
}

pub fn looks_like_url(s: &str) -> bool {
    if s.contains(char::is_whitespace) {
        return false;
    }
    if has_scheme(s) {
        return true;
    }
    let host = host_of(s);
    if is_local(host) {
        return true;
    }
    let mut labels = host.split('.');
    let tld = host.rsplit('.').next().unwrap_or("");
    host.contains('.')
        && labels.all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
}

fn has_scheme(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    ["http://", "https://", "file://", "about:", "data:", "view-source:", "mailto:", "rzen://"].iter().any(|p| lower.starts_with(p))
}

/// Host part of a scheme-less input, without port.
fn host_of(s: &str) -> &str {
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    match host.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

fn is_local(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Lowercased host without `www.`, used for display and autocomplete.
pub fn short_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host)
}

/// App-link schemes handed to the OS (Mail, FaceTime, Zoom, Slack, …).
const EXTERNAL_SCHEMES: &[&str] = &[
    "mailto", "tel", "sms", "facetime", "facetime-audio", "maps", "webcal", "itms-apps", "itms-appss", "macappstore",
    "zoommtg", "zoomus", "msteams", "slack", "spotify", "discord", "vscode", "cursor", "figma", "notion", "linear",
];

/// Decides what the web view may load. Returns true when the navigation must be cancelled:
/// known app links go to the OS, any other non-web scheme is dropped so a page can't
/// launch arbitrary apps.
pub fn open_externally(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    if matches!(scheme.as_str(), "http" | "https" | "about" | "blob" | "data" | "file" | "javascript" | "") {
        return false;
    }
    if EXTERNAL_SCHEMES.contains(&scheme.as_str()) {
        open_with_os(url);
    }
    true
}

/// "socks5://host:port" / "http://host:port" (scheme optional, defaults to HTTP CONNECT).
pub fn parse_proxy(spec: &str) -> Option<wry::ProxyConfig> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let (socks, rest) = match spec.split_once("://") {
        Some((scheme, rest)) => (scheme.eq_ignore_ascii_case("socks5") || scheme.eq_ignore_ascii_case("socks"), rest),
        None => (false, spec),
    };
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':')?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return None;
    }
    let endpoint = wry::ProxyEndpoint { host: host.trim_matches(['[', ']']).to_string(), port: port.to_string() };
    Some(if socks { wry::ProxyConfig::Socks5(endpoint) } else { wry::ProxyConfig::Http(endpoint) })
}

pub fn open_with_os(target: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(target).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(target).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: &str = "https://s/?q={}";

    #[test]
    fn resolves() {
        assert_eq!(resolve_input("github.com", S), "https://github.com");
        assert_eq!(resolve_input("localhost:3000/x", S), "http://localhost:3000/x");
        assert_eq!(resolve_input("192.168.1.1", S), "http://192.168.1.1");
        assert_eq!(resolve_input("https://a.b/c d", S), "https://s/?q=https%3A%2F%2Fa.b%2Fc+d");
        assert_eq!(resolve_input("rust lang", S), "https://s/?q=rust+lang");
        assert_eq!(resolve_input("rust", S), "https://s/?q=rust");
        assert_eq!(resolve_input("file.txt", S), "https://file.txt");
        assert_eq!(resolve_input("v1.2", S), "https://s/?q=v1.2");
        assert_eq!(resolve_input("HTTPS://x.io", S), "HTTPS://x.io");
    }

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn hosts() {
        assert_eq!(short_host("https://www.github.com/a"), "github.com");
        assert_eq!(short_host("about:blank"), "about:blank");
    }
}
