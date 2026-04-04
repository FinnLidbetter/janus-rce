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
//! | `tools/call` | Executes a command and returns buffered output |
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

use rocket::serde::json::Json;
use rocket::{Shutdown, State, post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::AuthToken;
use crate::config::{LoadedArgType, LoadedCommandSpec, LoadedConfig};
use crate::executor::{self, BufferedOutput};
use crate::routes::{JobLimiter, RunRequest};
use crate::validate;

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
// Route handler
// ---------------------------------------------------------------------------

/// `POST /mcp` — MCP Streamable HTTP transport endpoint.
///
/// Accepts a JSON-RPC 2.0 message, dispatches to the appropriate handler, and
/// returns a JSON-RPC response.  Notifications (messages without an `id`) are
/// acknowledged with an empty object `{}`.
///
/// Requires a valid `Authorization: Bearer <token>` header.
#[post("/mcp", format = "json", data = "<body>")]
pub async fn mcp_post(
    _auth: AuthToken,
    body: Json<JsonRpcMessage>,
    config: &State<LoadedConfig>,
    limiter: &State<JobLimiter>,
    shutdown: Shutdown,
) -> Json<Value> {
    let msg = body.into_inner();
    let id = msg.id;

    Json(match msg.method.as_str() {
        // ------------------------------------------------------------------
        // MCP handshake
        // ------------------------------------------------------------------
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": env!("CARGO_PKG_NAME"),
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        ),

        // Notification: client signals it is ready.  No response body needed.
        "notifications/initialized" => json!({}),

        // ------------------------------------------------------------------
        // Tool discovery
        // ------------------------------------------------------------------
        "tools/list" => {
            let tools: Vec<Value> = config.commands.iter().map(tool_definition).collect();
            ok(id, json!({ "tools": tools }))
        }

        // ------------------------------------------------------------------
        // Tool invocation
        // ------------------------------------------------------------------
        "tools/call" => {
            let name = match msg.params.get("name").and_then(Value::as_str) {
                Some(n) => n.to_string(),
                None => {
                    return Json(err(id, INVALID_PARAMS, "missing required parameter 'name'"));
                }
            };

            let args: HashMap<String, Value> = match msg.params.get("arguments") {
                Some(v) => match serde_json::from_value(v.clone()) {
                    Ok(map) => map,
                    Err(_) => {
                        return Json(err(id, INVALID_PARAMS, "'arguments' must be an object"));
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
                    return Json(ok(
                        id,
                        json!({
                            "content": [{"type": "text", "text": "server is at maximum concurrent job capacity"}],
                            "isError": true,
                        }),
                    ));
                }
            };

            let validated = match validate::validate(&run_req, config) {
                Ok(v) => v,
                Err(e) => {
                    return Json(ok(
                        id,
                        json!({
                            "content": [{"type": "text", "text": e.to_string()}],
                            "isError": true,
                        }),
                    ));
                }
            };

            let output = executor::run_command_buffered(validated, shutdown, permit).await;
            let is_error = output.exit_code.map(|c| c != 0).unwrap_or(true);
            let text = format_output(&output);

            ok(
                id,
                json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": is_error,
                }),
            )
        }

        // ------------------------------------------------------------------
        // Unknown methods
        // ------------------------------------------------------------------
        method => err(
            id,
            METHOD_NOT_FOUND,
            &format!("method '{method}' not found"),
        ),
    })
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
