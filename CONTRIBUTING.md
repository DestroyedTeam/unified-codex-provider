# Contributing

## Development

```bash
cargo fmt --all --check
cargo check
cargo test
```

Keep changes focused and add tests for user-visible behavior. Never commit
real Codex profiles, authentication snapshots, API keys, access tokens, or
refresh tokens.

## Pull Requests

Describe the problem, the chosen behavior, and the validation commands that
were run. Changes to authentication, switching, session migration, or macOS
service installation should include regression coverage.

## Security

Follow [SECURITY.md](SECURITY.md) for sensitive reports.
