# janus-rce

A small, configurable HTTP server that exposes a whitelist of pre-approved shell
commands as authenticated API endpoints, streaming their output as
[Server-Sent Events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events).

The intended use case is letting a sandboxed agent (e.g. an AI coding assistant
running inside Docker) trigger specific operations on the host — such as building
and testing an Xcode project — without giving the agent a general-purpose shell.

## Security model

- **Allowlist only.** The set of commands, executables, and acceptable argument
  values is declared statically in a TOML config file.  Anything not explicitly
  listed is rejected at validation time; the server never evaluates a shell
  expression.
- **Argument validation.** Each argument is validated against a strict type before
  reaching the child process: an exact-match enum, an anchored regex pattern, a
  canonical path within an allowed directory, or a boolean flag.  Shell
  metacharacters are rejected in all string arguments as an additional
  defence-in-depth layer.
- **Fixed arguments.** Server-side positional words (`fixed_args`) are declared in
  the config and always prepended to `argv` before any user-supplied arguments.
  They are never user-controlled and cannot be overridden at request time.
- **No shell.** Commands are spawned via `Command::args()` — arguments are passed
  directly to `execve`; no shell is involved.
- **Environment isolation.** Child processes start with a cleared environment
  (`env_clear()`).  Only a small, safe set of variables (`PATH`, `LANG`, `HOME`,
  `USER`, `TMPDIR`) is forwarded, so secrets such as `JANUS_TOKEN` are never
  inherited.
- **Bearer-token authentication.** All routes except `GET /health` require an
  `Authorization: Bearer <token>` header.  Token comparison uses constant-time
  equality to resist timing attacks.

## Requirements

- Rust 1.85+ (edition 2024)
- Targets macOS and Linux

## Installation

```sh
git clone https://github.com/you/janus-rce
cd janus-rce
cargo build --release
```

The binary is written to `target/release/janus-rce` (or wherever your
`[build] target-dir` points).

## Configuration

Copy `janus.toml` and edit it for your project.  The file has three sections:

### `[server]`

```toml
[server]
port = 9876
bind = "127.0.0.1"   # loopback-only; do not expose to 0.0.0.0 without a firewall
# token = "replace-with-a-long-random-secret"
```

The auth token can also be supplied via the `JANUS_TOKEN` environment variable
(takes precedence over `server.token`).  The server refuses to start if neither
is set.

### `[[commands]]`

Each command block declares one executable the server may run:

```toml
[[commands]]
name       = "build"           # used in POST /run requests
executable = "/usr/bin/xcodebuild"
working_dir = "/path/to/MyApp" # optional; defaults to /
```

### `[[commands.args]]`

Arguments are declared in the order they will be appended to `argv`.  Four
validation types are supported:

| Type | TOML | `flag` field | Description |
|------|------|--------------|-------------|
| `enum` | `type = "enum"; values = ["a", "b"]` | Required | Value must be one of the listed strings; appended as `[flag, value]` |
| `pattern` | `type = "pattern"; pattern = "[a-z]+"` | Required | Value must match the regex (auto-anchored `^…$`); appended as `[flag, value]` |
| `path` | `type = "path"; within = ["/tmp"]` | Required | Value must be an absolute path within one of the listed directories; appended as `[flag, value]` |
| `bool` | `type = "bool"` | Required | `true` appends the flag alone; `false` omits it entirely |

```toml
[[commands]]
name       = "build"
executable = "/usr/bin/xcodebuild"
# Server-side positional words always prepended to argv (not user-controlled).
fixed_args = ["build"]

[[commands.args]]
name     = "scheme"
flag     = "-scheme"
required = true
type     = "enum"
values   = ["MyApp", "MyAppTests"]

[[commands.args]]
name     = "quiet"
flag     = "-quiet"
required = false
type     = "bool"
```

## Running the server

```sh
JANUS_TOKEN="$(openssl rand -hex 32)" ./janus-rce --config janus.toml
```

The config file path defaults to `./janus.toml` and can be overridden with the
`JANUS_CONFIG` environment variable.

## API

### `GET /health`

Liveness check; no authentication required.

```
HTTP 200
{"status":"ok"}
```

### `GET /commands`

Returns the list of configured commands and their arguments.  Requires a valid
bearer token.

```
HTTP 200
[
  {
    "name": "build",
    "args": [
      {"name": "scheme",    "required": true,  "type": "enum"},
      {"name": "quiet",     "required": false, "type": "bool"}
    ]
  }
]
```

### `POST /run`

Validates and executes a command, streaming its output as Server-Sent Events.
Requires a valid bearer token and `Content-Type: application/json`.

**Request body:**

```json
{
  "command": "build",
  "args": {
    "scheme": "MyApp",
    "quiet": true
  }
}
```

**Response** (`Content-Type: text/event-stream`):

Each event's `data` field is a JSON object.  The stream always ends with an
`exit` event.

```
data:{"type":"stdout","data":"Build succeeded."}

data:{"type":"stderr","data":"warning: deprecated API"}

data:{"type":"exit","code":0}
```

`code` is `null` when the process was killed by a signal or its exit status
could not be read.

**Error responses:**

| Status | Condition |
|--------|-----------|
| `401`  | Missing or invalid bearer token |
| `404`  | Command name not found in config |
| `422`  | Unknown args, missing required args, or invalid arg value |

All error responses use the same JSON envelope:

```json
{"error": "human-readable message"}
```

## Example: curl

```sh
TOKEN="your-token-here"

# List commands
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:9876/commands | jq .

# Run a build and stream output
curl -s \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"command":"build","args":{"scheme":"MyApp"}}' \
  http://127.0.0.1:9876/run
```

## Development

```sh
# Format
cargo fmt

# Build
cargo build

# Lint (warnings treated as errors)
cargo clippy -- -D warnings

# Tests (unit + integration + doc-tests)
cargo test

# Generate documentation
cargo doc --no-deps --open
```

CI runs all of the above automatically on every push and pull request.

## License

MIT
