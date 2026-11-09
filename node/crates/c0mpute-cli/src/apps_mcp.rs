//! `c0mpute apps mcp`: an MCP server on stdio for agents.
//!
//! ```text
//! claude mcp add c0mpute-apps -- c0mpute apps mcp
//! ```
//!
//! Tools deploy, list, inspect, read logs and delete apps with this
//! machine's own key (the same thing `c0mpute apps ...` does), so an agent can
//! ship a backend to the network without a server or an API key. Reads go to
//! a gateway (`--gateway`, default the local node).

use std::time::Duration;

use anyhow::Result;
use c0mpute_core::apps::deploy;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn tools() -> Value {
    json!([
        {
            "name": "apps_deploy",
            "description": "Deploy (or redeploy) a backend from a directory with a Dockerfile, or a digest-pinned registry image. The network keeps 3 copies, runs one, snapshots `state` to the others and fails over by itself. Returns the app id; it is served at /a/<app_id>/ on every gateway.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "1-59 of a-z, 0-9, -"},
                    "dir": {"type": "string", "description": "Directory to docker build (c0mpute.toml there supplies defaults)"},
                    "pull": {"type": "string", "description": "Registry image pinned by digest, name@sha256:..."},
                    "port": {"type": "integer"},
                    "health": {"type": "string"},
                    "state": {"type": "string", "description": "Directory in the container holding state, e.g. /data"},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}},
                    "memory_mb": {"type": "integer"},
                    "replicas": {"type": "integer"},
                    "domains": {"type": "array", "items": {"type": "string"}},
                    "hosts": {"type": "array", "items": {"type": "string"}, "description": "Peer ids allowed to hold the app; empty = any apps node (each holder gets the app's secrets)"},
                    "wait_secs": {"type": "integer"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "apps_list",
            "description": "Apps the gateway knows: holders, primary, epoch, health.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "apps_status",
            "description": "One app's record and live state (by name deployed from here, or app id).",
            "inputSchema": {"type": "object", "properties": {"app": {"type": "string"}}, "required": ["app"]}
        },
        {
            "name": "apps_logs",
            "description": "The primary's container logs (owner only).",
            "inputSchema": {"type": "object", "properties": {"app": {"type": "string"}, "tail": {"type": "integer"}}, "required": ["app"]}
        },
        {
            "name": "apps_secret_set",
            "description": "Set a secret env var for an app; sealed to each holder at the next deploy, never stored on the network in the clear.",
            "inputSchema": {"type": "object", "properties": {"app": {"type": "string"}, "key": {"type": "string"}, "value": {"type": "string"}}, "required": ["app", "key", "value"]}
        },
        {
            "name": "apps_destroy",
            "description": "Delete an app and its state everywhere. Irreversible: pass confirm=true.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}, "confirm": {"type": "boolean"}}, "required": ["name", "confirm"]}
        }
    ])
}

async fn call(gateway: &str, name: &str, a: &Value) -> Result<String> {
    let s = |k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
    let n = |k: &str| a.get(k).and_then(Value::as_u64);
    match name {
        "apps_deploy" => {
            let app_name = s("name").unwrap_or_default();
            let wait = Duration::from_secs(n("wait_secs").unwrap_or(600));
            let dir = s("dir").map(std::path::PathBuf::from);
            let mut opts = match &dir {
                Some(d) => match deploy::read_manifest(d)? {
                    Some(m) => m.options(d, wait),
                    None => base(&app_name, deploy::Source::Build(d.clone()), wait),
                },
                None => base(&app_name, deploy::Source::Local(String::new()), wait),
            };
            if !app_name.is_empty() {
                opts.name = app_name;
            }
            if let Some(p) = s("pull") {
                opts.source = deploy::Source::Pull(p);
            }
            if matches!(&opts.source, deploy::Source::Local(i) if i.is_empty()) {
                anyhow::bail!("give dir (to build) or pull (a pinned image)");
            }
            if let Some(p) = n("port") {
                opts.port = p as u16;
            }
            if let Some(h) = s("health") {
                opts.health_path = h;
            }
            if let Some(st) = s("state") {
                opts.state_path = Some(st);
            }
            if let Some(m) = n("memory_mb") {
                opts.memory_mb = m as u32;
            }
            if let Some(r) = n("replicas") {
                opts.replicas = r as u8;
            }
            if let Some(env) = a.get("env").and_then(Value::as_object) {
                for (k, v) in env {
                    if let Some(v) = v.as_str() {
                        opts.env.insert(k.clone(), v.to_string());
                    }
                }
            }
            if let Some(h) = a.get("hosts").and_then(Value::as_array) {
                opts.hosts
                    .extend(h.iter().filter_map(Value::as_str).map(str::to_string));
            }
            if let Some(d) = a.get("domains").and_then(Value::as_array) {
                opts.domains
                    .extend(d.iter().filter_map(Value::as_str).map(str::to_string));
            }
            let rec = deploy::deploy(opts).await?;
            Ok(json!({"app_id": rec.app_id, "path": format!("/a/{}/", rec.app_id), "seq": rec.seq}).to_string())
        }
        "apps_list" => get(&format!("{gateway}/apps/v1/apps")).await,
        "apps_status" => {
            let app = s("app").unwrap_or_default();
            let id = deploy::load_owner(&app)
                .record
                .map(|r| r.app_id)
                .unwrap_or(app);
            get(&format!("{gateway}/apps/v1/apps/{id}")).await
        }
        "apps_logs" => {
            deploy::logs(
                &s("app").unwrap_or_default(),
                n("tail").unwrap_or(200) as u32,
            )
            .await
        }
        "apps_secret_set" => {
            deploy::set_secret(
                &s("app").unwrap_or_default(),
                &s("key").unwrap_or_default(),
                s("value").as_deref(),
            )?;
            Ok("saved; redeploy to hand it to the holders".into())
        }
        "apps_destroy" => {
            if a.get("confirm").and_then(Value::as_bool) != Some(true) {
                anyhow::bail!("refusing without confirm=true");
            }
            let id = deploy::destroy(&s("name").unwrap_or_default()).await?;
            Ok(format!("deleted {id}"))
        }
        _ => anyhow::bail!("unknown tool {name}"),
    }
}

fn base(name: &str, source: deploy::Source, wait: Duration) -> deploy::DeployOptions {
    deploy::DeployOptions {
        name: name.to_string(),
        source,
        port: 8080,
        health_path: "/".into(),
        env: Default::default(),
        state_path: None,
        snapshot_secs: c0mpute_core::apps::record::DEFAULT_SNAPSHOT_SECS,
        memory_mb: 256,
        millicpus: 500,
        replicas: c0mpute_core::apps::record::DEFAULT_REPLICAS,
        domains: vec![],
        hosts: vec![],
        wait,
    }
}

async fn get(url: &str) -> Result<String> {
    Ok(reqwest::get(url).await?.text().await?)
}

/// One JSON-RPC message in, at most one out.
pub async fn handle(gateway: &str, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "c0mpute-apps", "version": env!("CARGO_PKG_VERSION")}
        }),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            match call(gateway, name, &args).await {
                Ok(t) => json!({"content": [{"type": "text", "text": t}]}),
                Err(e) => {
                    json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true})
                }
            }
        }
        "ping" => json!({}),
        _ if id.is_none() => return None, // notifications
        _ => {
            return Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}}),
            );
        }
    };
    id.map(|id| json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

pub async fn serve(gateway: String) -> Result<()> {
    let gateway = gateway.trim_end_matches('/').to_string();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(resp) = handle(&gateway, &msg).await {
            out.write_all(resp.to_string().as_bytes()).await?;
            out.write_all(b"\n").await?;
            out.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn speaks_mcp() {
        let r = handle(
            "http://x",
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["serverInfo"]["name"], "c0mpute-apps");
        let r = handle(
            "http://x",
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await
        .unwrap();
        let names: Vec<_> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            names.contains(&"apps_deploy".to_string())
                && names.contains(&"apps_destroy".to_string())
        );
        assert!(
            handle(
                "http://x",
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .await
            .is_none()
        );
        let r = handle("http://x", &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"apps_destroy","arguments":{"name":"a","confirm":false}}})).await.unwrap();
        assert_eq!(
            r["result"]["isError"], true,
            "destroy refuses without confirm"
        );
    }
}
