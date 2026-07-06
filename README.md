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
