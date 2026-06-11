# claude-rmux-runner

Standalone Rust runner that launches interactive Claude inside a fresh
rmux-owned session, sends a prompt file, waits for a generic result marker,
writes an audit trace, cleans up only the session it created, and prints one
metadata JSON object to stdout.

```sh
cargo run -- \
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

- `0`: `--result-file` exists.
- `1`: Claude pane/process exited before the marker existed.
- `2`: timeout.
- `3`: setup, configuration, or validation failure.
- `4`: interrupted or cancelled.

The prompt file is sent as literal UTF-8 terminal input with no wrapper text.
The result file is treated only as a marker file; it is not parsed as JSON.
