# claude-rmux-runner

Standalone Rust runner that launches interactive Claude inside a fresh
rmux-owned session, sends a prompt file, waits for a generic result marker,
writes an audit trace, cleans up only the session it created, and prints one
metadata JSON object to stdout.

## Install

```sh
cargo install --path .
```

This builds a release binary and places it at `~/.cargo/bin/claude-rmux-runner`
(make sure `~/.cargo/bin` is on your `PATH`). Re-run the command after code
changes to update the installed binary.

## Usage

```sh
claude-rmux-runner \
  --workspace /abs/path/to/workspace \
  --prompt-file /abs/path/to/prompt.txt \
  --result-file /abs/path/to/workspace/agent_result.json \
  --trace-file /abs/path/to/run/meta.json \
  --timeout-seconds 3600
```

Useful options:

```sh
--model <model>
--session-name <name>
--claude-bin <path-or-command>
--rmux-bin <path-or-command>
--transcript-file /abs/path/to/run/trace.jsonl
--final-message-file /abs/path/to/run/final_message.txt
--permission-mode acceptEdits|bypassPermissions|default
```

Exit codes:

- `0`: `--result-file` exists with non-whitespace content (the agent finished and wrote it).
- `1`: Claude pane/process exited before the result file appeared.
- `2`: timeout.
- `3`: setup, configuration, or validation failure.
- `4`: interrupted or cancelled.

The prompt file is sent as literal UTF-8 terminal input with no wrapper text.
The result file is the completion marker the agent writes when it is done: the
run is complete once the file exists with non-whitespace content. Its contents
are the caller's contract and opaque to the runner — a bare output-dir path,
JSON, or anything else all count. As a best effort, when the contents are a bare
path that exists on disk that path is echoed back as `final_output_dir` in the
metadata JSON and the trace; a structured (e.g. JSON) body simply leaves
`final_output_dir` null.

## Trace capture

Each run emits **two artifacts**:

1. **Metadata** (`--trace-file`) — one JSON object with the full run record:
   session, `claude_session_id`, `claude_command`, `permission_mode`,
   `exit_reason`/`exit_code`, resolved `final_output_dir`, timestamps, and the
   `transcript_*` fields below. This is the **same object printed to stdout** —
   the file just persists it, so you no longer need to capture stdout separately.
2. **Transcript** (`--transcript-file`, default `trace.jsonl` beside the
   metadata file) — the **real Claude session transcript**, a valid `.jsonl` of
   the full turn-by-turn conversation (user prompt, assistant thinking, every
   `tool_use`/`tool_result`) copied verbatim from what Claude writes under its
   projects config dir.

The metadata's `transcript_source` says which was captured: `session_jsonl` (the
real transcript), `terminal_snapshot` (fallback), or `none`. `transcript_file`
is the copy's path; `transcript_jsonl_path` is the on-disk source Claude wrote.

How the real transcript is captured despite driving the interactive TUI (not
headless `-p --output-format json`):

- The runner forces `--session-id <uuid>`, so it knows the exact
  `<uuid>.jsonl` to harvest after the run — no path munging needed.
- Interactive Claude flushes that transcript incrementally, but **suppresses it
  for nested/child sessions**: when the runner itself runs under a Claude
  session, the spawned Claude inherits `CLAUDE_CODE_CHILD_SESSION` /
  `CLAUDECODE` and writes no transcript. The runner therefore spawns Claude
  through `env -u …`, clearing those vars so a normal session is persisted.
- Before teardown the runner quits Claude cleanly (Ctrl-C) so the final turn is
  flushed; this appends a trailing `[Request interrupted by user]` record.

**Fallback.** If the transcript file cannot be located or read
(`transcript_source` = `terminal_snapshot`), `--transcript-file` instead holds a
single JSON line `{"type":"terminal_snapshot","text":…}` wrapping the pane's
final visible frame (last screen only, no scrollback; the prompt may be
collapsed to a `[Pasted text #N +M lines]` placeholder). Use your own
`--prompt-file` as the source of truth for what was sent.

If the operator sets `CLAUDE_CONFIG_DIR`, the runner honors it when locating the
transcript.
