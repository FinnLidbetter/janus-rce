use std::path::PathBuf;

use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::Client;
use serde_json::{Value, json};

use janus_rce::config::{
    LoadedArgSpec, LoadedArgType, LoadedCommandSpec, LoadedConfig, ServerConfig,
};

const TEST_TOKEN: &str = "test-token";

fn test_config() -> LoadedConfig {
    LoadedConfig {
        server: ServerConfig {
            port: 8080,
            bind: "127.0.0.1".into(),
            token: None,
        },
        token: TEST_TOKEN.into(),
        commands: vec![
            LoadedCommandSpec {
                name: "succeed".into(),
                executable: PathBuf::from("/usr/bin/true"),
                working_dir: None,
                args: vec![],
            },
            LoadedCommandSpec {
                name: "fail".into(),
                executable: PathBuf::from("/usr/bin/false"),
                working_dir: None,
                args: vec![],
            },
            LoadedCommandSpec {
                name: "greet".into(),
                executable: PathBuf::from("/usr/bin/true"),
                working_dir: None,
                args: vec![LoadedArgSpec {
                    name: "format".into(),
                    flag: "--format".into(),
                    required: true,
                    arg_type: LoadedArgType::Enum {
                        values: vec!["text".into(), "json".into()],
                    },
                }],
            },
        ],
    }
}

async fn test_client() -> Client {
    Client::tracked(janus_rce::build_rocket(
        rocket::Config::figment(),
        test_config(),
    ))
    .await
    .expect("valid rocket instance")
}

fn auth_header(token: &str) -> Header<'static> {
    Header::new("Authorization", format!("Bearer {token}"))
}

fn parse_sse(body: &str) -> Vec<Value> {
    body.lines()
        .filter(|line| line.starts_with("data:"))
        .map(|line| serde_json::from_str(&line["data:".len()..]).unwrap())
        .collect()
}

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
// /run — SSE exit codes
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
