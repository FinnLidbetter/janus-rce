//! Configuration loading and startup validation.
//!
//! Configuration is processed in two phases:
//!
//! 1. **Deserialisation** — the raw TOML is parsed into [`Config`],
//!    [`CommandSpec`], and [`ArgSpec`].  These types mirror the file layout
//!    exactly and are only used during loading.
//!
//! 2. **Validation** — [`LoadedConfig::load`] converts the raw types into
//!    their `Loaded*` counterparts ([`LoadedConfig`], [`LoadedCommandSpec`],
//!    [`LoadedArgSpec`], [`LoadedArgType`]).  During this step the server
//!    checks that every executable exists and is runnable, compiles regex
//!    patterns, canonicalises `path` allow-lists, and resolves the auth token.
//!
//! Only the `Loaded*` types are used at request time.

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

/// Top-level raw configuration, deserialised directly from `janus.toml`.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Server bind/port/token settings.
    pub server: ServerConfig,
    /// Ordered list of commands the server may execute.
    pub commands: Vec<CommandSpec>,
}

/// Raw server settings from the `[server]` table.
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// TCP port to listen on.
    pub port: u16,
    /// Address to bind.  Defaults to `"127.0.0.1"`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Static bearer token.  May be `None` when the token is supplied via the
    /// `JANUS_TOKEN` environment variable instead.
    pub token: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}

/// Raw command specification from a `[[commands]]` entry.
#[derive(Debug, Deserialize, Clone)]
pub struct CommandSpec {
    /// Name used in API requests (`{"command": "<name>"}`).
    pub name: String,
    /// Absolute path to the executable.
    pub executable: PathBuf,
    /// Optional working directory; defaults to `/` when absent.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    /// Declared arguments, in the order they will be appended to `argv`.
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

/// Raw argument specification within a `[[commands]]` entry.
#[derive(Debug, Deserialize, Clone)]
pub struct ArgSpec {
    /// Argument name used in the JSON request body.
    pub name: String,
    /// CLI flag passed to the executable (e.g. `"--output"`).
    pub flag: String,
    /// Whether the caller must supply this argument.  Defaults to `false`.
    #[serde(default)]
    pub required: bool,
    /// Validation rule for the argument's value.
    #[serde(flatten)]
    pub arg_type: ArgType,
}

/// Validation rule for a single argument, tagged by `type` in TOML.
///
/// ```toml
/// # Enum — value must be one of the listed strings.
/// { type = "enum", values = ["text", "json"] }
///
/// # Pattern — value must match the regex.
/// { type = "pattern", pattern = "[a-zA-Z]+" }
///
/// # Path — value must be an absolute path within the listed directories.
/// { type = "path", within = ["/tmp"] }
///
/// # Bool — value must be a JSON boolean; true appends the flag, false omits it.
/// { type = "bool" }
/// ```
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArgType {
    /// Value must be one of the listed strings.
    Enum { values: Vec<String> },
    /// Value must match the given regular expression.
    Pattern { pattern: String },
    /// Value must be an absolute path that resolves within one of the listed
    /// directories.
    Path { within: Vec<PathBuf> },
    /// Value is a JSON boolean; `true` appends the flag, `false` omits it.
    Bool,
}

// ---------------------------------------------------------------------------
// Validated (runtime) types — pre-compiled, pre-canonicalized, ready to use
// ---------------------------------------------------------------------------

/// The fully-validated, ready-to-use server configuration.
///
/// Produced by [`LoadedConfig::load`] and placed in Rocket's managed state so
/// that route handlers can access it via [`rocket::State<LoadedConfig>`].
pub struct LoadedConfig {
    /// Server bind/port settings (token is stored separately in [`Self::token`]).
    pub server: ServerConfig,
    /// Validated command specifications, in config file order.
    pub commands: Vec<LoadedCommandSpec>,
    /// Resolved auth token.  `JANUS_TOKEN` environment variable takes
    /// precedence over the `server.token` config file value.
    pub token: String,
}

/// A validated command specification, ready for use at request time.
pub struct LoadedCommandSpec {
    /// Command name as it appears in API requests.
    pub name: String,
    /// Absolute path to the executable, verified to exist and be runnable at
    /// load time.
    pub executable: PathBuf,
    /// Working directory for the child process, or `None` to use `/`.
    pub working_dir: Option<PathBuf>,
    /// Validated argument specifications in declaration order.
    pub args: Vec<LoadedArgSpec>,
}

/// A validated argument specification with pre-compiled validation data.
pub struct LoadedArgSpec {
    /// Argument name as it appears in JSON requests.
    pub name: String,
    /// CLI flag appended to `argv` when the argument is present.
    pub flag: String,
    /// Whether the argument must be supplied by the caller.
    pub required: bool,
    /// Pre-compiled validation rule.
    pub arg_type: LoadedArgType,
}

/// Pre-compiled argument validation rule, derived from [`ArgType`] at load time.
pub enum LoadedArgType {
    /// Value must exactly match one of the listed strings.
    Enum { values: Vec<String> },
    /// Value must match the compiled (and anchored) regular expression.
    Pattern { compiled: Regex },
    /// Value must be an absolute path within one of the canonicalised
    /// directories.
    Path { within: Vec<PathBuf> },
    /// Value is a JSON boolean.
    Bool,
}

impl LoadedArgType {
    /// Returns the short type tag sent to callers in `GET /commands` responses.
    ///
    /// # Examples
    ///
    /// ```
    /// use janus_rce::config::LoadedArgType;
    ///
    /// assert_eq!(LoadedArgType::Bool.type_name(), "bool");
    /// assert_eq!(LoadedArgType::Enum { values: vec![] }.type_name(), "enum");
    /// ```
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
    /// Loads and validates a `janus.toml` configuration file.
    ///
    /// # Token resolution
    ///
    /// The auth token is resolved in this order:
    /// 1. `JANUS_TOKEN` environment variable (if set and non-empty).
    /// 2. `server.token` in the config file.
    ///
    /// If neither is present the function returns an error.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * `path` does not exist or cannot be parsed as TOML.
    /// * No auth token is configured.
    /// * Any command's executable is not an absolute path, does not exist, or
    ///   is not executable.
    /// * Any regex pattern fails to compile.
    /// * Any `path` argument's `within` directory does not exist.
    /// * Two commands share the same name.
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

    /// Returns the first command whose name matches `name`, or `None`.
    ///
    /// A linear scan is used because the number of configured commands is
    /// expected to be small.
    ///
    /// # Examples
    ///
    /// ```
    /// use janus_rce::config::{LoadedCommandSpec, LoadedConfig, ServerConfig};
    /// use std::path::PathBuf;
    ///
    /// let config = LoadedConfig {
    ///     server: ServerConfig { port: 8080, bind: "127.0.0.1".into(), token: None },
    ///     token: "s".into(),
    ///     commands: vec![LoadedCommandSpec {
    ///         name: "ping".into(),
    ///         executable: PathBuf::from("/usr/bin/true"),
    ///         working_dir: None,
    ///         args: vec![],
    ///     }],
    /// };
    ///
    /// assert!(config.find_command("ping").is_some());
    /// assert!(config.find_command("missing").is_none());
    /// ```
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
///
/// Without anchoring, a pattern like `[a-z]+` would match `"abc;"` because
/// the engine finds `"abc"` as a substring.  Anchoring prevents partial
/// matches and ensures the entire value is validated.
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
