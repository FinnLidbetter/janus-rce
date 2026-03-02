use std::process::Stdio;

use rocket::response::stream::{Event, EventStream};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::LinesStream;

use crate::validate::ValidatedCommand;

// ---------------------------------------------------------------------------
// SSE event payload types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputEvent {
    Stdout { data: String },
    Stderr { data: String },
    Exit { code: Option<i32> },
}

// Internal tag used before merging streams.
enum Tagged {
    Stdout(String),
    Stderr(String),
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// Spawns `cmd` and streams its stdout/stderr as Server-Sent Events.
///
/// Each SSE data field is a JSON object with a `type` key:
/// - `{"type":"stdout","data":"..."}` — a line from stdout
/// - `{"type":"stderr","data":"..."}` — a line from stderr
/// - `{"type":"exit","code":<int|null>}` — process exit (always the last event)
pub fn run_command(cmd: ValidatedCommand) -> EventStream![] {
    EventStream! {
        let mut child = match spawn_child(&cmd) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to spawn '{}': {}", cmd.executable.display(), e);
                yield Event::json(&OutputEvent::Exit { code: None });
                return;
            }
        };

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Wrap each handle: AsyncRead -> BufReader -> Lines -> Stream, then tag.
        let stdout_stream = LinesStream::new(BufReader::new(stdout).lines())
            .map(|r| r.map(Tagged::Stdout));
        let stderr_stream = LinesStream::new(BufReader::new(stderr).lines())
            .map(|r| r.map(Tagged::Stderr));

        // Merge interleaves items as they arrive.
        let mut merged = stdout_stream.merge(stderr_stream);

        while let Some(result) = merged.next().await {
            match result {
                Ok(Tagged::Stdout(line)) => {
                    yield Event::json(&OutputEvent::Stdout { data: line });
                }
                Ok(Tagged::Stderr(line)) => {
                    yield Event::json(&OutputEvent::Stderr { data: line });
                }
                Err(e) => {
                    tracing::warn!("IO error reading process output: {}", e);
                    // Continue draining the stream; don't abort on a single bad line.
                }
            }
        }

        // Wait for the process to fully exit after the output streams close.
        let exit_code = match child.wait().await {
            Ok(status) => status.code(),
            Err(e) => {
                tracing::error!("Error waiting for child process: {}", e);
                None
            }
        };

        yield Event::json(&OutputEvent::Exit { code: exit_code });
    }
}

// ---------------------------------------------------------------------------
// Process builder
// ---------------------------------------------------------------------------

fn spawn_child(cmd: &ValidatedCommand) -> std::io::Result<tokio::process::Child> {
    let working_dir = cmd
        .working_dir
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("/"));

    Command::new(&cmd.executable)
        .args(&cmd.argv)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Clear the server's environment to prevent leaking JANUS_TOKEN and
        // other sensitive variables into the child process.
        .env_clear()
        .envs(safe_env())
        // Kill the child if the Rocket handler is dropped (e.g. client disconnects).
        .kill_on_drop(true)
        .spawn()
}

/// Returns a minimal, safe set of environment variables for child processes.
///
/// This prevents the child from inheriting sensitive variables (e.g. `JANUS_TOKEN`)
/// from the server's environment while still allowing typical Xcode tooling to work.
fn safe_env() -> Vec<(&'static str, String)> {
    let mut vars: Vec<(&'static str, String)> = vec![
        (
            "PATH",
            "/usr/bin:/usr/local/bin:/usr/bin/xcode-select".to_string(),
        ),
        ("LANG", "en_US.UTF-8".to_string()),
    ];

    // Pass through a small allowlist of non-sensitive variables if they exist.
    for key in &["HOME", "USER", "TMPDIR", "DEVELOPER_DIR"] {
        if let Ok(val) = std::env::var(key) {
            vars.push((key, val));
        }
    }

    vars
}
