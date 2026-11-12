//! MCP tools (stateless streamable HTTP at `POST /mcp`, and `saltyctl mcp` over stdio).
//! Read-only for now: agents can inspect releases, but signing stays with the org's keys.

use salty_core::Store;
use serde_json::{Value, json};
use weirdnet_mcp::{Param, ParamType, Server, ServerInfo, ToolError, ToolSpec, Tools, args};
use weirdnet_update::Root;

const APP: Param = Param::required("app", ParamType::String, "App id");
const CHANNEL: Param = Param::required("channel", ParamType::String, "Channel (stable, beta, ...)");

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "list_channels",
        description: "Every app and channel with published updates, and the latest sequence number.",
        params: &[],
        auth: false,
        read_only: true,
        destructive: false,
        open_world: false,
    },
    ToolSpec {
        name: "channel_history",
        description: "Published updates for one channel, newest first: id, seq, version, kind, created, expires and notes.",
        params: &[APP, CHANNEL],
        auth: false,
        read_only: true,
        destructive: false,
        open_world: false,
    },
    ToolSpec {
        name: "root_info",
        description: "The org's current root: version, key count, thresholds, delegated release keys and the app pin.",
        params: &[],
        auth: false,
        read_only: true,
        destructive: false,
        open_world: false,
    },
];

struct SaltyTools<'a>(&'a Store);

fn internal(e: salty_core::Error) -> ToolError {
    match e {
        salty_core::Error::Db(_) | salty_core::Error::Io(_) => {
            eprintln!("salty-server: mcp internal error: {e}");
            ToolError("internal error".into())
        }
        e => ToolError(e.to_string()),
    }
}

impl Tools for SaltyTools<'_> {
    fn specs(&self) -> &[ToolSpec] {
        TOOLS
    }

    fn call(&self, tool: &ToolSpec, a: &Value, _token: Option<&str>) -> Result<Value, ToolError> {
        let store = self.0;
        Ok(match tool.name {
            "list_channels" => {
                let rows = store.channels().map_err(internal)?;
                json!({"channels": rows.iter().map(|(a, c, s)| json!({"app": a, "channel": c, "latest_seq": s})).collect::<Vec<_>>()})
            }
            "channel_history" => {
                let h = store.history(args::string(a, "app")?, args::string(a, "channel")?).map_err(internal)?;
                json!({"updates": h.iter().map(|m| json!({
                    "update_id": m.update_id,
                    "seq": m.seq,
                    "version": m.version.to_string(),
                    "kind": m.kind.name(),
                    "files": m.files.len(),
                    "bytes": m.total_size(),
                    "created": m.created.0,
                    "expires": m.expires.0,
                    "rollback_of": m.rollback_of,
                    "notes": m.notes,
                })).collect::<Vec<_>>()})
            }
            "root_info" => match store.current_root().map_err(internal)? {
                None => json!({"root": null}),
                Some(r) => root_json(&r),
            },
            other => return Err(ToolError(format!("unknown tool: {other}"))),
        })
    }
}

fn root_json(r: &Root) -> Value {
    json!({"root": {
        "version": r.version,
        "org": r.org,
        "keys": r.keys.len(),
        "threshold": r.threshold,
        "release_keys": r.release.iter().map(|d| d.key.to_hex()).collect::<Vec<_>>(),
        "release_threshold": r.release_threshold,
        "timestamp_keys": r.timestamp.len(),
        "pin": r.pin().0.to_hex(),
    }})
}

/// Handles one MCP JSON-RPC message (or batch).
pub fn handle(store: &Store, token: Option<&str>, msg: Value) -> Option<Value> {
    let info = ServerInfo {
        name: "salty-gull".into(),
        title: "Salty Gull".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        instructions:
            "Salty Gull serves signed over-the-air updates. These tools are read-only; signing and publishing stay with the org's keys and saltyctl."
                .into(),
    };
    Server::new(info, SaltyTools(store)).handle(token, msg)
}
