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
  --trace-file /abs/path/to/run/agent_trace.log \
  --timeout-seconds 3600
```

Useful options:

```sh
--model <model>
--session-name <name>
--claude-bin <path-or-command>
--rmux-bin <path-or-command>
--final-message-file /abs/path/to/run/final_message.txt
--permission-mode acceptEdits|bypassPermissions|default
```

Exit codes:

- `0`: `--result-file` exists and its trimmed contents name a path that exists on disk.
- `1`: Claude pane/process exited before a valid result marker appeared.
- `2`: timeout.
- `3`: setup, configuration, or validation failure.
- `4`: interrupted or cancelled.

The prompt file is sent as literal UTF-8 terminal input with no wrapper text.
The result file is a plain-text marker: its trimmed contents are read as the
run's final output directory. A run is complete only once that file holds a
non-empty path that exists on disk. The resolved path is echoed back as
`final_output_dir` in the metadata JSON and the trace. The file is never parsed
as JSON.

## Trace capture (best effort)

The `--trace-file` records run metadata (session, Claude command, exit reason,
resolved `final_output_dir`, timestamps) plus a `terminal_snapshot` block. That
block is a **single final frame** of the pane's visible text, not the full
conversation. It is best effort and has known limits:

- The prompt may appear scrolled off or collapsed by Claude's TUI to a
  `[Pasted text #N +M lines]` placeholder — so the snapshot is not a reliable
  record of the exact prompt. Use your own `--prompt-file` as the source of
  truth for what was sent.
- It captures only the last visible screen, not scrollback.

A structured, complete transcript is not available in this mode: Claude's native
JSON output is a headless (`claude -p --output-format json`) feature, and this
runner drives the interactive TUI on purpose. Interactive `--safe-mode` also
writes no session `.jsonl` under `~/.claude/projects/`. For a taller final
snapshot, run in a larger terminal.
