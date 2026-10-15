//! `rzen mcp`: a Model Context Protocol server over stdio, so AI agents (Claude Code,
//! Claude Desktop, Cursor, …) can drive a running rzen through its local agent API.
//!
//!   claude mcp add rzen -- /Applications/rzen.app/Contents/MacOS/rzen mcp

use std::{
    io::{self, BufRead, Write},
    path::Path,
};

use serde_json::{Value, json};

use crate::agent;

struct Tool {
    name: &'static str,
    method: &'static str,
    description: &'static str,
    /// (name, type, description, required)
    params: &'static [(&'static str, &'static str, &'static str, bool)],
}

const TAB: (&str, &str, &str, bool) = ("tab_id", "integer", "Tab to act on (default: the visible tab)", false);

const TOOLS: &[Tool] = &[
    Tool { name: "browser_list_tabs", method: "tabs.list", description: "List open tabs with ids, titles, URLs and which one is visible.", params: &[] },
    Tool {
        name: "browser_open",
        method: "tabs.open",
        description: "Open a URL (or search terms) in a new tab.",
        params: &[("url", "string", "URL or search terms", true), ("background", "boolean", "Open without switching to it", false)],
    },
    Tool { name: "browser_switch_tab", method: "tabs.activate", description: "Show a tab.", params: &[("tab_id", "integer", "Tab id", true)] },
    Tool { name: "browser_close_tab", method: "tabs.close", description: "Close a tab.", params: &[("tab_id", "integer", "Tab id", true)] },
    Tool { name: "browser_navigate", method: "page.navigate", description: "Load a URL in a tab.", params: &[("url", "string", "URL or search terms", true), TAB] },
    Tool {
        name: "browser_history",
        method: "page.history",
        description: "Go back, forward, or reload.",
        params: &[("action", "string", "back | forward | reload", true), TAB],
    },
    Tool {
        name: "browser_read",
        method: "page.text",
        description: "Read the page's visible text, title and URL.",
        params: &[("max_chars", "integer", "Truncate text (default 40000)", false), TAB],
    },
    Tool { name: "browser_links", method: "page.links", description: "List links on the page (text and href).", params: &[TAB] },
    Tool {
        name: "browser_click",
        method: "page.click",
        description: "Click an element by CSS selector, or by its visible text.",
        params: &[("selector", "string", "CSS selector", false), ("text", "string", "Visible text of a link/button", false), TAB],
    },
    Tool {
        name: "browser_type",
        method: "page.type",
        description: "Type text into an input/textarea/contenteditable, optionally submitting.",
        params: &[("selector", "string", "CSS selector of the field", true), ("text", "string", "Text to type", true), ("submit", "boolean", "Press Enter / submit the form afterwards", false), TAB],
    },
    Tool {
        name: "browser_eval",
        method: "page.eval",
        description: "Run a JavaScript expression in the page and return its JSON-serializable value.",
        params: &[("script", "string", "JavaScript expression", true), TAB],
    },
    Tool { name: "browser_screenshot", method: "page.screenshot", description: "Screenshot the visible page (PNG).", params: &[TAB] },
    Tool {
        name: "browser_wait",
        method: "page.wait",
        description: "Wait until the page finishes loading.",
        params: &[("timeout_ms", "integer", "Give up after this long (default 15000)", false), TAB],
    },
];

fn tool_list() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            let props: serde_json::Map<String, Value> =
                t.params.iter().map(|(n, ty, d, _)| ((*n).to_string(), json!({ "type": ty, "description": d }))).collect();
            let required: Vec<&str> = t.params.iter().filter(|p| p.3).map(|p| p.0).collect();
            json!({ "name": t.name, "description": t.description, "inputSchema": { "type": "object", "properties": props, "required": required } })
        })
        .collect();
    json!({ "tools": tools })
}

fn call_tool(dir: &Path, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return json!({ "content": [{ "type": "text", "text": format!("unknown tool {name}") }], "isError": true });
    };
    match agent::call(dir, tool.method, args) {
        Ok(v) if tool.method == "page.screenshot" => {
            json!({ "content": [{ "type": "image", "data": v["png_base64"], "mimeType": "image/png" }] })
        }
        Ok(v) => {
            let text = match v {
                Value::String(s) => s,
                other => serde_json::to_string_pretty(&other).unwrap_or_default(),
            };
            json!({ "content": [{ "type": "text", "text": text }] })
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

/// Newline-delimited JSON-RPC 2.0 over stdio, as MCP's stdio transport specifies.
pub fn run(dir: &Path) -> i32 {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "rzen", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Controls the user's running rzen browser. Tools act on the visible tab unless tab_id is given. Prefer browser_read over screenshots for reading pages."
            }}),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": tool_list() }),
            "tools/call" => json!({ "jsonrpc": "2.0", "id": id, "result": call_tool(dir, &params) }),
            other => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {other}") } }),
        };
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    0
}
