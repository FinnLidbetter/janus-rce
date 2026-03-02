//! janus-rce — a configurable remote-command-execution HTTP server.
//!
//! `janus-rce` lets operators expose a curated set of shell commands over
//! HTTP.  Each command is declared in a TOML configuration file with an
//! absolute executable path, an optional working directory, and a typed
//! argument specification.  Callers authenticate with a static bearer token,
//! submit requests as JSON, and receive the command's stdout/stderr streamed
//! back as [Server-Sent Events].
//!
//! # Security model
//!
//! * Only commands explicitly listed in the config may be run.
//! * Every argument value is validated against its declared type (`enum`,
//!   `pattern`, `path`, or `bool`) **and** screened for POSIX shell
//!   metacharacters before the process is spawned.
//! * Processes are launched with [`std::process::Command`] — no shell is
//!   involved — and with a minimal, scrubbed environment so that server
//!   secrets such as `JANUS_TOKEN` cannot leak to child processes.
//! * The bearer token is compared in constant time to resist timing attacks.
//!
//! # Module overview
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`config`] | TOML loading and startup validation of command specs |
//! | [`auth`] | Bearer-token request guard with constant-time comparison |
//! | [`validate`] | Per-request argument validation and `argv` construction |
//! | [`executor`] | Process spawning and SSE streaming |
//! | [`routes`] | Rocket route handlers and JSON request/response types |
//!
//! The [`build_rocket`] function wires all modules together and is used by
//! both the production binary and integration tests.
//!
//! [Server-Sent Events]: https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events

pub mod auth;
pub mod config;
pub mod executor;
pub mod routes;
pub mod validate;

/// Constructs a fully-wired [`rocket::Rocket`] instance from a figment and a
/// validated configuration.
///
/// Mounting routes and registering error catchers are centralised here so that
/// the production binary (`main.rs`) and integration tests use identical
/// routing without duplicating configuration.
///
/// # Arguments
///
/// * `figment` — A Rocket figment, typically [`rocket::Config::figment()`]
///   with `port` and `address` merged in from the janus config.
/// * `config` — A [`config::LoadedConfig`] that has already passed startup
///   validation.  It is placed in Rocket's managed state and injected into
///   route handlers via [`rocket::State`].
pub fn build_rocket(
    figment: rocket::figment::Figment,
    config: config::LoadedConfig,
) -> rocket::Rocket<rocket::Build> {
    rocket::custom(figment)
        .manage(config)
        .mount(
            "/",
            rocket::routes![routes::health, routes::commands, routes::run],
        )
        .register(
            "/",
            rocket::catchers![
                routes::unauthorized,
                routes::not_found,
                routes::unprocessable,
                routes::internal_error,
            ],
        )
}
