# Open Source Audit

Audit date: 2026-06-11

## Scope

- Current tracked tree
- All reachable Git commits and blobs
- Commit author metadata
- macOS LaunchAgent resources

## Results

- `gitleaks 8.30.1` scanned the complete reachable history with zero secret
  findings.
- No authentication snapshot or real API key file is tracked.
- The private history contains developer identity metadata and machine-specific
  absolute paths. These are privacy findings, not credential findings.
- The public tree replaces the LaunchAgent with a runtime-substituted template
  and contains no developer-machine path.

## Publication Strategy

Publish from a new root commit containing only the audited public tree. Do not
make the existing private repository public while old branches or remote refs
remain reachable.

Before publication:

1. Verify the clean-root branch with `gitleaks git --redact`.
2. Confirm that `git log --format='%an <%ae>'` contains only the intended public
   maintainer identity.
3. Create a new public repository, or replace every public remote ref with the
   clean-root history and delete obsolete branches.
4. Run CI before creating the first release tag.
