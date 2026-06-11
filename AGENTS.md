# Repository Agent Guide

Read this file before modifying the repository.

## Mission

Unified Codex Provider (`ucp`) manages Codex account and provider profiles. It
must preserve authentication refreshes, shared configuration, and historical
session visibility while switching profiles.

## Engineering Rules

- Never print, commit, or include real tokens, API keys, or `auth.json` data in
  tests and documentation.
- Treat a profile as an account/provider identity and `model_provider` as the
  Codex runtime provider key.
- Native OpenAI login must use the official `codex login` flow in an isolated
  temporary `CODEX_HOME`; do not implement private OAuth endpoints.
- Preserve session file modification times when rewriting metadata.
- Any non-built-in `model_provider` must receive a matching
  `[model_providers.<name>]` configuration section.
- UCP operations that touch Codex state must remain serialized by the global
  lock.
- macOS service resources must be generated from templates. Never commit an
  absolute home directory or developer-machine path.

## Validation

Run before claiming completion:

```bash
cargo fmt --all --check
cargo check --locked
cargo test --locked
```

For release or open-source preparation, also run a full-history secret scan:

```bash
gitleaks git --redact .
```

## Important Local Data

UCP operates on files under `~/.codex`, including credential snapshots and the
Codex sessions database. Tests must use an isolated temporary `HOME` whenever
they create, delete, or inspect these files.
