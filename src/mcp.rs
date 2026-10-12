//! The agentdesk MCP server: newline-delimited JSON-RPC 2.0 on stdio. It needs five things
//! (initialize, notifications/initialized, ping, tools/list, tools/call), so it is written out here.

use crate::identity::Caller;
use crate::tools_gen::{INSTRUCTIONS, TOOLS};
use crate::{db::Db, paths, tools};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const LATEST: &str = "2025-06-18";
const KNOWN: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

pub fn serve() -> anyhow::Result<()> {
    let db = Db::open(&paths::db_path())?;
    db.init()?;
    let caller = Caller::from_env();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&db, &caller, &msg),
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
        };
        if let Some(r) = reply {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    Ok(())
}

fn result(id: Value, r: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": r})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The reply to one message, or None for a notification (a message with no id gets none).
pub fn handle(db: &Db, caller: &Caller, msg: &Value) -> Option<Value> {
    let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
        let id = msg.get("id").cloned();
        return match id {
            Some(id) if msg.get("result").is_none() && msg.get("error").is_none() => Some(error(id, -32600, "invalid request")),
            _ => None,
        };
    };
    let id = msg.get("id").filter(|i| i.is_number() || i.is_string()).cloned()?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().filter(|v| KNOWN.contains(v)).unwrap_or(LATEST);
            result(
                id,
                json!({
                    "protocolVersion": asked,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "agentdesk", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => result(id, json!({})),
        "tools/list" => {
            let list: Vec<Value> = TOOLS
                .iter()
                .map(|(name, desc, schema)| json!({"name": name, "description": desc, "inputSchema": serde_json::from_str::<Value>(schema).unwrap_or(json!({}))}))
                .collect();
            result(id, json!({"tools": list}))
        }
        "tools/call" => {
            let Some(name) = params["name"].as_str() else { return Some(error(id, -32602, "tools/call needs a tool name")) };
            if !tools::tool_names().contains(&name) {
                return Some(error(id, -32602, &format!("unknown tool: {name}")));
            }
            let args = params.get("arguments").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
            let text = tools::call(db, caller, name, &args);
            result(id, json!({"content": [{"type": "text", "text": text}]}))
        }
        other => error(id, -32601, &format!("method not found: {other}")),
    })
}
