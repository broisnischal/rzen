//! Local automation API for AI agents. Off by default (Settings → AI agents).
//!
//! Safety: binds 127.0.0.1 only; every request needs a random per-run bearer token kept in
//! the profile's `agent.json` (owner-readable only); requests carrying browser headers
//! (`Origin`, `Sec-Fetch-*`) or a foreign `Host` are refused, so no website can reach it.

use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};
use tao::event_loop::EventLoopProxy;

use crate::{browser::UserEvent, profiles};

const MAX_BODY: usize = 4 << 20;
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// One API call, executed on the UI thread; the answer goes back through `reply`.
pub struct AgentCall {
    pub method: String,
    pub params: Value,
    pub reply: mpsc::Sender<Result<Value, String>>,
}

pub struct AgentServer {
    stop: Arc<AtomicBool>,
    port: u16,
    file: PathBuf,
}

impl AgentServer {
    pub fn start(proxy: EventLoopProxy<UserEvent>, dir: &Path) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let token = profiles::hex(&profiles::random_bytes::<24>());
        let file = dir.join("agent.json");
        write_private(&file, json!({ "port": port, "token": token }).to_string().as_bytes())?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        thread::Builder::new().name("rzen-agent".into()).spawn(move || {
            for conn in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (proxy, token) = (proxy.clone(), token.clone());
                thread::spawn(move || {
                    let _ = serve(conn, &proxy, &token);
                });
            }
        })?;
        Ok(Self { stop, port, file })
    }
}

impl Drop for AgentServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the blocking accept so the thread sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        let _ = fs::remove_file(&self.file);
    }
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents)
}

fn serve(mut conn: TcpStream, proxy: &EventLoopProxy<UserEvent>, token: &str) -> io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let (mut len, mut authorized, mut from_browser, mut local_host) = (0usize, false, false, false);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = value.parse().unwrap_or(0),
            "authorization" => authorized = value.strip_prefix("Bearer ").is_some_and(|t| t == token),
            "origin" | "sec-fetch-site" | "sec-fetch-mode" => from_browser = true,
            "host" => local_host = value.starts_with("127.0.0.1:") || value.starts_with("localhost:"),
            _ => {}
        }
    }
    let (status, out) = if from_browser || !local_host {
        (403, json!({ "error": "forbidden" }))
    } else if !authorized {
        (401, json!({ "error": "missing or wrong token" }))
    } else if !request_line.starts_with("POST /rpc ") {
        (404, json!({ "error": "use POST /rpc" }))
    } else if len > MAX_BODY {
        (413, json!({ "error": "request too large" }))
    } else {
        let mut body = vec![0; len];
        reader.read_exact(&mut body)?;
        dispatch(proxy, &body)
    };
    let body = out.to_string();
    write!(
        conn,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    )
}

fn dispatch(proxy: &EventLoopProxy<UserEvent>, body: &[u8]) -> (u16, Value) {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return (400, json!({ "error": e.to_string() })),
    };
    let (tx, rx) = mpsc::channel();
    let call = AgentCall {
        method: req["method"].as_str().unwrap_or_default().to_string(),
        params: req.get("params").cloned().unwrap_or_else(|| json!({})),
        reply: tx,
    };
    if proxy.send_event(UserEvent::Agent(call)).is_err() {
        return (503, json!({ "error": "browser is shutting down" }));
    }
    match rx.recv_timeout(CALL_TIMEOUT) {
        Ok(Ok(result)) => (200, json!({ "result": result })),
        Ok(Err(error)) => (200, json!({ "error": error })),
        Err(_) => (504, json!({ "error": "timed out waiting for the page" })),
    }
}

/// Client side, used by `rzen mcp`: one JSON-RPC call to a running browser.
pub fn call(dir: &Path, method: &str, params: Value) -> Result<Value, String> {
    let off = "rzen isn't running, or AI agent access is off (rzen → Settings → AI agents).";
    let info: Value = fs::read(dir.join("agent.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or(off)?;
    let port = info["port"].as_u64().ok_or(off)? as u16;
    let token = info["token"].as_str().ok_or(off)?;
    let body = json!({ "method": method, "params": params }).to_string();
    let mut conn = TcpStream::connect(("127.0.0.1", port)).map_err(|_| off.to_string())?;
    conn.set_read_timeout(Some(CALL_TIMEOUT + Duration::from_secs(5))).ok();
    write!(
        conn,
        "POST /rpc HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut raw = String::new();
    conn.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let json = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bad response: {e}"))?;
    match v.get("error") {
        Some(e) => Err(e.as_str().map_or_else(|| e.to_string(), str::to_string)),
        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
    }
}
