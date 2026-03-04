//! Process spawning and output streaming.
//!
//! [`run_command`] takes a [`ValidatedCommand`] (produced by the validator),
//! spawns the executable, and streams its output back to the HTTP client as
//! [Server-Sent Events].  Each event's `data` field is a JSON object; the
//! stream always ends with an `exit` event.
//!
//! # Environment isolation
//!
//! Child processes are started with [`Command::env_clear`] followed by a small
//! allow-list of non-sensitive variables (see `safe_env`).  This ensures
//! that secrets such as `JANUS_TOKEN` are never inherited by child processes.
//!
//! # Client disconnect
//!
//! `kill_on_drop(true)` is set on the [`Child`] handle so that if the Rocket
//! handler is dropped (e.g. the HTTP client disconnects mid-stream) the child
//! process is terminated automatically.
//!
//! [`Child`]: tokio::process::Child
//! [Server-Sent Events]: https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events

use std::process::Stdio;

use rocket::Shutdown;
use rocket::response::stream::{Event, EventStream};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Duration, Instant};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::LinesStream;

use crate::validate::ValidatedCommand;

// ---------------------------------------------------------------------------
// SSE event payload types
// ---------------------------------------------------------------------------

/// A single event emitted over the SSE stream for a running command.
///
/// Events are serialised as JSON and sent as the `data` field of each
/// server-sent event.  The `type` key acts as a discriminant:
///
/// ```json
/// {"type":"stdout","data":"hello\n"}
/// {"type":"stderr","data":"warning: something\n"}
/// {"type":"exit","code":0}
/// ```
///
/// The `exit` event is always the last event in the stream.  `code` is `null`
/// when the process was killed by a signal or its exit status could not be
/// read.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputEvent {
    /// A line read from the child process's standard output.
    Stdout { data: String },
    /// A line read from the child process's standard error.
    Stderr { data: String },
    /// The child process exited.  Always the final event in the stream.
    Exit {
        /// Exit code, or `None` if the process was killed by a signal or the
        /// exit status could not be retrieved.
        code: Option<i32>,
    },
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
/// Each SSE `data` field is a JSON-serialised [`OutputEvent`]:
/// - `{"type":"stdout","data":"..."}` — a line from stdout
/// - `{"type":"stderr","data":"..."}` — a line from stderr
/// - `{"type":"exit","code":<int|null>}` — process exit (always the last event)
///
/// Stdout and stderr are merged and interleaved in arrival order.  A single
/// IO error on one stream is logged and skipped; the other stream continues
/// draining normally.
///
/// If the child cannot be spawned at all, a single `exit` event with
/// `code: null` is emitted immediately.
///
/// The `shutdown` handle is obtained from Rocket's managed shutdown mechanism.
/// When the server begins shutting down, the running child process is killed
/// immediately rather than waiting for it to finish naturally.
pub fn run_command(
    cmd: ValidatedCommand,
    mut shutdown: Shutdown,
    permit: Option<OwnedSemaphorePermit>,
) -> EventStream![] {
    EventStream! {
        // Hold the semaphore permit for the lifetime of this stream.
        // Dropping it at the end releases one concurrent-job slot.
        let _permit = permit;

        let start = std::time::Instant::now();
        tracing::info!(
            command = %cmd.name,
            executable = %cmd.executable.display(),
            argv = ?cmd.argv,
            "command started",
        );

        let mut child = match spawn_child(&cmd) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    command = %cmd.name,
                    executable = %cmd.executable.display(),
                    error = %e,
                    "failed to spawn command",
                );
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

        // Compute the optional deadline once, before the loop.
        let deadline: Option<Instant> = cmd.timeout_secs
            .map(|s| Instant::now() + Duration::from_secs(s));

        let mut output_bytes: u64 = 0;
        let cap: Option<u64> = cmd.output_bytes_max;

        loop {
            tokio::select! {
                // Priority order (biased): shutdown > timeout > output.
                biased;

                _ = &mut shutdown => {
                    tracing::info!(
                        command = %cmd.name,
                        "command killed: server shutting down",
                    );
                    let _ = child.kill().await;
                    return;
                }

                // Fire when the per-command deadline is reached; pending
                // forever when no timeout is configured.
                _ = async {
                    match deadline {
                        Some(dl) => tokio::time::sleep_until(dl).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    tracing::warn!(
                        command = %cmd.name,
                        timeout_secs = ?cmd.timeout_secs,
                        "command timed out",
                    );
                    let _ = child.kill().await;
                    yield Event::json(&OutputEvent::Exit { code: None });
                    return;
                }

                result = merged.next() => {
                    match result {
                        Some(Ok(Tagged::Stdout(line))) => {
                            output_bytes += line.len() as u64;
                            if cap.is_some_and(|max| output_bytes > max) {
                                tracing::warn!(
                                    command = %cmd.name,
                                    output_bytes,
                                    cap = ?cap,
                                    "output cap exceeded",
                                );
                                let _ = child.kill().await;
                                yield Event::json(&OutputEvent::Exit { code: None });
                                return;
                            }
                            yield Event::json(&OutputEvent::Stdout { data: line });
                        }
                        Some(Ok(Tagged::Stderr(line))) => {
                            output_bytes += line.len() as u64;
                            if cap.is_some_and(|max| output_bytes > max) {
                                tracing::warn!(
                                    command = %cmd.name,
                                    output_bytes,
                                    cap = ?cap,
                                    "output cap exceeded",
                                );
                                let _ = child.kill().await;
                                yield Event::json(&OutputEvent::Exit { code: None });
                                return;
                            }
                            yield Event::json(&OutputEvent::Stderr { data: line });
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                command = %cmd.name,
                                error = %e,
                                "IO error reading process output",
                            );
                            // Continue draining; don't abort on a single bad line.
                        }
                        None => break,
                    }
                }
            }
        }

        // Wait for the process to fully exit after the output streams close.
        let exit_code = match child.wait().await {
            Ok(status) => status.code(),
            Err(e) => {
                tracing::error!(
                    command = %cmd.name,
                    error = %e,
                    "error waiting for child process",
                );
                None
            }
        };

        tracing::info!(
            command = %cmd.name,
            exit_code = ?exit_code,
            duration_ms = start.elapsed().as_millis(),
            "command finished",
        );

        yield Event::json(&OutputEvent::Exit { code: exit_code });
    }
}

// ---------------------------------------------------------------------------
// Process builder
// ---------------------------------------------------------------------------

/// Spawns `cmd` as a child process with piped stdout/stderr.
///
/// The child's environment is cleared and replaced with the safe allow-list
/// returned by `safe_env`.  `kill_on_drop(true)` is set so that the child
/// is terminated if the returned [`Child`] handle is dropped.
///
/// On Unix, the child is placed in its own process group (`process_group(0)`)
/// so that terminal signals such as `SIGINT` and `SIGQUIT` sent to the
/// server's process group are not automatically forwarded to child processes.
///
/// [`Child`]: tokio::process::Child
fn spawn_child(cmd: &ValidatedCommand) -> std::io::Result<tokio::process::Child> {
    let working_dir = cmd
        .working_dir
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("/"));

    let mut command = Command::new(&cmd.executable);
    command
        .args(&cmd.argv)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Clear the server's environment to prevent leaking JANUS_TOKEN and
        // other sensitive variables into the child process.
        .env_clear()
        .envs(safe_env())
        // Kill the child if the Rocket handler is dropped (e.g. client disconnects).
        .kill_on_drop(true);

    // Isolate the child in its own process group so terminal signals sent to
    // the server's group are not automatically forwarded to child processes.
    #[cfg(unix)]
    command.process_group(0);

    command.spawn()
}

/// Returns a minimal, safe set of environment variables for child processes.
///
/// Only well-known, non-sensitive variables are forwarded from the server's
/// environment.  Everything else — including `JANUS_TOKEN` and any custom
/// variables the operator may have set — is stripped by [`Command::env_clear`].
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
