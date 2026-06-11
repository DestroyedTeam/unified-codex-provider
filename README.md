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
- Reconcile session metadata so historical sessions remain visible.
- Detect duplicate ChatGPT auth snapshots that may cause
  `refresh_token_reused` failures.
- Generate dynamic shell completion for Bash, Zsh, and Fish.
- Optionally run auto-sync through a per-user macOS LaunchAgent.

## Requirements

- Rust 1.75 or newer for source installation.
- Codex CLI available in `PATH` for `ucp login`.
- macOS for the LaunchAgent helper scripts. Core CLI behavior is also tested on
  Linux.

## Install From Source

```bash
git clone https://github.com/DestroyedTeam/unified-codex-provider.git
cd unified-codex-provider
cargo install --path .
ucp --help
```

Signed macOS packages and Homebrew installation are planned for a future
release.

## Quick Start

Migrate existing Codex configuration:

```bash
ucp init
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

The checked-in plist is a template and contains no user-specific paths. Install
the per-user LaunchAgent after `ucp` is available in `PATH`:

```bash
./scripts/install-launch-agent.sh
```

Remove it without touching Codex profiles or authentication data:

```bash
./scripts/uninstall-launch-agent.sh
```

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
