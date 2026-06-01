# warden

A terminal UI for managing and monitoring AI/LLM agents, built in Rust with [Ratatui](https://ratatui.rs).

## Build & run

```bash
cargo run --release
```

State is persisted to `~/.warden/agents.json`. Press `q` to save and quit.

Select an agent and press `r` to run it (`x` to stop); output streams into the
log pane live.

## Backends

Execution is pluggable via the `Backend` trait in `src/runner.rs`:

- **Anthropic** — used automatically when `ANTHROPIC_API_KEY` is set. Streams
  from the Messages API; the agent's `model` field selects the model (falling
  back to `claude-sonnet-4-6` if blank). Set a real Anthropic model id — e.g.
  `claude-opus-4-8`, `claude-sonnet-4-6` — not a non-Anthropic name.
- **Mock** — the default fallback when no key is set. Streams the task text
  back as fake tokens, so the UI works fully offline.

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cargo run --release
```

The status bar shows which runner is active at startup.
