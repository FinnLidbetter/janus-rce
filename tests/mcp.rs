mod common;

use std::path::PathBuf;
use std::time::Duration;

use regex::Regex;
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::Client;
use serde_json::{Value, json};

use janus_rce::config::{
    LoadedArgSpec, LoadedArgType, LoadedCommandSpec, LoadedConfig, ServerConfig,
};

use common::{TEST_TOKEN, auth_header, mcp_msg, post_mcp, test_client};

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_no_auth() {
    let client = test_client().await;
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .body(mcp_msg("initialize", json!({})).to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);
}

#[rocket::async_test]
async fn mcp_wrong_token() {
    let client = test_client().await;
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", "Bearer wrong-token"))
        .body(mcp_msg("initialize", json!({})).to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_initialize() {
    let client = test_client().await;
    let (status, body) = post_mcp(
        &client,
        mcp_msg(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"},
            }),
        ),
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let result = &body["result"];
    assert_eq!(result["protocolVersion"], "2025-03-26");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["serverInfo"]["name"].is_string());
    assert!(result["serverInfo"]["version"].is_string());
}

// ---------------------------------------------------------------------------
// notifications/initialized
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_initialized_notification() {
    // The client sends this as a notification (no id); the server must not crash.
    let client = test_client().await;
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
}

// ---------------------------------------------------------------------------
// tools/list — command enumeration
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_tools_list_returns_all_commands() {
    let client = test_client().await;
    let (status, body) = post_mcp(&client, mcp_msg("tools/list", json!({}))).await;
    assert_eq!(status, Status::Ok);
    let tools = body["result"]["tools"]
        .as_array()
        .expect("tools must be a JSON array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"succeed"));
    assert!(names.contains(&"fail"));
    assert!(names.contains(&"greet"));
}

// ---------------------------------------------------------------------------
// tools/list — JSON Schema shapes for each arg type
// (detailed schema correctness is covered by unit tests in src/mcp.rs;
// these tests verify the HTTP wiring produces a well-formed tool list)
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_tools_list_enum_arg_schema() {
    let client = test_client().await;
    let (status, body) = post_mcp(&client, mcp_msg("tools/list", json!({}))).await;
    assert_eq!(status, Status::Ok);

    let tools = body["result"]["tools"].as_array().unwrap();
    let greet = tools
        .iter()
        .find(|t| t["name"] == "greet")
        .expect("greet tool must be present");

    assert_eq!(greet["description"], "Greets the caller.");

    let schema = &greet["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["format"]["type"], "string");
    assert_eq!(
        schema["properties"]["format"]["enum"],
        json!(["text", "json"])
    );

    let required = schema["required"]
        .as_array()
        .expect("required must be an array");
    assert!(required.iter().any(|v| v == "format"));
}

#[rocket::async_test]
async fn mcp_tools_list_bool_arg_schema() {
    let config = LoadedConfig {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: None,
            output_bytes_max: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![LoadedCommandSpec {
            name: "cmd".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/true"),
            working_dir: None,
            args: vec![LoadedArgSpec {
                name: "verbose".into(),
                description: None,
                flag: "--verbose".into(),
                required: false,
                arg_type: LoadedArgType::Bool,
            }],
            fixed_args: vec![],
            timeout_secs: None,
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(mcp_msg("tools/list", json!({})).to_string())
        .dispatch()
        .await;
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    let tools = body["result"]["tools"].as_array().unwrap();
    let cmd = tools.iter().find(|t| t["name"] == "cmd").unwrap();
    let schema = &cmd["inputSchema"];

    assert_eq!(schema["properties"]["verbose"]["type"], "boolean");
    assert!(
        schema["required"].is_null() || schema["required"].as_array().is_some_and(|a| a.is_empty()),
        "optional arg must not be in required"
    );
}

#[rocket::async_test]
async fn mcp_tools_list_pattern_arg_schema() {
    let config = LoadedConfig {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: None,
            output_bytes_max: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![LoadedCommandSpec {
            name: "cmd".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/true"),
            working_dir: None,
            args: vec![LoadedArgSpec {
                name: "name".into(),
                description: None,
                flag: "--name".into(),
                required: true,
                arg_type: LoadedArgType::Pattern {
                    compiled: Regex::new("^[a-z]+$").unwrap(),
                },
            }],
            fixed_args: vec![],
            timeout_secs: None,
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(mcp_msg("tools/list", json!({})).to_string())
        .dispatch()
        .await;
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    let tools = body["result"]["tools"].as_array().unwrap();
    let prop =
        &tools.iter().find(|t| t["name"] == "cmd").unwrap()["inputSchema"]["properties"]["name"];

    assert_eq!(prop["type"], "string");
    assert_eq!(prop["pattern"], "^[a-z]+$");
}

#[rocket::async_test]
async fn mcp_tools_list_path_arg_schema() {
    let config = LoadedConfig {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: None,
            output_bytes_max: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![LoadedCommandSpec {
            name: "cmd".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/true"),
            working_dir: None,
            args: vec![LoadedArgSpec {
                name: "output".into(),
                description: None,
                flag: "--output".into(),
                required: false,
                arg_type: LoadedArgType::Path {
                    within: vec![PathBuf::from("/tmp").canonicalize().unwrap()],
                },
            }],
            fixed_args: vec![],
            timeout_secs: None,
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(mcp_msg("tools/list", json!({})).to_string())
        .dispatch()
        .await;
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    let tools = body["result"]["tools"].as_array().unwrap();
    let prop =
        &tools.iter().find(|t| t["name"] == "cmd").unwrap()["inputSchema"]["properties"]["output"];

    assert_eq!(prop["type"], "string");
    assert!(
        prop["description"]
            .as_str()
            .unwrap_or("")
            .contains("Absolute path within"),
        "path arg description must mention allowed dirs"
    );
}

// ---------------------------------------------------------------------------
// tools/call — exit codes and isError
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_tools_call_success() {
    let client = test_client().await;
    let (status, body) = post_mcp(
        &client,
        mcp_msg("tools/call", json!({"name": "succeed", "arguments": {}})),
    )
    .await;
    assert_eq!(status, Status::Ok);
    let result = &body["result"];
    assert_eq!(result["isError"], false);
    let content = result["content"]
        .as_array()
        .expect("content must be an array");
    assert!(!content.is_empty());
    assert_eq!(content[0]["type"], "text");
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.contains("Exit code: 0"), "got: {text}");
}

#[rocket::async_test]
async fn mcp_tools_call_failure() {
    let client = test_client().await;
    let (status, body) = post_mcp(
        &client,
        mcp_msg("tools/call", json!({"name": "fail", "arguments": {}})),
    )
    .await;
    assert_eq!(status, Status::Ok);
    let result = &body["result"];
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Exit code: 1"), "got: {text}");
}

// ---------------------------------------------------------------------------
// tools/call — stdout is captured in the response text
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_tools_call_captures_stdout() {
    let config = LoadedConfig {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: None,
            output_bytes_max: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![LoadedCommandSpec {
            name: "hello".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/printf"),
            working_dir: None,
            args: vec![],
            fixed_args: vec!["hello world".into()],
            timeout_secs: None,
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(mcp_msg("tools/call", json!({"name": "hello", "arguments": {}})).to_string())
        .dispatch()
        .await;
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("hello world"),
        "stdout must appear in MCP response text, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// tools/call — validation errors returned as isError results (not HTTP errors)
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_tools_call_unknown_command() {
    let client = test_client().await;
    let (status, body) = post_mcp(
        &client,
        mcp_msg("tools/call", json!({"name": "nope", "arguments": {}})),
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("not found"), "got: {text}");
}

#[rocket::async_test]
async fn mcp_tools_call_missing_required_arg() {
    let client = test_client().await;
    let (status, body) = post_mcp(
        &client,
        mcp_msg("tools/call", json!({"name": "greet", "arguments": {}})),
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("missing required"), "got: {text}");
}

#[rocket::async_test]
async fn mcp_tools_call_invalid_arg_value() {
    let client = test_client().await;
    let (status, body) = post_mcp(
        &client,
        mcp_msg(
            "tools/call",
            json!({"name": "greet", "arguments": {"format": "xml"}}),
        ),
    )
    .await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("invalid value"), "got: {text}");
}

#[rocket::async_test]
async fn mcp_tools_call_missing_name_param() {
    // tools/call with no "name" must return a JSON-RPC error, not a tool result.
    let client = test_client().await;
    let (status, body) = post_mcp(&client, mcp_msg("tools/call", json!({"arguments": {}}))).await;
    assert_eq!(status, Status::Ok);
    assert!(
        body["error"].is_object(),
        "missing name must produce a JSON-RPC error, got: {body}"
    );
    assert_eq!(body["error"]["code"], -32602); // INVALID_PARAMS
}

// ---------------------------------------------------------------------------
// tools/call — resource limits
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_tools_call_at_capacity() {
    let config = LoadedConfig {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: Some(0),
            output_bytes_max: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![LoadedCommandSpec {
            name: "succeed".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/true"),
            working_dir: None,
            args: vec![],
            fixed_args: vec![],
            timeout_secs: None,
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");
    let response = client
        .post("/mcp")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(mcp_msg("tools/call", json!({"name": "succeed", "arguments": {}})).to_string())
        .dispatch()
        .await;
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("capacity"), "got: {text}");
}

#[rocket::async_test]
async fn mcp_tools_call_timeout() {
    let config = LoadedConfig {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: None,
            output_bytes_max: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![LoadedCommandSpec {
            name: "yes".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/yes"),
            working_dir: None,
            args: vec![],
            fixed_args: vec![],
            timeout_secs: Some(1),
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client
            .post("/mcp")
            .header(ContentType::JSON)
            .header(auth_header(TEST_TOKEN))
            .body(mcp_msg("tools/call", json!({"name": "yes", "arguments": {}})).to_string())
            .dispatch(),
    )
    .await
    .expect("response received within 10 s");

    let body: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(10), response.into_string())
            .await
            .expect("body drained")
            .unwrap_or_default(),
    )
    .unwrap();

    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("killed"), "got: {text}");
}

#[rocket::async_test]
async fn mcp_tools_call_output_cap() {
    let config = LoadedConfig {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".into(),
            token: None,
            concurrent_jobs_max: None,
            output_bytes_max: Some(10),
        },
        token: TEST_TOKEN.into(),
        commands: vec![LoadedCommandSpec {
            name: "yes".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/yes"),
            working_dir: None,
            args: vec![],
            fixed_args: vec![],
            timeout_secs: None,
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client
            .post("/mcp")
            .header(ContentType::JSON)
            .header(auth_header(TEST_TOKEN))
            .body(mcp_msg("tools/call", json!({"name": "yes", "arguments": {}})).to_string())
            .dispatch(),
    )
    .await
    .expect("response received within 10 s");

    let body: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(10), response.into_string())
            .await
            .expect("body drained")
            .unwrap_or_default(),
    )
    .unwrap();

    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("killed"), "got: {text}");
}

// ---------------------------------------------------------------------------
// Unknown method
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn mcp_unknown_method() {
    let client = test_client().await;
    let (status, body) = post_mcp(&client, mcp_msg("unknown/method", json!({}))).await;
    assert_eq!(status, Status::Ok);
    assert!(
        body["error"].is_object(),
        "unknown method must return a JSON-RPC error, got: {body}"
    );
    assert_eq!(body["error"]["code"], -32601); // METHOD_NOT_FOUND
}
