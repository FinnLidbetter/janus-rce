use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use figment::{
    Figment,
    providers::{Format, Toml},
};
use regex::Regex;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Raw (deserialized) types — direct representation of janus.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub commands: Vec<CommandSpec>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub port: u16,
    #[serde(default = "default_bind")]
    pub bind: String,
    pub token: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct CommandSpec {
    pub name: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ArgSpec {
    pub name: String,
    pub flag: String,
    #[serde(default)]
    pub required: bool,
    #[serde(flatten)]
    pub arg_type: ArgType,
}

/// The discriminated union for arg validation rules.
/// The `type` field in TOML is the tag.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArgType {
    Enum { values: Vec<String> },
    Pattern { pattern: String },
    Path { within: Vec<PathBuf> },
    Bool,
}

// ---------------------------------------------------------------------------
// Validated (runtime) types — pre-compiled, pre-canonicalized, ready to use
// ---------------------------------------------------------------------------

pub struct LoadedConfig {
    pub server: ServerConfig,
    pub commands: Vec<LoadedCommandSpec>,
    /// Resolved token: JANUS_TOKEN env var wins over config file value.
    pub token: String,
}

pub struct LoadedCommandSpec {
    pub name: String,
    pub executable: PathBuf,
    pub working_dir: Option<PathBuf>,
    pub args: Vec<LoadedArgSpec>,
}

pub struct LoadedArgSpec {
    pub name: String,
    pub flag: String,
    pub required: bool,
    pub arg_type: LoadedArgType,
}

pub enum LoadedArgType {
    Enum { values: Vec<String> },
    Pattern { compiled: Regex },
    Path { within: Vec<PathBuf> },
    Bool,
}

impl LoadedArgType {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Enum { .. } => "enum",
            Self::Pattern { .. } => "pattern",
            Self::Path { .. } => "path",
            Self::Bool => "bool",
        }
    }
}

// ---------------------------------------------------------------------------
// Loading and validation
// ---------------------------------------------------------------------------

impl LoadedConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Config = Figment::new()
            .merge(Toml::file(path))
            .extract()
            .with_context(|| format!("loading config from '{}'", path.display()))?;

        // Resolve token: env var takes precedence over config file value.
        let token = match std::env::var("JANUS_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => config.server.token.clone().context(
                "No token configured: set JANUS_TOKEN env var or server.token in config",
            )?,
        };

        // Validate and compile each command spec.
        let mut commands = Vec::with_capacity(config.commands.len());
        for cmd in &config.commands {
            commands.push(
                LoadedCommandSpec::validate(cmd)
                    .with_context(|| format!("validating command '{}'", cmd.name))?,
            );
        }

        // Reject duplicate command names — fail early with a clear message.
        let mut seen: HashSet<&str> = HashSet::new();
        for cmd in &commands {
            if !seen.insert(cmd.name.as_str()) {
                bail!("Duplicate command name: '{}'", cmd.name);
            }
        }

        Ok(LoadedConfig {
            server: config.server,
            commands,
            token,
        })
    }

    /// Linear scan — config sizes are small so this is fine.
    pub fn find_command(&self, name: &str) -> Option<&LoadedCommandSpec> {
        self.commands.iter().find(|c| c.name == name)
    }
}

impl LoadedCommandSpec {
    fn validate(spec: &CommandSpec) -> Result<Self> {
        // Require absolute paths to prevent PATH-injection attacks.
        if !spec.executable.is_absolute() {
            bail!(
                "Executable '{}' must be an absolute path",
                spec.executable.display()
            );
        }

        // Verify the binary exists and is executable.
        let meta = std::fs::metadata(&spec.executable)
            .with_context(|| format!("stat '{}'", spec.executable.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                bail!(
                    "Executable '{}' does not have execute permission",
                    spec.executable.display()
                );
            }
        }

        // meta is used only for the unix permission check above; suppress the
        // unused-variable warning on non-unix platforms.
        let _ = meta;

        let mut args = Vec::with_capacity(spec.args.len());
        for arg in &spec.args {
            args.push(LoadedArgSpec::validate(arg).with_context(|| format!("arg '{}'", arg.name))?);
        }

        Ok(LoadedCommandSpec {
            name: spec.name.clone(),
            executable: spec.executable.clone(),
            working_dir: spec.working_dir.clone(),
            args,
        })
    }
}

impl LoadedArgSpec {
    fn validate(spec: &ArgSpec) -> Result<Self> {
        let arg_type = match &spec.arg_type {
            ArgType::Enum { values } => {
                if values.is_empty() {
                    bail!("enum arg '{}' must have at least one value", spec.name);
                }
                LoadedArgType::Enum {
                    values: values.clone(),
                }
            }
            ArgType::Pattern { pattern } => {
                let anchored = enforce_anchoring(pattern);
                let compiled = Regex::new(&anchored)
                    .with_context(|| format!("invalid regex pattern '{}'", pattern))?;
                LoadedArgType::Pattern { compiled }
            }
            ArgType::Path { within } => {
                let canon: Result<Vec<PathBuf>> = within
                    .iter()
                    .map(|p| {
                        p.canonicalize().with_context(|| {
                            format!(
                                "within path '{}' does not exist or cannot be resolved",
                                p.display()
                            )
                        })
                    })
                    .collect();
                LoadedArgType::Path { within: canon? }
            }
            ArgType::Bool => LoadedArgType::Bool,
        };

        Ok(LoadedArgSpec {
            name: spec.name.clone(),
            flag: spec.flag.clone(),
            required: spec.required,
            arg_type,
        })
    }
}

/// Ensures a regex pattern is anchored with `^` and `$`.
/// This prevents partial matches that could allow unintended values.
fn enforce_anchoring(pattern: &str) -> String {
    let start = if pattern.starts_with('^') { "" } else { "^" };
    let end = if pattern.ends_with('$') { "" } else { "$" };
    format!("{start}{pattern}{end}")
}

#[cfg(test)]
mod tests {
    use super::enforce_anchoring;

    #[test]
    fn anchoring_adds_both() {
        assert_eq!(enforce_anchoring("foo"), "^foo$");
    }

    #[test]
    fn anchoring_preserves_existing() {
        assert_eq!(enforce_anchoring("^foo$"), "^foo$");
    }

    #[test]
    fn anchoring_adds_missing_end() {
        assert_eq!(enforce_anchoring("^foo"), "^foo$");
    }

    #[test]
    fn anchoring_adds_missing_start() {
        assert_eq!(enforce_anchoring("foo$"), "^foo$");
    }
}
