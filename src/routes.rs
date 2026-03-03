//! Rocket route handlers and HTTP request/response types.
//!
//! # Routes
//!
//! | Method | Path | Auth | Description |
//! |--------|------|------|-------------|
//! | `GET`  | `/health` | No | Liveness check |
//! | `GET`  | `/commands` | Yes | List available commands and their arguments |
//! | `POST` | `/run` | Yes | Validate and execute a command; stream output as SSE |
//!
//! All authenticated routes require an `Authorization: Bearer <token>` header.
//! Missing or invalid tokens result in a `401 Unauthorized` JSON response.
//!
//! # Error responses
//!
//! Error responses for the statuses handled by this module's custom catchers
//! (401, 404, 422, 500) all use the same JSON envelope:
//!
//! ```json
//! {"error": "<human-readable message>"}
//! ```
//!
//! Other error statuses (e.g. 400 for a syntactically invalid JSON body) fall
//! through to Rocket's default catcher and may return a plain-text body.

use std::collections::HashMap;

use rocket::http::Status;
use rocket::response::stream::EventStream;
use rocket::serde::json::Json;
use rocket::{Shutdown, State, catch, get, post};
use serde::{Deserialize, Serialize};

use crate::auth::AuthToken;
use crate::config::LoadedConfig;
use crate::executor;
use crate::validate::{self, ValidationError};

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

/// Request body for `POST /run`.
#[derive(Debug, Deserialize)]
pub struct RunRequest {
    /// Name of the command to execute, as declared in the config.
    pub command: String,
    /// Argument values keyed by argument name.  Unknown keys and missing
    /// required keys are rejected by the validator.  Absent optional keys are
    /// simply omitted from `argv`.
    #[serde(default)]
    pub args: HashMap<String, serde_json::Value>,
}

/// Common error envelope returned by all error responses.
#[derive(Serialize)]
pub struct ErrorBody {
    /// Human-readable description of the error.
    pub error: String,
}

/// Response body for `GET /health`.
#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
}

/// Describes one command as seen by the caller.
///
/// Intentionally omits the executable path and working directory — those are
/// server-side implementation details that callers have no need to see.
#[derive(Serialize)]
pub struct CommandInfo {
    /// Command name used in `POST /run` requests.
    pub name: String,
    /// Declared arguments in config order.
    pub args: Vec<ArgInfo>,
}

/// Describes one argument as seen by the caller.
#[derive(Serialize)]
pub struct ArgInfo {
    /// Argument name used in the `args` map of a `POST /run` request.
    pub name: String,
    /// Whether the argument must be supplied.
    pub required: bool,
    /// Validation type: `"enum"`, `"pattern"`, `"path"`, or `"bool"`.
    #[serde(rename = "type")]
    pub arg_type: &'static str,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /health` — liveness check, no authentication required.
///
/// Returns `200 OK` with `{"status":"ok"}`.  Intended for health checks and
/// load-balancer probes.
#[get("/health")]
pub fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// `GET /commands` — list the commands available to authenticated callers.
///
/// Returns a JSON array of [`CommandInfo`] objects, one per configured command,
/// in config file order.  Requires a valid bearer token.
#[get("/commands")]
pub fn commands(_auth: AuthToken, config: &State<LoadedConfig>) -> Json<Vec<CommandInfo>> {
    let infos = config
        .commands
        .iter()
        .map(|cmd| CommandInfo {
            name: cmd.name.clone(),
            args: cmd
                .args
                .iter()
                .map(|a| ArgInfo {
                    name: a.name.clone(),
                    required: a.required,
                    arg_type: a.arg_type.type_name(),
                })
                .collect(),
        })
        .collect();
    Json(infos)
}

/// `POST /run` — validate and execute a command, streaming output as SSE.
///
/// The request body must be `Content-Type: application/json` and deserialise
/// as a [`RunRequest`].  Requires a valid bearer token.
///
/// On success, responds with `200 OK` and a `text/event-stream` body.  Each
/// event's `data` field is a JSON object; see [`crate::executor::OutputEvent`]
/// for the schema.
///
/// # Error responses
///
/// | Status | Condition |
/// |--------|-----------|
/// | `401`  | Missing or invalid bearer token |
/// | `404`  | Command name not found in config |
/// | `422`  | Unknown args, missing required args, or invalid arg value |
#[post("/run", format = "json", data = "<request>")]
pub fn run(
    _auth: AuthToken,
    request: Json<RunRequest>,
    config: &State<LoadedConfig>,
    shutdown: Shutdown,
) -> Result<EventStream![], (Status, Json<ErrorBody>)> {
    let validated = match validate::validate(&request, config) {
        Ok(v) => v,
        Err(ValidationError::CommandNotFound) => {
            return Err((
                Status::NotFound,
                Json(ErrorBody {
                    error: format!("command '{}' not found", request.command),
                }),
            ));
        }
        Err(ValidationError::UnknownArgs(args)) => {
            return Err((
                Status::UnprocessableEntity,
                Json(ErrorBody {
                    error: format!("unknown args: {}", args.join(", ")),
                }),
            ));
        }
        Err(ValidationError::MissingRequiredArgs(args)) => {
            return Err((
                Status::UnprocessableEntity,
                Json(ErrorBody {
                    error: format!("missing required args: {}", args.join(", ")),
                }),
            ));
        }
        Err(ValidationError::InvalidArgValue { arg, reason }) => {
            return Err((
                Status::UnprocessableEntity,
                Json(ErrorBody {
                    error: format!("invalid value for '{}': {}", arg, reason),
                }),
            ));
        }
    };

    Ok(executor::run_command(validated, shutdown))
}

// ---------------------------------------------------------------------------
// Error catchers
// ---------------------------------------------------------------------------

/// Returns a JSON `401 Unauthorized` body instead of Rocket's default HTML.
#[catch(401)]
pub fn unauthorized() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "unauthorized".into(),
    })
}

/// Returns a JSON `404 Not Found` body instead of Rocket's default HTML.
#[catch(404)]
pub fn not_found() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "not found".into(),
    })
}

/// Returns a JSON `422 Unprocessable Entity` body instead of Rocket's default
/// HTML.  Triggered when the request body is malformed or has the wrong
/// `Content-Type`.
#[catch(422)]
pub fn unprocessable() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "unprocessable request — check Content-Type and request body".into(),
    })
}

/// Returns a JSON `500 Internal Server Error` body instead of Rocket's default
/// HTML.
#[catch(500)]
pub fn internal_error() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "internal server error".into(),
    })
}
