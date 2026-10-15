//! rzen — a fast, memory-lean, Zen-style browser.
//!
//! Pages render in the OS engine (WKWebView = Safari's WebKit on macOS, WebView2 on
//! Windows), so rzen adds almost nothing on top of the engine the OS already keeps warm.

/// `log!` that never panics. Rust's own panics when stderr is gone (rzen started from a
/// terminal that has since closed, or a log reader that stopped), and that crashed the app.
macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod agent;
mod boosts;
mod browser;
mod dns;
mod favicons;
mod filters;
mod history;
mod mcp;
mod menu;
mod model;
mod perf;
mod platform;
mod profiles;
mod shields;
mod util;

use browser::{Browser, ProfileCtx, UserEvent};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
};

/// Picks the profile to run: `--profile <id>`, else the last used one. First launch (no
/// profiles yet) shows the sign-up screen; password-protected profiles start locked.
fn choose_profile(requested: Option<&str>) -> ProfileCtx {
    let mut reg = profiles::Registry::load();
    if reg.profiles.is_empty() {
        // An install from before profiles keeps its data (and a name) as "Personal".
        let legacy = model::root_dir().join("session.json").exists();
        let name = if legacy { "Personal" } else { "" };
        reg.profiles.push(profiles::Profile { name: name.into(), ..Default::default() });
    }
    let id = requested
        .filter(|id| reg.get(id).is_some())
        .or(reg.last.as_deref().filter(|id| reg.get(id).is_some()))
        .unwrap_or(&reg.profiles[0].id)
        .to_string();
    reg.last = Some(id.clone());
    reg.save();
    let p = reg.get(&id).cloned().unwrap_or_default();
    let gate = if p.name.is_empty() {
        Some("setup")
    } else if p.password.is_some() {
        Some("lock")
    } else {
        None
    };
    ProfileCtx { dir: profiles::dir(&id), id, gate }
}

/// Release builds abort on panic; record why first, so crashes are never silent.
fn install_crash_log() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let line = format!(
            "--- {:?} rzen {}\n{info}\n{}\n",
            std::time::SystemTime::now(),
            env!("CARGO_PKG_VERSION"),
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(model::root_dir().join("crash.log")) {
            let _ = f.write_all(line.as_bytes());
        }
    }));
}

fn main() {
    install_crash_log();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let requested = args.windows(2).find(|w| w[0] == "--profile").map(|w| w[1].clone());
    if args.first().is_some_and(|a| a == "mcp") {
        let reg = profiles::Registry::load();
        let id = requested.or(reg.last).unwrap_or_else(|| profiles::DEFAULT_ID.into());
        std::process::exit(mcp::run(&profiles::dir(&id)));
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    platform::install_key_fixes();
    // A private window (see `Browser::open_private_window`) runs from a throwaway folder, with no profile.
    let private_dir = args.windows(2).find(|w| w[0] == "--private-dir").map(|w| std::path::PathBuf::from(&w[1]));
    let ctx = match private_dir {
        Some(dir) => ProfileCtx { id: browser::PRIVATE_ID.into(), dir, gate: None },
        None => choose_profile(requested.as_deref()),
    };
    let mut browser = match Browser::new(&event_loop, ctx) {
        Ok(b) => b,
        Err(e) => {
            log!("rzen: failed to start: {e}");
            std::process::exit(1);
        }
    };
    // `--load-all` loads every URL immediately (benchmarking); normally only the last one loads.
    let eager = args.iter().any(|a| a == "--load-all");
    let values: Vec<usize> = args.iter().enumerate().filter(|(_, a)| *a == "--profile" || *a == "--private-dir").map(|(i, _)| i + 1).collect();
    let urls = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !values.contains(i) && !a.starts_with("--") && (a.contains(':') || a.contains('.')))
        .map(|(_, a)| a.clone());
    browser.open_urls(urls, eager);

    event_loop.run(move |event, _, control_flow| match event {
        Event::UserEvent(e) => browser.on_event(e),
        Event::WindowEvent { event, .. } => match event {
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => browser.layout(),
            WindowEvent::Moved(_) => browser.touch(),
            WindowEvent::Focused(focused) => {
                if focused {
                    browser.reload_settings();
                }
                browser.layout();
            }
            WindowEvent::CloseRequested => {
                browser.shutdown();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        },
        Event::Opened { urls } => browser.open_urls(urls.into_iter().map(String::from), false),
        Event::MainEventsCleared => browser.flush(control_flow),
        Event::LoopDestroyed => browser.shutdown(),
        _ => {}
    });
}
