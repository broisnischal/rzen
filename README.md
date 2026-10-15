# rzen

A fast, memory-lean, Zen-style browser written in Rust.

Pages render in the engine the OS already ships and keeps warm: WKWebView (Safari's WebKit) on macOS, WebView2 on Windows. rzen adds a thin native shell on top instead of bundling its own engine.

## Features

- **Lean tabs.** A tab only owns a web view while it's in use. Restored tabs start unloaded, idle tabs are unloaded, and a hard LRU cap bounds how many stay live.
- **Workspaces and profiles.** Separate people or identities, each with its own tabs, history, settings and website data. Profiles can be password-locked.
- **Ad and tracker blocking.** EasyList, EasyPrivacy, Peter Lowe's and uBlock lists are converted to WebKit content rules and compiled natively, so no JavaScript runs per request.
- **Boosts.** Your own CSS and JavaScript per site, applied as each page starts loading.
- **Command bar.** History search ranked by frecency; site icons cached on disk.
- **DNS tools.** Resolver benchmarks, lookups, system DNS switching and cache flush.
- **Performance panel.** Memory and CPU for the browser plus every WebKit helper process, matching Activity Monitor.
- **AI agents.** An opt-in local automation API (127.0.0.1 only, per-run bearer token) and an MCP server.

## Build

Requires Rust (edition 2024). macOS 13+ gets the full native integration; other platforms use portable fallbacks.

```sh
cargo run --release
```

To build `rzen.app` (needed for "Make default browser" and Dock integration):

```sh
./scripts/bundle-macos.sh
```

Passkeys need Apple's browser entitlement and a real signature; see the comments in `scripts/bundle-macos.sh`.

## Use with AI agents

Turn on Settings → AI agents, then register the MCP server, for example with Claude Code:

```sh
claude mcp add rzen -- /Applications/rzen.app/Contents/MacOS/rzen mcp
```
