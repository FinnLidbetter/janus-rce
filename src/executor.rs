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
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Duration, Instant};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::LinesStream;

use crate::validate::ValidatedCommand;

// ---------------------------------------------------------------------------
// Output types
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

/// Output collected from a single buffered command run.
pub struct BufferedOutput {
    /// Lines captured from the child's standard output, in arrival order.
    pub stdout_lines: Vec<String>,
    /// Lines captured from the child's standard error, in arrival order.
    pub stderr_lines: Vec<String>,
    /// Exit code, or `None` if the process was killed (timeout, output cap,
    /// or server shutdown).
    pub exit_code: Option<i32>,
}

// Internal tag used when draining merged streams.
enum Tagged {
    Stdout(String),
    Stderr(String),
}

/// Outcome of a [`drain_child`] call.
enum DrainOutcome {
    /// Process exited naturally or was killed by timeout / output cap.
    /// Callers should emit an exit event with the given code.
    Exited(Option<i32>),
    /// The server shutdown signal fired and the child was killed.
    /// Callers should NOT emit an exit event (the stream is torn down).
    Shutdown,
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

// ---------------------------------------------------------------------------
// Drain loop
// ---------------------------------------------------------------------------

/// Drives the select loop for a running child process.
///
/// Reads lines from the merged stdout/stderr stream, enforces the per-command
/// timeout and output cap, and sends each [`Tagged`] line through `tx`.
/// Returns the exit code of the process, or `None` if it was killed due to
/// server shutdown, timeout, or an exceeded output cap.
///
/// `tx` is dropped when this function returns (naturally or early), which
/// closes the channel and signals the receiver that no more lines are coming.
async fn drain_child(
    name: String,
    timeout_secs: Option<u64>,
    output_bytes_max: Option<u64>,
    mut child: tokio::process::Child,
    mut shutdown: Shutdown,
    tx: UnboundedSender<Tagged>,
) -> DrainOutcome {
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let stdout_stream =
        LinesStream::new(BufReader::new(stdout).lines()).map(|r| r.map(Tagged::Stdout));
    let stderr_stream =
        LinesStream::new(BufReader::new(stderr).lines()).map(|r| r.map(Tagged::Stderr));
    let mut merged = stdout_stream.merge(stderr_stream);

    let deadline: Option<Instant> = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
    let mut output_bytes: u64 = 0;
    let cap = output_bytes_max;

    loop {
        tokio::select! {
            // Priority order (biased): shutdown > timeout > output.
            biased;

            _ = &mut shutdown => {
                tracing::info!(command = %name, "command killed: server shutting down");
                let _ = child.kill().await;
                return DrainOutcome::Shutdown;
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
                    command = %name,
                    timeout_secs = ?timeout_secs,
                    "command timed out",
                );
                let _ = child.kill().await;
                return DrainOutcome::Exited(None);
            }

            result = merged.next() => {
                match result {
                    Some(Ok(tagged)) => {
                        let line_len = match &tagged {
                            Tagged::Stdout(s) | Tagged::Stderr(s) => s.len(),
                        };
                        output_bytes += line_len as u64;
                        if cap.is_some_and(|max| output_bytes > max) {
                            tracing::warn!(
                                command = %name,
                                output_bytes,
                                cap = ?cap,
                                "output cap exceeded",
                            );
                            let _ = child.kill().await;
                            return DrainOutcome::Exited(None);
                        }
                        // Ignore send errors: the receiver may have been dropped
                        // (e.g. the EventStream was torn down on client disconnect).
                        let _ = tx.send(tagged);
                    }
                    Some(Err(e)) => {
                        tracing::warn!(
                            command = %name,
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
                command = %name,
                error = %e,
                "error waiting for child process",
            );
            None
        }
    };
    DrainOutcome::Exited(exit_code)
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
    shutdown: Shutdown,
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

        let child = match spawn_child(&cmd) {
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

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Tagged>();
        let name = cmd.name.clone();
        let drain_fut = drain_child(
            name.clone(),
            cmd.timeout_secs,
            cmd.output_bytes_max,
            child,
            shutdown,
            tx,
        );
        tokio::pin!(drain_fut);

        let mut outcome: Option<DrainOutcome> = None;

        // Interleave drain_child with yielding received events.
        //
        // `biased;` ensures that when drain_child completes and drops `tx`
        // at the same instant rx signals EOF, the drain_fut arm fires first
        // and we always capture the outcome.
        loop {
            tokio::select! {
                biased;
                result = &mut drain_fut => {
                    outcome = Some(result);
                    break;
                }
                tagged = rx.recv() => {
                    match tagged {
                        Some(Tagged::Stdout(line)) => {
                            yield Event::json(&OutputEvent::Stdout { data: line });
                        }
                        Some(Tagged::Stderr(line)) => {
                            yield Event::json(&OutputEvent::Stderr { data: line });
                        }
                        // tx dropped before drain_fut fired; shouldn't happen
                        // with biased select but handle gracefully.
                        None => break,
                    }
                }
            }
        }

        // Drain any lines that arrived in the channel before drain_fut returned.
        while let Some(tagged) = rx.recv().await {
            match tagged {
                Tagged::Stdout(line) => yield Event::json(&OutputEvent::Stdout { data: line }),
                Tagged::Stderr(line) => yield Event::json(&OutputEvent::Stderr { data: line }),
            }
        }

        match outcome {
            Some(DrainOutcome::Shutdown) | None => {
                // Server is shutting down: end the stream without an exit event.
                tracing::info!(command = %name, "stream ended: server shut down");
            }
            Some(DrainOutcome::Exited(exit_code)) => {
                tracing::info!(
                    command = %name,
                    exit_code = ?exit_code,
                    duration_ms = start.elapsed().as_millis(),
                    "command finished",
                );
                yield Event::json(&OutputEvent::Exit { code: exit_code });
            }
        }
    }
}

/// Spawns `cmd`, collects its full output, and returns it as a
/// [`BufferedOutput`].
///
/// Behaves identically to [`run_command`] with respect to timeouts, output
/// caps, and server shutdown, but accumulates output in memory rather than
/// streaming it as SSE events.  Intended for callers (e.g. the MCP handler)
/// that need the complete output before returning a response.
pub async fn run_command_buffered(
    cmd: ValidatedCommand,
    shutdown: Shutdown,
    permit: Option<OwnedSemaphorePermit>,
) -> BufferedOutput {
    // Hold the semaphore permit for the duration of this call.
    let _permit = permit;

    let start = std::time::Instant::now();
    tracing::info!(
        command = %cmd.name,
        executable = %cmd.executable.display(),
        argv = ?cmd.argv,
        "command started (buffered)",
    );

    let child = match spawn_child(&cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                command = %cmd.name,
                executable = %cmd.executable.display(),
                error = %e,
                "failed to spawn command",
            );
            return BufferedOutput {
                stdout_lines: vec![],
                stderr_lines: vec![],
                exit_code: None,
            };
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Tagged>();
    let name = cmd.name.clone();

    // drain_child sends lines through the channel and returns the outcome.
    // Lines accumulate in the unbounded channel while drain_child runs; we
    // collect them after drain_child returns (and drops tx, closing the channel).
    let outcome = drain_child(
        name.clone(),
        cmd.timeout_secs,
        cmd.output_bytes_max,
        child,
        shutdown,
        tx,
    )
    .await;

    let exit_code = match outcome {
        DrainOutcome::Exited(code) => code,
        DrainOutcome::Shutdown => None,
    };

    let mut stdout_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    while let Some(tagged) = rx.recv().await {
        match tagged {
            Tagged::Stdout(line) => stdout_lines.push(line),
            Tagged::Stderr(line) => stderr_lines.push(line),
        }
    }

    tracing::info!(
        command = %name,
        exit_code = ?exit_code,
        duration_ms = start.elapsed().as_millis(),
        "command finished (buffered)",
    );

    BufferedOutput {
        stdout_lines,
        stderr_lines,
        exit_code,
    }
}
