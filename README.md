# Unified Codex Provider (`ucp`)

`ucp` is a command-line profile manager for switching Codex accounts and model
providers without losing refreshed authentication state or historical session
visibility.

> Status: usable, but still pre-1.0. Back up `~/.codex` before the first
> migration and review the output of `ucp status` after switching.

## Features

- Keep provider-specific profiles separate from shared Codex configuration.
- Preserve full ChatGPT authentication snapshots, including refreshed tokens.
- Switch between native OpenAI accounts, API-key providers, and proxies.
- Reconcile Codex session metadata and database indexes so historical sessions
  remain visible across provider switches without touching tool call/output
  rows.
- Build a local read-only history audit index from raw rollout JSONL on every
  switch/sync, so command executions and tool calls remain inspectable even if
  Codex Desktop history replay omits them.
- Detect duplicate ChatGPT auth snapshots that may cause
  `refresh_token_reused` failures.
- Generate dynamic shell completion for Bash, Zsh, and Fish.
- Optionally run auto-sync through a per-user macOS LaunchAgent.

## Requirements

- Codex CLI available in `PATH` for `ucp login`.
- macOS for auto-sync LaunchAgent support. Core CLI behavior is also tested on
  Linux.

## Install

```bash
brew install DestroyedTeam/tap/ucp
ucp setup
```

`ucp setup` creates the local UCP state directory, migrates legacy Codex
provider files when present, installs the macOS auto-sync service, and runs a
diagnostic check.

To install without the macOS auto-sync service:

```bash
ucp setup --no-service
```

Source installation is also supported for development:

```bash
cargo install --path .
```

## Quick Start

Migrate existing Codex configuration:

```bash
ucp setup
ucp list
ucp status
```

Register an OpenAI/ChatGPT account through the official Codex login flow:

```bash
ucp login --name openai_work --switch
```

Add an API-compatible provider:

```bash
ucp add my-provider \
  --model-provider my_api \
  --model gpt-5.5 \
  --base-url https://example.com/v1 \
  --wire-api responses \
  --api-key YOUR_API_KEY
```

Switch, inspect, and remove profiles:

```bash
ucp switch openai_work
ucp status
ucp remove old-profile
```

`ucp switch` and `ucp sync` update only `session_meta` and `turn_context` rows
inside historical `rollout-*.jsonl` files by default. Tool calls, command
outputs, assistant messages, and other event rows are left byte-for-byte
unchanged. If you are intentionally repairing an old corrupted rollout and want
the legacy full-file rewrite behavior, pass `--rewrite-rollouts`; UCP will back
up matching rollout files and SQLite state files first.

Every switch/sync also refreshes `~/.codex/.ucp_history/` from the raw rollout
files under both `sessions/` and `archived_sessions/`. The generated
`tool_calls.jsonl`, `command_executions.jsonl`, and `summary.json` files are a
read-only audit index: they list recovered tool calls, command/cwd/exit status,
rollout path, and source line references without rewriting the original
rollout history. Command output is stored only as a bounded preview in the
index and is not printed by default during switch/sync.

## Shell Completion

Zsh example:

```bash
mkdir -p ~/.zsh/completions
ucp completions zsh > ~/.zsh/completions/_ucp
```

Add the directory to `fpath` in `~/.zshrc` if it is not already present:

```zsh
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

Profile completion is dynamic, so newly registered profiles are available
without regenerating the completion file.

## macOS Auto-Sync

Install, inspect, or remove the per-user LaunchAgent:

```bash
ucp service install
ucp service status
ucp service uninstall
```

The LaunchAgent watches `~/.codex/auth.json` and `~/.codex/config.toml`, then
runs `ucp sync --auto`. The generated plist contains user-specific paths, so it
is created locally and is not checked into the repository.

## Diagnostics

```bash
ucp doctor
```

`doctor` reports the current binary path, Codex CLI availability, config/auth
files, profiles, active state, LaunchAgent status, and duplicate ChatGPT auth
snapshot warnings.

## Homebrew Tap

```bash
brew install DestroyedTeam/tap/ucp
```

The tap formula downloads prebuilt release artifacts for Apple Silicon and
Intel macOS.

## Data Layout

```text
~/.codex/
├── auth.json
├── config.toml
├── common.toml
├── providers/
│   ├── profile.toml
│   └── profile.auth.json
└── .ucp_state.json
```

Authentication snapshots contain credentials. Never commit or share them.

## Development

```bash
cargo fmt --all --check
cargo check --locked
cargo test --locked
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
