# Claude rmux Wrapper Requirements

Purpose: provide a benchmark runner backend that drives an interactive Claude
session through `rmux` instead of `claude -p`, while preserving the same
benchmark contract and as much isolation and reproducibility as possible.

## Binary Interface

The wrapper should be a small executable, for example `claude-rmux-runner`.

Required CLI:

```bash
claude-rmux-runner \
  --workspace /abs/path/to/fresh/workspace \
  --prompt-file /abs/path/to/prompt.txt \
  --result-file /abs/path/to/workspace/agent_result.json \
  --trace-file /abs/path/to/run/agent_trace.log \
  --timeout-seconds 3600
```

Optional CLI:

```bash
--model <model>
--session-name <name>
--claude-bin <path-or-command>
--rmux-bin <path-or-command>
--final-message-file /abs/path/to/run/final_message.txt
```

Exit codes:

- `0`: Claude finished and the result file exists and names an output path that exists.
- `1`: Claude exited or pane ended without a valid result.
- `2`: timeout.
- `3`: wrapper setup failure.
- `4`: interrupted or cancelled.

## Benchmark Contract

The wrapper must send the benchmark prompt exactly as provided in
`--prompt-file`.

The agent must be instructed by the prompt to write a plain-text result file at
`--result-file` whose trimmed contents are the final output directory path:

```text
/absolute/path/to/results
```

After completion, the wrapper should verify that:

- `--result-file` exists.
- Its trimmed contents are a non-empty path.
- That path exists on disk.

The resolved path is surfaced as `final_output_dir` in the metadata and trace.
The result file is not parsed as JSON.

## Isolation Requirements

Each run must start from the fresh workspace passed by `--workspace`.

The Claude process must start with cwd set to `--workspace`.

The wrapper must not expose the QEClaw repo, prior benchmark results, skills,
plugins, MCP configs, or previous task workspaces unless the benchmark
explicitly copied them into the workspace.

Best-effort isolation:

- Start a new rmux session or pane per run.
- Do not resume old Claude sessions.
- Do not attach to existing conversation state.
- Avoid loading project-local instructions from outside `--workspace`.
- Avoid using prior pane scrollback as context.
- Clear or isolate any wrapper-managed temporary files per run.

If Claude interactive mode cannot fully disable user memory, plugins, or
configuration, the wrapper must report that limitation in metadata.

## Runtime Behavior

The wrapper should:

1. Create or enter a fresh rmux session or pane.
2. Launch interactive Claude in `--workspace`.
3. Send the prompt from `--prompt-file`.
4. Wait until Claude finishes or until `--timeout-seconds`.
5. Capture pane transcript continuously or at the end into `--trace-file`.
6. Validate `--result-file`.
7. Kill and clean up the pane or session on timeout or failure.

Completion detection can be one of:

- Claude process exits.
- A wrapper sentinel appears, such as a valid `agent_result.json`.
- A configured idle or finished marker, if reliable.

Prefer sentinel detection plus process and pane cleanup.

## Trace And Metadata

The wrapper must write a trace file with enough information to audit the run:

- prompt send time
- Claude command used
- rmux session or pane id
- transcript or captured terminal output
- timeout or failure reason if any

The captured terminal output is best effort: a single final visible frame of the
pane, not the full conversation, and Claude's TUI may collapse a pasted prompt to
a `[Pasted text #N +M lines]` placeholder in that frame. A structured transcript
is not available — native JSON is a headless-mode feature, and interactive
`--safe-mode` writes no session `.jsonl`. Treat `--prompt-file` as the source of
truth for what was sent.

It should also emit JSON metadata to stdout or a file if requested:

```json
{
  "entrypoint": "claude_interactive_rmux",
  "workspace": "/abs/path",
  "session": "...",
  "pane": "...",
  "result_file": "/abs/path/agent_result.json",
  "final_output_dir": "/abs/path/results",
  "isolation_notes": [],
  "exit_reason": "completed"
}
```

## Safety Requirements

On timeout, the wrapper must terminate only the Claude process or pane it
created.

It must not kill unrelated rmux sessions.

It must generate unique session or pane names unless explicitly provided.

It must handle prompts containing quotes, shell metacharacters, newlines, and
long text without shell interpolation bugs. Prefer sending file contents
directly rather than embedding the prompt in a shell command.

## Benchmark Integration

In `agent-benchmark`, this should become a separate runner:

```text
claude_interactive
```

It should not replace the existing headless runner:

```text
claude
```

The runner still uses the same `agent_result.json` contract, same QE judge,
same scoring, and same validity checks.

Billing and authentication mode should be recorded only as metadata, not as a
runner identity or scoring dimension.

