#!/usr/bin/env bash
#
# pre_commit_hook.sh — run the same checks as the CI pipeline before each commit.
#
# Symlink into place from within the .git/hooks/ directory with:
#   ln -s ../../scripts/pre_commit_hook.sh pre-commit

set -euo pipefail

# ── Colour helpers ────────────────────────────────────────────────────────────
BLUE='\033[1;34m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
RESET='\033[0m'

step() { printf "\n${BLUE}==> %s${RESET}\n" "$*"; }
pass() { printf "${GREEN}    ok${RESET}\n"; }
warn() { printf "${YELLOW}    warn: %s${RESET}\n" "$*"; }
fail() { printf "${RED}    FAILED${RESET}\n" >&2; exit 1; }

# ── Environment ───────────────────────────────────────────────────────────────
# Source cargo's env file so the script works when invoked by Git outside of an
# interactive shell (e.g. from a GUI client that does not load ~/.bashrc).
# shellcheck source=/dev/null
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

# Always run from the repository root regardless of where Git invokes us.
cd "$(git rev-parse --show-toplevel)"

# ── Checks ────────────────────────────────────────────────────────────────────
step "fmt"
cargo fmt --check || fail

step "clippy"
cargo clippy --all-targets -- -D warnings || fail

step "test"
cargo test || fail

step "doc"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps || fail

if command -v cargo-audit >/dev/null 2>&1; then
    step "audit"
    cargo audit || fail
else
    warn "cargo-audit not installed — skipping security audit"
    warn "install with: cargo install cargo-audit --locked"
fi

# ── Done ──────────────────────────────────────────────────────────────────────
printf "\n${GREEN}All checks passed.${RESET}\n"
