mod common;

use std::path::PathBuf;

use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::Client;
use serde_json::{Value, json};

use janus_rce::config::{
    LoadedArgSpec, LoadedArgType, LoadedCommandSpec, LoadedConfig, ServerConfig,
};

use common::{TEST_TOKEN, auth_header, parse_sse, test_client};

// ---------------------------------------------------------------------------
// /health
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn health_no_auth() {
    let client = test_client().await;
    let response = client.get("/health").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert_eq!(body["status"], "ok");
}

// ---------------------------------------------------------------------------
// /commands
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn commands_no_auth() {
    let client = test_client().await;
    let response = client.get("/commands").dispatch().await;
    assert_eq!(response.status(), Status::Unauthorized);
}

#[rocket::async_test]
async fn commands_wrong_token() {
    let client = test_client().await;
    let response = client
        .get("/commands")
        .header(auth_header("wrong-token"))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);
}

#[rocket::async_test]
async fn commands_ok() {
    let client = test_client().await;
    let response = client
        .get("/commands")
        .header(auth_header(TEST_TOKEN))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let body: Vec<Value> = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    let names: Vec<&str> = body.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"succeed"));
    assert!(names.contains(&"fail"));
}

#[rocket::async_test]
async fn commands_response_arg_structure() {
    let client = test_client().await;
    let response = client
        .get("/commands")
        .header(auth_header(TEST_TOKEN))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let body: Vec<Value> = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();

    let greet = body
        .iter()
        .find(|c| c["name"] == "greet")
        .expect("greet command should appear in /commands response");
    let args = greet["args"].as_array().expect("args must be a JSON array");
    assert_eq!(args.len(), 1, "greet has exactly one arg");
    assert_eq!(args[0]["name"], "format");
    assert_eq!(args[0]["description"], "Output format to use.");
    assert_eq!(args[0]["required"], true);
    assert_eq!(args[0]["type"], "enum");
    assert_eq!(
        args[0]["values"],
        json!(["text", "json"]),
        "enum values must be listed",
    );
    assert!(
        args[0]["pattern"].is_null(),
        "pattern must be absent for enum args"
    );
    assert!(
        args[0]["within"].is_null(),
        "within must be absent for enum args"
    );
    assert_eq!(greet["description"], "Greets the caller.");
    assert_eq!(greet["timeout_secs"], 30);
}

#[rocket::async_test]
async fn commands_path_arg_exposes_within() {
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
            name: "write".into(),
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
        .get("/commands")
        .header(auth_header(TEST_TOKEN))
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let body: Vec<Value> = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    let cmd = body.iter().find(|c| c["name"] == "write").unwrap();
    let arg = &cmd["args"][0];
    assert_eq!(arg["type"], "path");
    let within = arg["within"].as_array().expect("within must be an array");
    assert!(
        !within.is_empty(),
        "within must list at least one directory"
    );
    assert!(
        arg["values"].is_null(),
        "values must be absent for path args"
    );
    assert!(
        arg["pattern"].is_null(),
        "pattern must be absent for path args"
    );
}

// ---------------------------------------------------------------------------
// /run — auth and validation errors
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn run_no_auth() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .body("{}")
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);
}

#[rocket::async_test]
async fn run_authorization_wrong_scheme() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", "Basic dXNlcjpwYXNz"))
        .body(r#"{"command":"succeed"}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Unauthorized);
}

#[rocket::async_test]
async fn run_malformed_json_body() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body("{not valid json")
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::BadRequest);
}

#[rocket::async_test]
async fn run_missing_command_field() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(r#"{"args": {}}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
}

#[rocket::async_test]
async fn run_unknown_command() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(r#"{"command":"nope"}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::NotFound);
}

#[rocket::async_test]
async fn run_unknown_arg() {
    let client = test_client().await;
    let body = json!({"command": "greet", "args": {"format": "text", "unknown": "x"}});
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(body.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
}

#[rocket::async_test]
async fn run_missing_required_arg() {
    let client = test_client().await;
    let body = json!({"command": "greet", "args": {}});
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(body.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
}

#[rocket::async_test]
async fn run_invalid_enum() {
    let client = test_client().await;
    let body = json!({"command": "greet", "args": {"format": "xml"}});
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(body.to_string())
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::UnprocessableEntity);
}

// ---------------------------------------------------------------------------
// /run — SSE output and exit codes
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn run_succeed_exits_zero() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(r#"{"command":"succeed"}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let raw = response.into_string().await.unwrap();
    let events = parse_sse(&raw);
    let last = events.last().expect("at least one SSE event");
    assert_eq!(last["type"], "exit");
    assert_eq!(last["code"], 0);
}

#[rocket::async_test]
async fn run_fail_exits_nonzero() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(r#"{"command":"fail"}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let raw = response.into_string().await.unwrap();
    let events = parse_sse(&raw);
    let last = events.last().expect("at least one SSE event");
    assert_eq!(last["type"], "exit");
    assert_eq!(last["code"], 1);
}

#[rocket::async_test]
async fn run_sse_content_type() {
    let client = test_client().await;
    let response = client
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(r#"{"command":"succeed"}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let ct = response
        .headers()
        .get_one("Content-Type")
        .expect("response must have a Content-Type header");
    assert!(
        ct.contains("text/event-stream"),
        "expected text/event-stream, got: {ct}"
    );
}

// ---------------------------------------------------------------------------
// /run — unknown route 404 JSON envelope
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn get_unknown_route() {
    let client = test_client().await;
    let response = client.get("/nonexistent").dispatch().await;
    assert_eq!(response.status(), Status::NotFound);
    let body: Value = serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
    assert!(
        body["error"].is_string(),
        "expected JSON error envelope, got: {body}"
    );
}

// ---------------------------------------------------------------------------
// Executor — environment isolation
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn env_isolation_strips_parent_environment() {
    use std::collections::HashSet;

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
            name: "env".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/env"),
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
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(r#"{"command":"env"}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::Ok);
    let raw = response.into_string().await.unwrap();
    let events = parse_sse(&raw);

    let safe_keys: HashSet<&str> = ["PATH", "LANG", "HOME", "USER", "TMPDIR", "DEVELOPER_DIR"]
        .iter()
        .copied()
        .collect();

    for event in &events {
        if event["type"] == "stdout" {
            let line = event["data"].as_str().unwrap_or("");
            if let Some(key) = line.split('=').next() {
                assert!(
                    safe_keys.contains(key),
                    "unexpected env var in child process: {key:?} (from line {line:?})"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Executor — server shutdown kills running child
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn run_killed_on_shutdown() {
    use std::time::Duration;

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
            timeout_secs: None,
        }],
    };
    let client = Client::tracked(janus_rce::build_rocket(rocket::Config::figment(), config))
        .await
        .expect("valid rocket instance");

    let shutdown = client.rocket().shutdown().clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.notify();
    });

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post("/run")
            .header(ContentType::JSON)
            .header(auth_header(TEST_TOKEN))
            .body(r#"{"command":"yes"}"#)
            .dispatch(),
    )
    .await
    .expect("dispatch completed within timeout");

    assert_eq!(response.status(), Status::Ok);

    let raw = tokio::time::timeout(Duration::from_secs(5), response.into_string())
        .await
        .expect("body drained within timeout")
        .unwrap_or_default();

    let events = parse_sse(&raw);

    assert!(
        !events.iter().any(|e| e["type"] == "exit"),
        "expected no exit event when killed by shutdown, got: {events:?}",
    );
    assert!(
        events.iter().any(|e| e["type"] == "stdout"),
        "expected stdout events from yes(1) before shutdown, got: {events:?}",
    );
}

// ---------------------------------------------------------------------------
// Executor — concurrent job limit
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn run_at_capacity_returns_429() {
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
        .post("/run")
        .header(ContentType::JSON)
        .header(auth_header(TEST_TOKEN))
        .body(r#"{"command":"succeed"}"#)
        .dispatch()
        .await;
    assert_eq!(response.status(), Status::TooManyRequests);
}

// ---------------------------------------------------------------------------
// Executor — per-command timeout
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn run_timeout_kills_child() {
    use std::time::Duration as StdDuration;

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

    let wall_start = std::time::Instant::now();

    let response = tokio::time::timeout(
        StdDuration::from_secs(10),
        client
            .post("/run")
            .header(ContentType::JSON)
            .header(auth_header(TEST_TOKEN))
            .body(r#"{"command":"yes"}"#)
            .dispatch(),
    )
    .await
    .expect("response received within 10 s");

    assert_eq!(response.status(), Status::Ok);

    let raw = tokio::time::timeout(StdDuration::from_secs(10), response.into_string())
        .await
        .expect("body drained within 10 s")
        .unwrap_or_default();

    let events = parse_sse(&raw);
    let last = events.last().expect("at least one SSE event");
    assert_eq!(last["type"], "exit", "final event must be exit");
    assert!(
        last["code"].is_null(),
        "exit code must be null for a timed-out command, got: {}",
        last["code"],
    );
    assert!(
        wall_start.elapsed() < StdDuration::from_secs(8),
        "timeout should have fired within ~1 s, not {:?}",
        wall_start.elapsed(),
    );
}

// ---------------------------------------------------------------------------
// Executor — output byte cap
// ---------------------------------------------------------------------------

#[rocket::async_test]
async fn run_output_cap_terminates_stream() {
    use std::time::Duration as StdDuration;

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
        StdDuration::from_secs(10),
        client
            .post("/run")
            .header(ContentType::JSON)
            .header(auth_header(TEST_TOKEN))
            .body(r#"{"command":"yes"}"#)
            .dispatch(),
    )
    .await
    .expect("response received within 10 s");

    assert_eq!(response.status(), Status::Ok);

    let raw = tokio::time::timeout(StdDuration::from_secs(10), response.into_string())
        .await
        .expect("body drained within 10 s")
        .unwrap_or_default();

    let events = parse_sse(&raw);
    let last = events.last().expect("at least one SSE event");
    assert_eq!(last["type"], "exit", "final event must be exit");
    assert!(
        last["code"].is_null(),
        "exit code must be null when output cap is exceeded, got: {}",
        last["code"],
    );
}
