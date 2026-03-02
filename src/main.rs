use std::path::Path;

use anyhow::Context;
use janus_rce::config::LoadedConfig;

#[rocket::main]
async fn main() -> anyhow::Result<()> {
    // Config file path: JANUS_CONFIG env var, falling back to ./janus.toml.
    let config_path_str =
        std::env::var("JANUS_CONFIG").unwrap_or_else(|_| "janus.toml".to_string());
    let config_path = Path::new(&config_path_str);

    // Use eprintln! here — Rocket's tracing subscriber is not yet initialised.
    eprintln!("janus-rce: loading config from '{}'", config_path.display());

    let config = LoadedConfig::load(config_path)
        .with_context(|| format!("failed to load config from '{}'", config_path.display()))?;

    eprintln!("janus-rce: {} command(s) loaded", config.commands.len());
    for cmd in &config.commands {
        eprintln!(
            "janus-rce:   '{}' -> {}",
            cmd.name,
            cmd.executable.display()
        );
    }

    // Extract bind settings before moving `config` into managed state.
    // In Rocket master, address and port are no longer fields on rocket::Config;
    // they are configured by merging figment keys over the default provider.
    let port = config.server.port;
    let bind = config.server.bind.clone();

    eprintln!("janus-rce: listening on {}:{}", bind, port);

    // Build on top of Rocket's default figment (which reads Rocket.toml and
    // ROCKET_* env vars) and overlay our own port/address.
    let figment = rocket::Config::figment()
        .merge(("port", port))
        .merge(("address", &bind));

    janus_rce::build_rocket(figment, config)
        .launch()
        .await
        .map_err(|e| anyhow::anyhow!("Rocket server terminated with an error: {e}"))?;

    Ok(())
}
