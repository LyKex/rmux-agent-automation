# CLAUDE.md

Guidance for agents working in `claude-rmux-runner` — a standalone Rust runner
that launches interactive Claude inside a fresh rmux-owned session, sends a
prompt file, waits for a result marker, writes an audit trace, and prints one
metadata JSON object to stdout. See `README.md` for CLI usage and exit codes.

## Agent skills

### Issue tracker

Issues and PRDs live as GitHub issues in `LyKex/rmux-agent-automation` (via the
`gh` CLI); external PRs are not a triage surface. See
`docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`,
`ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See
`docs/agents/domain.md`.
