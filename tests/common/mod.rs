#![allow(dead_code)] // helpers are shared across test binaries; not all are used in each

use std::path::PathBuf;

use rocket::http::Header;
use rocket::local::asynchronous::Client;
use serde_json::Value;

use janus_rce::config::{
    LoadedArgSpec, LoadedArgType, LoadedCommandSpec, LoadedConfig, ServerConfig,
};

pub const TEST_TOKEN: &str = "test-token";

pub fn test_config() -> LoadedConfig {
    LoadedConfig {
        server: ServerConfig {
            port: 8080,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: None,
            output_bytes_max: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![
            LoadedCommandSpec {
                name: "succeed".into(),
                description: None,
                executable: PathBuf::from("/usr/bin/true"),
                working_dir: None,
                args: vec![],
                fixed_args: vec![],
                timeout_secs: None,
            },
            LoadedCommandSpec {
                name: "fail".into(),
                description: None,
                executable: PathBuf::from("/usr/bin/false"),
                working_dir: None,
                args: vec![],
                fixed_args: vec![],
                timeout_secs: None,
            },
            LoadedCommandSpec {
                name: "greet".into(),
                description: Some("Greets the caller.".into()),
                executable: PathBuf::from("/usr/bin/true"),
                working_dir: None,
                args: vec![LoadedArgSpec {
                    name: "format".into(),
                    description: Some("Output format to use.".into()),
                    flag: "--format".into(),
                    required: true,
                    arg_type: LoadedArgType::Enum {
                        values: vec!["text".into(), "json".into()],
                    },
                }],
                fixed_args: vec![],
                timeout_secs: Some(30),
            },
        ],
    }
}

pub async fn test_client() -> Client {
    Client::tracked(janus_rce::build_rocket(
        rocket::Config::figment(),
        test_config(),
    ))
    .await
    .expect("valid rocket instance")
}

pub fn auth_header(token: &str) -> Header<'static> {
    Header::new("Authorization", format!("Bearer {token}"))
}

/// Parses SSE body text into a `Vec` of the JSON payloads from each `data:` line.
pub fn parse_sse(body: &str) -> Vec<Value> {
    body.lines()
        .filter(|line| line.starts_with("data:"))
        .map(|line| serde_json::from_str(&line["data:".len()..]).unwrap())
        .collect()
}

/// Builds a JSON-RPC 2.0 request with `id = 1`.
pub fn mcp_msg(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
}

/// Posts a JSON-RPC message to `/mcp` with the test token and returns the
/// HTTP status and parsed response body.
pub async fn post_mcp(client: &Client, body: serde_json::Value) -> (rocket::http::Status, Value) {
    use rocket::http::ContentType;
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(body.to_string())
        .dispatch()
        .await;
    let status = response.status();
    let parsed: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    (status, parsed)
}
