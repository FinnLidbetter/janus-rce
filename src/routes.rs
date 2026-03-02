use std::collections::HashMap;

use rocket::http::Status;
use rocket::response::stream::EventStream;
use rocket::serde::json::Json;
use rocket::{State, catch, get, post};
use serde::{Deserialize, Serialize};

use crate::auth::AuthToken;
use crate::config::LoadedConfig;
use crate::executor;
use crate::validate::{self, ValidationError};

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RunRequest {
    pub command: String,
    #[serde(default)]
    pub args: HashMap<String, serde_json::Value>,
}

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
}

/// Describes one command as seen by the caller — no executable paths or
/// working directories are included.
#[derive(Serialize)]
pub struct CommandInfo {
    pub name: String,
    pub args: Vec<ArgInfo>,
}

#[derive(Serialize)]
pub struct ArgInfo {
    pub name: String,
    pub required: bool,
    #[serde(rename = "type")]
    pub arg_type: &'static str,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /health` — liveness check, no authentication required.
#[get("/health")]
pub fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// `GET /commands` — list allowed commands and their declared args.
/// Requires a valid bearer token.
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
/// Requires a valid bearer token.
#[post("/run", format = "json", data = "<request>")]
pub fn run(
    _auth: AuthToken,
    request: Json<RunRequest>,
    config: &State<LoadedConfig>,
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

    Ok(executor::run_command(validated))
}

// ---------------------------------------------------------------------------
// Error catchers
// ---------------------------------------------------------------------------

/// Return JSON for 401 Unauthorized instead of Rocket's default HTML page.
#[catch(401)]
pub fn unauthorized() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "unauthorized".into(),
    })
}

/// Return JSON for 404 Not Found instead of Rocket's default HTML page.
#[catch(404)]
pub fn not_found() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "not found".into(),
    })
}

/// Return JSON for 422 Unprocessable Entity (e.g. malformed request body).
#[catch(422)]
pub fn unprocessable() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "unprocessable request — check Content-Type and request body".into(),
    })
}

/// Return JSON for 500 Internal Server Error.
#[catch(500)]
pub fn internal_error() -> Json<ErrorBody> {
    Json(ErrorBody {
        error: "internal server error".into(),
    })
}
