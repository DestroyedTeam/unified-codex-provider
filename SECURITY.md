# Security Policy

## Reporting a Vulnerability

Please do not open a public issue for vulnerabilities that may expose Codex
credentials, OAuth tokens, API keys, or private configuration.

Use GitHub's private vulnerability reporting feature for this repository. If
that feature is unavailable, contact the maintainers through the repository
owner's private contact channel.

Include reproduction steps and affected versions, but never include real
tokens, `auth.json` contents, or private provider credentials.

## Credential Handling

UCP reads and writes authentication snapshots under `~/.codex`. These files
must remain local and must never be committed. Reports and logs should be
redacted before sharing.
