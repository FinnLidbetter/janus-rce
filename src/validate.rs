//! Per-request validation of [`RunRequest`]s against the loaded configuration.
//!
//! The entry point is [`validate`], which performs four checks in order:
//!
//! 1. **Command lookup** — the requested command name must exist in the config.
//! 2. **Unknown args** — every key in the request must be declared in the
//!    command's argument list.
//! 3. **Required args** — every required argument must be present.
//! 4. **Value validation** — each argument value is checked for shell
//!    metacharacters and then validated against its declared type (`enum`,
//!    `pattern`, `path`, or `bool`).
//!
//! On success, [`validate`] returns a [`ValidatedCommand`] that the executor
//! can spawn directly without further checking.

use std::path::PathBuf;

use crate::config::{LoadedArgType, LoadedConfig};
use crate::routes::RunRequest;

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// A fully-validated command ready to pass to the executor.
///
/// All values have been type-checked, screened for shell metacharacters, and
/// ordered according to the config declaration.  No further validation is
/// needed before spawning.
#[derive(Debug)]
pub struct ValidatedCommand {
    /// Config name of the command (used in audit logs).
    pub name: String,
    /// Absolute path to the executable.
    pub executable: PathBuf,
    /// Working directory for the child process, or `None` to use `/`.
    pub working_dir: Option<PathBuf>,
    /// Flat, ordered list of CLI arguments with no shell interpretation.
    /// Flags and their values appear in config declaration order.
    pub argv: Vec<String>,
}

/// Errors that [`validate`] can return.
#[derive(Debug)]
pub enum ValidationError {
    /// The requested command name was not found in the config.
    CommandNotFound,
    /// The request contained argument keys not declared in the config.
    /// The inner `Vec` lists the unknown names.
    UnknownArgs(Vec<String>),
    /// One or more required arguments were absent from the request.
    /// The inner `Vec` lists the missing names.
    MissingRequiredArgs(Vec<String>),
    /// An argument value failed type validation or the shell metacharacter
    /// check.
    InvalidArgValue {
        /// Name of the offending argument.
        arg: String,
        /// Human-readable explanation of why the value was rejected.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Shell metacharacter guard
// ---------------------------------------------------------------------------

/// Returns an error if `value` contains any character that has special meaning
/// in POSIX-compatible shells.
///
/// Although arguments are passed to child processes via `Command::args()` (no
/// shell involved), child programs may themselves invoke a shell or call
/// `system(3)`.  Rejecting metacharacters here provides defence-in-depth and
/// keeps all validated values safe for display, logging, and any downstream
/// shell invocation a child might make.
fn reject_shell_metacharacters(arg: &str, value: &str) -> Result<(), ValidationError> {
    // Full POSIX special-character set plus common control characters.
    const DANGEROUS: &[char] = &[
        // Command / argument separators
        '|', '&', ';', // Expansions
        '$', '`', // Grouping / redirection
        '(', ')', '{', '}', '<', '>', // Quoting
        '"', '\'', '\\', // Glob / pattern characters
        '*', '?', '[', ']', '^', // Miscellaneous shell specials
        '!', '~', '#', // Control characters
        '\n', '\r', '\0',
    ];

    if let Some(c) = value.chars().find(|c| DANGEROUS.contains(c)) {
        return Err(ValidationError::InvalidArgValue {
            arg: arg.to_string(),
            reason: format!("contains a disallowed shell metacharacter: {:?}", c),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validates a [`RunRequest`] against `config` and returns a
/// [`ValidatedCommand`] ready for the executor.
///
/// Validation is performed in four sequential steps; the first failure short-
/// circuits the rest.  See the [module documentation](self) for details.
///
/// # Errors
///
/// * [`ValidationError::CommandNotFound`] — unknown command name.
/// * [`ValidationError::UnknownArgs`] — request keys not in the command spec.
/// * [`ValidationError::MissingRequiredArgs`] — required keys absent from
///   request.
/// * [`ValidationError::InvalidArgValue`] — a value contained a shell
///   metacharacter or failed its type check.
///
/// # Examples
///
/// ```
/// use janus_rce::config::{LoadedCommandSpec, LoadedConfig, ServerConfig};
/// use janus_rce::routes::RunRequest;
/// use janus_rce::validate::{self, ValidationError};
/// use std::collections::HashMap;
/// use std::path::PathBuf;
///
/// let config = LoadedConfig {
///     server: ServerConfig { port: 8080, bind: "127.0.0.1".into(), token: None },
///     token: "secret".into(),
///     commands: vec![LoadedCommandSpec {
///         name: "ping".into(),
///         executable: PathBuf::from("/usr/bin/true"),
///         working_dir: None,
///         args: vec![],
///         fixed_args: vec![],
///     }],
/// };
///
/// // Valid request.
/// let ok = RunRequest { command: "ping".into(), args: HashMap::new() };
/// assert!(validate::validate(&ok, &config).is_ok());
///
/// // Unknown command.
/// let bad = RunRequest { command: "rm".into(), args: HashMap::new() };
/// assert!(matches!(validate::validate(&bad, &config), Err(ValidationError::CommandNotFound)));
/// ```
pub fn validate(
    request: &RunRequest,
    config: &LoadedConfig,
) -> Result<ValidatedCommand, ValidationError> {
    // 1. Look up the command.
    let spec = config
        .find_command(&request.command)
        .ok_or(ValidationError::CommandNotFound)?;

    // 2. Reject any args not declared in the config.
    let declared: std::collections::HashSet<&str> =
        spec.args.iter().map(|a| a.name.as_str()).collect();
    let unknown: Vec<String> = request
        .args
        .keys()
        .filter(|k| !declared.contains(k.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(ValidationError::UnknownArgs(unknown));
    }

    // 3. Ensure all required args are present.
    let missing: Vec<String> = spec
        .args
        .iter()
        .filter(|a| a.required && !request.args.contains_key(&a.name))
        .map(|a| a.name.clone())
        .collect();
    if !missing.is_empty() {
        return Err(ValidationError::MissingRequiredArgs(missing));
    }

    // 4. Validate each arg value and build the argv list in config declaration order.
    //    fixed_args are always prepended unconditionally.
    let mut argv: Vec<String> = spec.fixed_args.clone();

    for arg_spec in &spec.args {
        let raw_value = match request.args.get(&arg_spec.name) {
            Some(v) => v,
            None => continue, // optional and absent — skip
        };

        match &arg_spec.arg_type {
            LoadedArgType::Enum { values } => {
                let v = raw_value
                    .as_str()
                    .ok_or_else(|| ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: "must be a string".into(),
                    })?;
                reject_shell_metacharacters(&arg_spec.name, v)?;
                if !values.iter().any(|allowed| allowed == v) {
                    return Err(ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: format!("'{}' is not one of the allowed values: {:?}", v, values),
                    });
                }
                argv.push(arg_spec.flag.clone());
                argv.push(v.to_string());
            }

            LoadedArgType::Pattern { compiled } => {
                let v = raw_value
                    .as_str()
                    .ok_or_else(|| ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: "must be a string".into(),
                    })?;
                reject_shell_metacharacters(&arg_spec.name, v)?;
                if !compiled.is_match(v) {
                    return Err(ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: format!("'{}' does not match the required pattern", v),
                    });
                }
                argv.push(arg_spec.flag.clone());
                argv.push(v.to_string());
            }

            LoadedArgType::Path { within } => {
                let v = raw_value
                    .as_str()
                    .ok_or_else(|| ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: "must be a string".into(),
                    })?;
                reject_shell_metacharacters(&arg_spec.name, v)?;
                let candidate = std::path::Path::new(v);
                if !candidate.is_absolute() {
                    return Err(ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: "path must be absolute".into(),
                    });
                }
                let canonical =
                    candidate
                        .canonicalize()
                        .map_err(|_| ValidationError::InvalidArgValue {
                            arg: arg_spec.name.clone(),
                            reason: format!("'{}' cannot be resolved — does the path exist?", v),
                        })?;
                let allowed = within.iter().any(|w| canonical.starts_with(w));
                if !allowed {
                    return Err(ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: format!("'{}' is not within any allowed directory", v),
                    });
                }
                // Pass the original string value, not the canonicalized form.
                argv.push(arg_spec.flag.clone());
                argv.push(v.to_string());
            }

            LoadedArgType::Bool => {
                let v = raw_value
                    .as_bool()
                    .ok_or_else(|| ValidationError::InvalidArgValue {
                        arg: arg_spec.name.clone(),
                        reason: "must be a boolean".into(),
                    })?;
                if v {
                    // Boolean flag: present means "true", no value argument.
                    argv.push(arg_spec.flag.clone());
                }
                // false — omit the flag entirely.
            }
        }
    }

    Ok(ValidatedCommand {
        name: spec.name.clone(),
        executable: spec.executable.clone(),
        working_dir: spec.working_dir.clone(),
        argv,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use regex::Regex;
    use serde_json::json;

    use crate::config::{
        LoadedArgSpec, LoadedArgType, LoadedCommandSpec, LoadedConfig, ServerConfig,
    };
    use crate::routes::RunRequest;

    use super::{ValidatedCommand, ValidationError, validate};

    fn test_config() -> LoadedConfig {
        LoadedConfig {
            server: ServerConfig {
                port: 8080,
                bind: "127.0.0.1".into(),
                token: None,
            },
            token: "test-token".into(),
            commands: vec![LoadedCommandSpec {
                name: "greet".into(),
                executable: PathBuf::from("/usr/bin/true"),
                working_dir: None,
                args: vec![
                    LoadedArgSpec {
                        name: "format".into(),
                        flag: "--format".into(),
                        required: true,
                        arg_type: LoadedArgType::Enum {
                            values: vec!["text".into(), "json".into()],
                        },
                    },
                    LoadedArgSpec {
                        name: "name".into(),
                        flag: "--name".into(),
                        required: false,
                        arg_type: LoadedArgType::Pattern {
                            compiled: Regex::new("^[a-zA-Z]+$").unwrap(),
                        },
                    },
                    LoadedArgSpec {
                        name: "verbose".into(),
                        flag: "--verbose".into(),
                        required: false,
                        arg_type: LoadedArgType::Bool,
                    },
                    LoadedArgSpec {
                        name: "output".into(),
                        flag: "--output".into(),
                        required: false,
                        arg_type: LoadedArgType::Path {
                            within: vec![PathBuf::from("/tmp").canonicalize().unwrap()],
                        },
                    },
                ],
                fixed_args: vec!["--greet".into()],
            }],
        }
    }

    fn req(command: &str, args: Vec<(&str, serde_json::Value)>) -> RunRequest {
        RunRequest {
            command: command.into(),
            args: args
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn unknown_command() {
        let config = test_config();
        let result = validate(&req("nope", vec![]), &config);
        assert!(matches!(result, Err(ValidationError::CommandNotFound)));
    }

    #[test]
    fn unknown_args_rejected() {
        let config = test_config();
        let result = validate(&req("greet", vec![("unknown", json!("value"))]), &config);
        match result {
            Err(ValidationError::UnknownArgs(args)) => {
                assert_eq!(args, vec!["unknown"]);
            }
            other => panic!("expected UnknownArgs, got {:?}", other),
        }
    }

    #[test]
    fn missing_required_arg() {
        let config = test_config();
        let result = validate(&req("greet", vec![]), &config);
        match result {
            Err(ValidationError::MissingRequiredArgs(args)) => {
                assert_eq!(args, vec!["format"]);
            }
            other => panic!("expected MissingRequiredArgs, got {:?}", other),
        }
    }

    #[test]
    fn enum_valid() {
        let config = test_config();
        let result = validate(&req("greet", vec![("format", json!("text"))]), &config).unwrap();
        // fixed_args = ["--greet"] is prepended; user args follow.
        assert_eq!(result.argv, vec!["--greet", "--format", "text"]);
    }

    #[test]
    fn enum_invalid_value() {
        let config = test_config();
        let result = validate(&req("greet", vec![("format", json!("xml"))]), &config);
        assert!(matches!(
            result,
            Err(ValidationError::InvalidArgValue { .. })
        ));
    }

    #[test]
    fn enum_non_string_value() {
        let config = test_config();
        let result = validate(&req("greet", vec![("format", json!(42))]), &config);
        assert!(matches!(
            result,
            Err(ValidationError::InvalidArgValue { .. })
        ));
    }

    #[test]
    fn pattern_valid() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("name", json!("Alice"))],
            ),
            &config,
        )
        .unwrap();
        assert!(result.argv.contains(&"--name".to_string()));
        assert!(result.argv.contains(&"Alice".to_string()));
    }

    #[test]
    fn pattern_no_match() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("name", json!("Alice123"))],
            ),
            &config,
        );
        assert!(matches!(
            result,
            Err(ValidationError::InvalidArgValue { .. })
        ));
    }

    #[test]
    fn bool_true_adds_flag() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("verbose", json!(true))],
            ),
            &config,
        )
        .unwrap();
        assert!(result.argv.contains(&"--verbose".to_string()));
    }

    #[test]
    fn bool_false_omits_flag() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("verbose", json!(false))],
            ),
            &config,
        )
        .unwrap();
        assert!(!result.argv.contains(&"--verbose".to_string()));
    }

    #[test]
    fn bool_non_bool_rejected() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("verbose", json!("yes"))],
            ),
            &config,
        );
        assert!(matches!(
            result,
            Err(ValidationError::InvalidArgValue { .. })
        ));
    }

    #[test]
    fn path_valid() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("output", json!("/tmp"))],
            ),
            &config,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn path_relative_rejected() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("output", json!("relative"))],
            ),
            &config,
        );
        match result {
            Err(ValidationError::InvalidArgValue { reason, .. }) => {
                assert!(
                    reason.contains("must be absolute"),
                    "expected 'must be absolute' in: {reason}"
                );
            }
            other => panic!("expected InvalidArgValue, got {:?}", other),
        }
    }

    #[test]
    fn path_outside_rejected() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("output", json!("/usr"))],
            ),
            &config,
        );
        match result {
            Err(ValidationError::InvalidArgValue { reason, .. }) => {
                assert!(
                    reason.contains("not within any allowed"),
                    "expected 'not within any allowed' in: {reason}"
                );
            }
            other => panic!("expected InvalidArgValue, got {:?}", other),
        }
    }

    #[test]
    fn path_nonexistent_rejected() {
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![
                    ("format", json!("text")),
                    ("output", json!("/tmp/janus_no_such_path_xyz")),
                ],
            ),
            &config,
        );
        assert!(matches!(
            result,
            Err(ValidationError::InvalidArgValue { .. })
        ));
    }

    #[test]
    fn argv_order_follows_config() {
        let config = test_config();
        // Supply args in reverse declaration order; argv must still follow config order.
        let result = validate(
            &req(
                "greet",
                vec![
                    ("output", json!("/tmp")),
                    ("verbose", json!(true)),
                    ("name", json!("Alice")),
                    ("format", json!("text")),
                ],
            ),
            &config,
        )
        .unwrap();
        let argv = &result.argv;
        let format_pos = argv.iter().position(|x| x == "--format").unwrap();
        let name_pos = argv.iter().position(|x| x == "--name").unwrap();
        let verbose_pos = argv.iter().position(|x| x == "--verbose").unwrap();
        let output_pos = argv.iter().position(|x| x == "--output").unwrap();
        assert!(format_pos < name_pos);
        assert!(name_pos < verbose_pos);
        assert!(verbose_pos < output_pos);
    }

    #[test]
    fn executable_and_workdir_passthrough() {
        let config = test_config();
        let result = validate(&req("greet", vec![("format", json!("text"))]), &config).unwrap();
        assert_eq!(result.name, "greet");
        assert_eq!(result.executable, PathBuf::from("/usr/bin/true"));
        assert!(result.working_dir.is_none());
    }

    // ------------------------------------------------------------------
    // Shell metacharacter tests
    // ------------------------------------------------------------------

    /// Helper: assert that `result` is an InvalidArgValue whose reason
    /// mentions "shell metacharacter".
    fn assert_metachar_error(result: Result<ValidatedCommand, ValidationError>) {
        match result {
            Err(ValidationError::InvalidArgValue { reason, .. }) => {
                assert!(
                    reason.contains("shell metacharacter"),
                    "expected 'shell metacharacter' in reason: {reason}"
                );
            }
            other => panic!("expected InvalidArgValue (metachar), got {:?}", other),
        }
    }

    #[test]
    fn metachar_semicolon_in_enum_rejected() {
        // The metacharacter check fires before the "not in allowed values"
        // check, so the error reason must mention "shell metacharacter".
        let config = test_config();
        assert_metachar_error(validate(
            &req("greet", vec![("format", json!("text;"))]),
            &config,
        ));
    }

    #[test]
    fn metachar_pipe_in_enum_rejected() {
        let config = test_config();
        assert_metachar_error(validate(
            &req("greet", vec![("format", json!("text|json"))]),
            &config,
        ));
    }

    #[test]
    fn metachar_dollar_in_pattern_rejected() {
        // "$USER" could expand to a username in a shell — reject even though
        // the pattern `^[a-zA-Z]+$` would never match it anyway.
        let config = test_config();
        assert_metachar_error(validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("name", json!("$USER"))],
            ),
            &config,
        ));
    }

    #[test]
    fn metachar_backtick_in_pattern_rejected() {
        let config = test_config();
        assert_metachar_error(validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("name", json!("`id`"))],
            ),
            &config,
        ));
    }

    #[test]
    fn metachar_newline_in_enum_rejected() {
        let config = test_config();
        assert_metachar_error(validate(
            &req("greet", vec![("format", json!("text\nmore"))]),
            &config,
        ));
    }

    #[test]
    fn metachar_null_byte_in_enum_rejected() {
        let config = test_config();
        assert_metachar_error(validate(
            &req("greet", vec![("format", json!("text\x00"))]),
            &config,
        ));
    }

    #[test]
    fn metachar_safe_value_accepted() {
        // Plain alphanumeric + hyphen/dot values must not be rejected.
        let config = test_config();
        assert!(validate(&req("greet", vec![("format", json!("text"))]), &config).is_ok());
    }

    // ------------------------------------------------------------------
    // fixed_args tests
    // ------------------------------------------------------------------

    #[test]
    fn fixed_args_prepended_to_argv() {
        // test_config() sets fixed_args = ["--greet"] on the "greet" command.
        // fixed_args must appear before user-supplied args in argv.
        let config = test_config();
        let result = validate(&req("greet", vec![("format", json!("text"))]), &config).unwrap();
        let argv = &result.argv;
        assert_eq!(
            argv.first().map(String::as_str),
            Some("--greet"),
            "fixed_args must be prepended to argv"
        );
        let fixed_pos = argv.iter().position(|x| x == "--greet").unwrap();
        let flag_pos = argv.iter().position(|x| x == "--format").unwrap();
        assert!(
            fixed_pos < flag_pos,
            "fixed_args must appear before user-supplied args"
        );
    }

    // ------------------------------------------------------------------
    // Multiple-error aggregation tests
    // ------------------------------------------------------------------

    #[test]
    fn multiple_unknown_args_rejected() {
        // All unknown arg names must be collected and returned together.
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("unknown1", json!("a")), ("unknown2", json!("b"))],
            ),
            &config,
        );
        match result {
            Err(ValidationError::UnknownArgs(mut args)) => {
                args.sort();
                assert_eq!(args, vec!["unknown1", "unknown2"]);
            }
            other => panic!("expected UnknownArgs with two entries, got {:?}", other),
        }
    }

    #[test]
    fn multiple_missing_required_args() {
        // All missing required arg names must be collected and returned together.
        let config = LoadedConfig {
            server: ServerConfig {
                port: 8080,
                bind: "127.0.0.1".into(),
                token: None,
            },
            token: "test-token".into(),
            commands: vec![LoadedCommandSpec {
                name: "multi".into(),
                executable: PathBuf::from("/usr/bin/true"),
                working_dir: None,
                fixed_args: vec![],
                args: vec![
                    LoadedArgSpec {
                        name: "alpha".into(),
                        flag: "--alpha".into(),
                        required: true,
                        arg_type: LoadedArgType::Enum {
                            values: vec!["a".into()],
                        },
                    },
                    LoadedArgSpec {
                        name: "beta".into(),
                        flag: "--beta".into(),
                        required: true,
                        arg_type: LoadedArgType::Enum {
                            values: vec!["b".into()],
                        },
                    },
                ],
            }],
        };
        let result = validate(&req("multi", vec![]), &config);
        match result {
            Err(ValidationError::MissingRequiredArgs(mut args)) => {
                args.sort();
                assert_eq!(args, vec!["alpha", "beta"]);
            }
            other => panic!(
                "expected MissingRequiredArgs with two entries, got {:?}",
                other
            ),
        }
    }

    // ------------------------------------------------------------------
    // Metacharacter check on Path args
    // ------------------------------------------------------------------

    #[test]
    fn metachar_semicolon_in_path_rejected() {
        // A semicolon in a path argument must be caught by the metacharacter
        // guard before path resolution runs.
        let config = test_config();
        assert_metachar_error(validate(
            &req(
                "greet",
                vec![
                    ("format", json!("text")),
                    ("output", json!("/tmp;rm -rf /")),
                ],
            ),
            &config,
        ));
    }

    // ------------------------------------------------------------------
    // Path traversal test
    // ------------------------------------------------------------------

    #[test]
    fn path_dotdot_outside_allowed_rejected() {
        // /tmp/../usr canonicalises to /usr (outside the allowed /tmp tree).
        // The validator must detect this even though the raw string starts with
        // "/tmp/".
        let config = test_config();
        let result = validate(
            &req(
                "greet",
                vec![("format", json!("text")), ("output", json!("/tmp/../usr"))],
            ),
            &config,
        );
        assert!(
            matches!(result, Err(ValidationError::InvalidArgValue { .. })),
            "expected InvalidArgValue for dotdot traversal path, got {:?}",
            result
        );
    }
}
