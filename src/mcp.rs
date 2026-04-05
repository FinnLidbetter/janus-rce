//! MCP (Model Context Protocol) HTTP transport handler.
//!
//! Exposes janus-rce commands as MCP tools via the [Streamable HTTP transport].
//! The single endpoint `POST /mcp` accepts JSON-RPC 2.0 messages and dispatches
//! to the appropriate handler.
//!
//! # Supported methods
//!
//! | Method | Description |
//! |--------|-------------|
//! | `initialize` | MCP handshake; returns server capabilities |
//! | `notifications/initialized` | Notification from client; acknowledged silently |
//! | `tools/list` | Returns all configured commands as MCP tools |
//! | `tools/call` | Executes a command; streams output as SSE when accepted, otherwise buffered |
//!
//! # Streaming
//!
//! When the client sends `Accept: text/event-stream`, `tools/call` returns a
//! `text/event-stream` response.  As the child process runs, each output line
//! is emitted as a `notifications/message` JSON-RPC notification so the client
//! receives progress in real time.  The final event is the JSON-RPC response for
//! the original request, containing the complete buffered output.
//!
//! When the client does not include `text/event-stream` in `Accept`, the
//! response is a plain `application/json` object (original behaviour).
//!
//! # Tool schema
//!
//! Each janus command is exposed as an MCP tool.  Argument types are mapped
//! to JSON Schema as follows:
//!
//! | Janus type | JSON Schema |
//! |------------|-------------|
//! | `enum`     | `{"type":"string","enum":[...]}` |
//! | `pattern`  | `{"type":"string","pattern":"..."}` |
//! | `path`     | `{"type":"string"}` with allowed dirs in description |
//! | `bool`     | `{"type":"boolean"}` |
//!
//! # Authentication
//!
//! All requests require a valid `Authorization: Bearer <token>` header,
//! enforced by the [`AuthToken`] request guard — the same token used by the
//! existing REST endpoints.
//!
//! # Claude Code configuration
//!
//! Add janus-rce as an MCP server in your Claude Code settings:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "janus": {
//!       "type": "http",
//!       "url": "http://host.docker.internal:<port>/mcp",
//!       "headers": { "Authorization": "Bearer <token>" }
//!     }
//!   }
//! }
//! ```
//!
//! [Streamable HTTP transport]: https://modelcontextprotocol.io/specification/2025-03-26/basic/transports#streamable-http

use std::collections::HashMap;

use rocket::Request;
use rocket::request::{FromRequest, Outcome};
use rocket::response::Responder;
use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::{Shutdown, State, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::auth::AuthToken;
use crate::config::{LoadedArgType, LoadedCommandSpec, LoadedConfig};
use crate::executor::{self, BufferedOutput, DrainOutcome, Tagged};
use crate::routes::{JobLimiter, RunRequest};
use crate::validate;

// ---------------------------------------------------------------------------
// Response type
// ---------------------------------------------------------------------------

/// Route return type: either a plain JSON body or an SSE stream.
enum McpResponse {
    Json(Json<Value>),
    Sse(EventStream<UnboundedReceiverStream<Event>>),
}

impl<'r> Responder<'r, 'r> for McpResponse {
    fn respond_to(self, req: &'r Request<'_>) -> rocket::response::Result<'r> {
        match self {
            McpResponse::Json(j) => j.respond_to(req),
            McpResponse::Sse(s) => s.respond_to(req),
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC error codes (defined by the JSON-RPC 2.0 spec)
// ---------------------------------------------------------------------------

const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;

// ---------------------------------------------------------------------------
// JSON-RPC request type
// ---------------------------------------------------------------------------

/// Inbound JSON-RPC 2.0 message (request or notification).
///
/// A **request** carries an `id`; the server must send a matching response.
/// A **notification** has no `id`; the server must not send a response.
#[derive(Deserialize)]
pub struct JsonRpcMessage {
    /// Request identifier.  `None` indicates a notification.
    #[serde(default)]
    pub id: Option<Value>,
    /// Method name (e.g. `"tools/list"`).
    pub method: String,
    /// Method parameters; defaults to a JSON null when absent.
    #[serde(default)]
    pub params: Value,
}

// ---------------------------------------------------------------------------
// JSON-RPC response helpers
// ---------------------------------------------------------------------------

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

// ---------------------------------------------------------------------------
// Tool schema builder
// ---------------------------------------------------------------------------

/// Converts a [`LoadedCommandSpec`] into an MCP tool definition.
fn tool_definition(cmd: &LoadedCommandSpec) -> Value {
    let mut properties: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();

    for arg in &cmd.args {
        let mut prop: serde_json::Map<String, Value> = serde_json::Map::new();

        match &arg.arg_type {
            LoadedArgType::Enum { values } => {
                prop.insert("type".into(), json!("string"));
                prop.insert("enum".into(), json!(values));
            }
            LoadedArgType::Pattern { compiled } => {
                prop.insert("type".into(), json!("string"));
                prop.insert("pattern".into(), json!(compiled.as_str()));
            }
            LoadedArgType::Path { within } => {
                prop.insert("type".into(), json!("string"));
                let dirs: Vec<String> = within.iter().map(|p| p.display().to_string()).collect();
                let base = arg.description.as_deref().unwrap_or("");
                let note = format!("Absolute path within: {}", dirs.join(", "));
                prop.insert(
                    "description".into(),
                    json!(if base.is_empty() {
                        note
                    } else {
                        format!("{base}. {note}")
                    }),
                );
            }
            LoadedArgType::Bool => {
                prop.insert("type".into(), json!("boolean"));
            }
        }

        // Add description for non-Path types (Path already embeds it above).
        if !matches!(&arg.arg_type, LoadedArgType::Path { .. })
            && let Some(desc) = &arg.description
        {
            prop.insert("description".into(), json!(desc));
        }

        properties.insert(arg.name.clone(), Value::Object(prop));

        if arg.required {
            required.push(arg.name.clone());
        }
    }

    let mut schema = json!({
        "type": "object",
        "properties": properties,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }

    let mut tool = json!({
        "name": cmd.name,
        "inputSchema": schema,
    });
    if let Some(desc) = &cmd.description {
        tool["description"] = json!(desc);
    }

    tool
}

// ---------------------------------------------------------------------------
// Output formatter
// ---------------------------------------------------------------------------

/// Formats a [`BufferedOutput`] as a single text string for MCP tool results.
///
/// stdout and stderr are kept separate for readability.  The exit code (or a
/// killed-process note) is appended at the end.
fn format_output(output: &BufferedOutput) -> String {
    let mut text = String::new();

    if !output.stdout_lines.is_empty() {
        text.push_str(&output.stdout_lines.join("\n"));
        text.push('\n');
    }

    if !output.stderr_lines.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&output.stderr_lines.join("\n"));
        text.push('\n');
    }

    let exit_note = match output.exit_code {
        Some(code) => format!("\nExit code: {code}"),
        None => "\n[Process was killed before completing]".to_string(),
    };
    text.push_str(&exit_note);

    text.trim().to_string()
}

// ---------------------------------------------------------------------------
// SSE streaming support
// ---------------------------------------------------------------------------

/// Request guard that reports whether the client accepts `text/event-stream`.
///
/// Returns `true` when the `Accept` header contains `text/event-stream`.
/// Never fails: a missing or non-matching `Accept` header yields `false`.
pub(crate) struct WantsEventStream(bool);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WantsEventStream {
    type Error = std::convert::Infallible;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let wants = req
            .headers()
            .get("Accept")
            .any(|v| v.contains("text/event-stream"));
        Outcome::Success(WantsEventStream(wants))
    }
}

/// Spawns `cmd` and returns a channel-backed stream of MCP SSE events.
///
/// While the child process runs, each stdout line is emitted as a
/// `notifications/message` event at level `"info"` and each stderr line at
/// level `"warning"`.  When the process finishes the final event is the
/// JSON-RPC response for the original `tools/call` request (same `id`),
/// containing the complete buffered output formatted by [`format_output`].
///
/// On server shutdown the spawned task exits early without sending the final
/// response, which closes the channel and terminates the SSE stream.
fn stream_tools_call(
    cmd: validate::ValidatedCommand,
    id: Option<Value>,
    shutdown: Shutdown,
    permit: Option<OwnedSemaphorePermit>,
) -> UnboundedReceiverStream<Event> {
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    tokio::spawn(async move {
        let _permit = permit;
        let start = std::time::Instant::now();
        tracing::info!(
            command = %cmd.name,
            executable = %cmd.executable.display(),
            argv = ?cmd.argv,
            "command started (mcp stream)",
        );

        let child = match executor::spawn_child(&cmd) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    command = %cmd.name,
                    executable = %cmd.executable.display(),
                    error = %e,
                    "failed to spawn command",
                );
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": format!("failed to spawn command: {e}")}],
                        "isError": true,
                    }
                });
                let _ = event_tx.send(Event::json(&response).event("message"));
                return;
            }
        };

        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<Tagged>();
        let name = cmd.name.clone();
        let drain_fut = executor::drain_child(
            name.clone(),
            cmd.timeout_secs,
            cmd.output_bytes_max,
            child,
            shutdown,
            line_tx,
        );
        tokio::pin!(drain_fut);

        let mut outcome: Option<DrainOutcome> = None;
        let mut stdout_lines: Vec<String> = Vec::new();
        let mut stderr_lines: Vec<String> = Vec::new();

        loop {
            tokio::select! {
                biased;
                result = &mut drain_fut => {
                    outcome = Some(result);
                    break;
                }
                tagged = line_rx.recv() => {
                    match tagged {
                        Some(Tagged::Stdout(line)) => {
                            let notif = json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/message",
                                "params": {"level": "info", "logger": &name, "data": &line}
                            });
                            let _ = event_tx.send(Event::json(&notif).event("message"));
                            stdout_lines.push(line);
                        }
                        Some(Tagged::Stderr(line)) => {
                            let notif = json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/message",
                                "params": {"level": "warning", "logger": &name, "data": &line}
                            });
                            let _ = event_tx.send(Event::json(&notif).event("message"));
                            stderr_lines.push(line);
                        }
                        None => break,
                    }
                }
            }
        }

        // Drain any lines that arrived after drain_fut completed.
        while let Some(tagged) = line_rx.recv().await {
            match tagged {
                Tagged::Stdout(line) => {
                    let notif = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/message",
                        "params": {"level": "info", "logger": &name, "data": &line}
                    });
                    let _ = event_tx.send(Event::json(&notif).event("message"));
                    stdout_lines.push(line);
                }
                Tagged::Stderr(line) => {
                    let notif = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/message",
                        "params": {"level": "warning", "logger": &name, "data": &line}
                    });
                    let _ = event_tx.send(Event::json(&notif).event("message"));
                    stderr_lines.push(line);
                }
            }
        }

        let exit_code = match outcome {
            Some(DrainOutcome::Exited(code)) => {
                tracing::info!(
                    command = %name,
                    exit_code = ?code,
                    duration_ms = start.elapsed().as_millis(),
                    "command finished (mcp stream)",
                );
                code
            }
            Some(DrainOutcome::Shutdown) | None => {
                tracing::info!(command = %name, "mcp stream ended: server shut down");
                return; // No final response on shutdown.
            }
        };

        let buffered = BufferedOutput {
            stdout_lines,
            stderr_lines,
            exit_code,
        };
        let is_error = buffered.exit_code.map(|c| c != 0).unwrap_or(true);
        let text = format_output(&buffered);

        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{"type": "text", "text": text}],
                "isError": is_error,
            }
        });
        let _ = event_tx.send(Event::json(&response).event("message"));
        // event_tx is dropped here, closing the channel and ending the SSE stream.
    });

    UnboundedReceiverStream::new(event_rx)
}

// ---------------------------------------------------------------------------
// Route handler
// ---------------------------------------------------------------------------

/// `POST /mcp` — MCP Streamable HTTP transport endpoint.
///
/// Accepts a JSON-RPC 2.0 message, dispatches to the appropriate handler, and
/// returns either a JSON-RPC response or an SSE stream depending on the
/// `Accept` header and method.
///
/// For `tools/call`: when the client sends `Accept: text/event-stream` an SSE
/// stream is returned.  All other methods and non-SSE calls return JSON.
///
/// Notifications (messages without an `id`) are acknowledged with an empty
/// object `{}`.
///
/// Requires a valid `Authorization: Bearer <token>` header.
#[allow(private_interfaces)] // WantsEventStream is an internal request guard
#[post("/mcp", format = "json", data = "<body>")]
pub async fn mcp_post(
    _auth: AuthToken,
    wants_sse: WantsEventStream,
    body: Json<JsonRpcMessage>,
    config: &State<LoadedConfig>,
    limiter: &State<JobLimiter>,
    shutdown: Shutdown,
) -> McpResponse {
    let msg = body.into_inner();
    let id = msg.id;

    match msg.method.as_str() {
        // ------------------------------------------------------------------
        // MCP handshake
        // ------------------------------------------------------------------
        "initialize" => McpResponse::Json(Json(ok(
            id,
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": env!("CARGO_PKG_NAME"),
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        ))),

        // Notification: client signals it is ready.  No response body needed.
        "notifications/initialized" => McpResponse::Json(Json(json!({}))),

        // ------------------------------------------------------------------
        // Tool discovery
        // ------------------------------------------------------------------
        "tools/list" => {
            let tools: Vec<Value> = config.commands.iter().map(tool_definition).collect();
            McpResponse::Json(Json(ok(id, json!({ "tools": tools }))))
        }

        // ------------------------------------------------------------------
        // Tool invocation
        // ------------------------------------------------------------------
        "tools/call" => {
            let name = match msg.params.get("name").and_then(Value::as_str) {
                Some(n) => n.to_string(),
                None => {
                    return McpResponse::Json(Json(err(
                        id,
                        INVALID_PARAMS,
                        "missing required parameter 'name'",
                    )));
                }
            };

            let args: HashMap<String, Value> = match msg.params.get("arguments") {
                Some(v) => match serde_json::from_value(v.clone()) {
                    Ok(map) => map,
                    Err(_) => {
                        return McpResponse::Json(Json(err(
                            id,
                            INVALID_PARAMS,
                            "'arguments' must be an object",
                        )));
                    }
                },
                None => HashMap::new(),
            };

            let run_req = RunRequest {
                command: name,
                args,
            };

            let permit = match limiter.inner().try_acquire() {
                Ok(p) => p,
                Err(()) => {
                    return McpResponse::Json(Json(ok(
                        id,
                        json!({
                            "content": [{"type": "text", "text": "server is at maximum concurrent job capacity"}],
                            "isError": true,
                        }),
                    )));
                }
            };

            let validated = match validate::validate(&run_req, config) {
                Ok(v) => v,
                Err(e) => {
                    return McpResponse::Json(Json(ok(
                        id,
                        json!({
                            "content": [{"type": "text", "text": e.to_string()}],
                            "isError": true,
                        }),
                    )));
                }
            };

            if wants_sse.0 {
                let stream = stream_tools_call(validated, id, shutdown, permit);
                McpResponse::Sse(EventStream::from(stream))
            } else {
                let output = executor::run_command_buffered(validated, shutdown, permit).await;
                let is_error = output.exit_code.map(|c| c != 0).unwrap_or(true);
                let text = format_output(&output);
                McpResponse::Json(Json(ok(
                    id,
                    json!({
                        "content": [{"type": "text", "text": text}],
                        "isError": is_error,
                    }),
                )))
            }
        }

        // ------------------------------------------------------------------
        // Unknown methods
        // ------------------------------------------------------------------
        method => McpResponse::Json(Json(err(
            id,
            METHOD_NOT_FOUND,
            &format!("method '{method}' not found"),
        ))),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use regex::Regex;
    use serde_json::json;

    use crate::config::{LoadedArgSpec, LoadedArgType, LoadedCommandSpec};
    use crate::executor::BufferedOutput;

    use super::{format_output, tool_definition};

    fn make_cmd(args: Vec<LoadedArgSpec>) -> LoadedCommandSpec {
        LoadedCommandSpec {
            name: "cmd".into(),
            description: None,
            executable: PathBuf::from("/usr/bin/true"),
            working_dir: None,
            args,
            fixed_args: vec![],
            timeout_secs: None,
        }
    }

    // ------------------------------------------------------------------
    // format_output
    // ------------------------------------------------------------------

    #[test]
    fn format_output_exit_zero_no_output() {
        let out = BufferedOutput {
            stdout_lines: vec![],
            stderr_lines: vec![],
            exit_code: Some(0),
        };
        let text = format_output(&out);
        assert!(text.contains("Exit code: 0"), "got: {text}");
    }

    #[test]
    fn format_output_exit_nonzero() {
        let out = BufferedOutput {
            stdout_lines: vec![],
            stderr_lines: vec![],
            exit_code: Some(1),
        };
        let text = format_output(&out);
        assert!(text.contains("Exit code: 1"), "got: {text}");
    }

    #[test]
    fn format_output_killed_process() {
        let out = BufferedOutput {
            stdout_lines: vec![],
            stderr_lines: vec![],
            exit_code: None,
        };
        let text = format_output(&out);
        assert!(text.contains("killed"), "got: {text}");
    }

    #[test]
    fn format_output_stdout_included() {
        let out = BufferedOutput {
            stdout_lines: vec!["line one".into(), "line two".into()],
            stderr_lines: vec![],
            exit_code: Some(0),
        };
        let text = format_output(&out);
        assert!(text.contains("line one"), "got: {text}");
        assert!(text.contains("line two"), "got: {text}");
    }

    #[test]
    fn format_output_stderr_labelled() {
        let out = BufferedOutput {
            stdout_lines: vec![],
            stderr_lines: vec!["warning: something".into()],
            exit_code: Some(0),
        };
        let text = format_output(&out);
        assert!(text.contains("[stderr]"), "got: {text}");
        assert!(text.contains("warning: something"), "got: {text}");
    }

    #[test]
    fn format_output_stdout_before_stderr() {
        let out = BufferedOutput {
            stdout_lines: vec!["stdout line".into()],
            stderr_lines: vec!["stderr line".into()],
            exit_code: Some(0),
        };
        let text = format_output(&out);
        let stdout_pos = text.find("stdout line").unwrap();
        let stderr_pos = text.find("stderr line").unwrap();
        assert!(
            stdout_pos < stderr_pos,
            "stdout must appear before stderr in output"
        );
    }

    // ------------------------------------------------------------------
    // tool_definition — schema shapes per arg type
    // ------------------------------------------------------------------

    #[test]
    fn tool_definition_enum_arg() {
        let cmd = make_cmd(vec![LoadedArgSpec {
            name: "format".into(),
            description: Some("Output format.".into()),
            flag: "--format".into(),
            required: true,
            arg_type: LoadedArgType::Enum {
                values: vec!["text".into(), "json".into()],
            },
        }]);
        let tool = tool_definition(&cmd);
        let prop = &tool["inputSchema"]["properties"]["format"];
        assert_eq!(prop["type"], "string");
        assert_eq!(prop["enum"], json!(["text", "json"]));
        assert_eq!(prop["description"], "Output format.");
        let required = tool["inputSchema"]["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "format"));
    }

    #[test]
    fn tool_definition_bool_arg_optional() {
        let cmd = make_cmd(vec![LoadedArgSpec {
            name: "verbose".into(),
            description: None,
            flag: "--verbose".into(),
            required: false,
            arg_type: LoadedArgType::Bool,
        }]);
        let tool = tool_definition(&cmd);
        assert_eq!(
            tool["inputSchema"]["properties"]["verbose"]["type"],
            "boolean"
        );
        assert!(
            tool["inputSchema"]["required"].is_null()
                || tool["inputSchema"]["required"]
                    .as_array()
                    .is_some_and(|a| a.is_empty()),
            "optional arg must not appear in required"
        );
    }

    #[test]
    fn tool_definition_pattern_arg() {
        let cmd = make_cmd(vec![LoadedArgSpec {
            name: "name".into(),
            description: None,
            flag: "--name".into(),
            required: false,
            arg_type: LoadedArgType::Pattern {
                compiled: Regex::new("^[a-z]+$").unwrap(),
            },
        }]);
        let tool = tool_definition(&cmd);
        let prop = &tool["inputSchema"]["properties"]["name"];
        assert_eq!(prop["type"], "string");
        assert_eq!(prop["pattern"], "^[a-z]+$");
    }

    #[test]
    fn tool_definition_path_arg_description_mentions_dirs() {
        let cmd = make_cmd(vec![LoadedArgSpec {
            name: "output".into(),
            description: None,
            flag: "--output".into(),
            required: false,
            arg_type: LoadedArgType::Path {
                within: vec![PathBuf::from("/tmp")],
            },
        }]);
        let tool = tool_definition(&cmd);
        let prop = &tool["inputSchema"]["properties"]["output"];
        assert_eq!(prop["type"], "string");
        let desc = prop["description"].as_str().unwrap();
        assert!(desc.contains("Absolute path within"), "got: {desc}");
        assert!(desc.contains("/tmp"), "got: {desc}");
    }

    #[test]
    fn tool_definition_path_arg_with_description_prepends_it() {
        let cmd = make_cmd(vec![LoadedArgSpec {
            name: "output".into(),
            description: Some("Where to write.".into()),
            flag: "--output".into(),
            required: false,
            arg_type: LoadedArgType::Path {
                within: vec![PathBuf::from("/tmp")],
            },
        }]);
        let tool = tool_definition(&cmd);
        let desc = tool["inputSchema"]["properties"]["output"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.starts_with("Where to write."), "got: {desc}");
        assert!(desc.contains("Absolute path within"), "got: {desc}");
    }

    #[test]
    fn tool_definition_command_description_propagated() {
        let mut cmd = make_cmd(vec![]);
        cmd.description = Some("Does something useful.".into());
        let tool = tool_definition(&cmd);
        assert_eq!(tool["description"], "Does something useful.");
    }

    #[test]
    fn tool_definition_no_description_omitted() {
        let tool = tool_definition(&make_cmd(vec![]));
        assert!(
            tool.get("description").is_none() || tool["description"].is_null(),
            "absent description must not appear in tool JSON"
        );
    }
}
