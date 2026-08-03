<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/warden-logo-white.svg">
    <img src="assets/logo/warden-logo-black.svg" width="420" alt="warden">
  </picture>
</p>

A local, read-only CLI that reads your coding agent's session logs and tells you
how you actually spend tokens, prompts, and context.

```
$ warden report projects --since 7d
PROJECT                     SESSIONS      IN     OUT  CACHE R  EST. COST
adept-impl                         7  105.5k  505.8k  176.92M   $50.83 ~
adept                             32   24.6k  680.6k  126.36M   $25.71 ~
cli-tracing-spec                   1   10.8k  108.9k   29.53M          –
make-workspace-publishable         4    8.0k  196.3k   27.86M    $3.51 ~
adept-python-packaging             1    8.6k  119.2k   22.50M    $2.65 ~
adept-create-spec                  2     260   67.0k   13.24M          –
backlog-issue-links                4    2.4k   92.4k   11.87M    $1.96 ~
warden                             1     295   57.3k   10.81M          –
backlog-upskill                    1     120   31.0k    4.06M          –
assets                             1     106   23.0k    3.60M    $0.68 ~
agents-md-rename                   1      56   13.4k    1.45M          –
logo                               1      15    2.0k   355.6k          –
                                                             ~ estimated
```

**warden is not a harness.** It never calls a model provider, never needs an API
key, and contains no network code — not as a setting, but as an absence. It
reads logs, normalizes them into a JSONL store you own, and answers questions
about them.

The `–` in that table is deliberate: it means *this source cannot tell me*, not
zero. `warden doctor` explains every one of them.

## Install

```bash
cargo install --path .     # or: cargo build --release && install target/release/warden ~/.local/bin/
warden doctor              # what can warden see?
warden ingest              # read the logs, append new events
warden report projects --since 7d
```

Nothing else is required. warden creates `~/.warden/` (mode `0700`) on first
write and reads `~/.claude/projects` strictly read-only.

## Commands

| command | what it does |
|---|---|
| `warden ingest` | Scan sources from the last cursor, normalize, append. Idempotent — event ids are content-derived, so re-running (or interrupting and re-running) changes nothing. Runs implicitly before any report unless `--no-ingest`. |
| `warden report <name>` | One of seven named reports (below). |
| `warden query --group-by <dims>` | Ad-hoc rollup over `project`, `model`, `agent`, `day`, … |
| `warden suggest` | Prompts you have sent more than once, byte-for-byte. |
| `warden suggest --draft <id>` | Print a `SKILL.md` draft for one of them, **to stdout only**. |
| `warden watch --oneline` | One status-bar line of burn rate, then exit. |
| `warden doctor` | Why is this number empty? |
| `warden purge --prompts` | Delete stored prompt text. |

### Reports

| report | answers |
|---|---|
| `summary` | totals for the period, by day |
| `projects` | tokens and est. cost per project |
| `models` | usage split by model |
| `sessions` | longest and most expensive sessions |
| `files` | *attributed* tokens per file |
| `tools` | tool call frequency and failure rate |
| `compare` | this period vs. the previous one |

`report files` is attribution, not measurement: no log records "this file cost N
tokens", so warden splits a turn's cost evenly across the files its tool calls
touched. The output is labelled `attributed` and `--json` carries
`"method": "even-split"` so you can tell.

### Global flags

`--json` · `--since <7d|24h|2026-01-01>` · `--project <name>` ·
`--data-dir <path>` · `--no-ingest` · `--no-sidechain`

Sidechain (subagent) events are **included by default**. They are real spend,
but they are counted in no per-session figure your agent shows you, so warden's
totals read higher than the number in your terminal. `--no-sidechain` excludes
them; the reports say which side of that line they are on.

### `warden suggest`

One detector, deliberately: exact-duplicate prompts, matched on `text_hash`. No
embeddings, no fuzzy matching, no false positives.

```
$ warden suggest --since 14d
1015 repeated prompts found, showing the 20 most repeated — --json has them all

ID        COUNT  LAST     PROJECTS                                                SUGGESTION                                     PROMPT
17eaf8bd     86  59m ago  adept, adept-impl, adept-python-packaging, +7 more      → draft skill: simplify-4-cleanup-agents       "`/simplify → 4 cleanup agents in parallel → apply the fixes` Yo…"
319df741     10  9d ago   adept, fradragsjagt                                     → save as a slash command                      "Now update the @docs/BACKLOG.md"
abd268ed      6  1d ago   adept, backlog-issue-links, make-workspace-publishable  → draft skill: base-directory-this-skill       "Base directory for this skill: /home/mathias/.claude/skills/pla…"
246674b1      6  1d ago   adept, adept-impl, make-workspace-publishable           → draft skill: base-directory-this-skill       "Base directory for this skill: /home/mathias/.claude/skills/cod…"
```

`warden suggest --draft 17eaf8bd` prints a `SKILL.md` to stdout. **It writes
nothing.** Redirect it yourself if you want the file — staged writing with a
diff and a confirmation is a v0.2 feature, and until it exists warden will not
put a file in your repo.

Ids come from the prompt's hash, so they are stable across runs and machines.
Groups that are only the client's own transcript furniture — slash-command
expansions, compaction notices, interrupt markers — are set aside and counted in
the notes rather than suggested at you.

### `warden watch`

```
$ warden watch --oneline
warden · 34.93M tok/hr · session 33m25s · 11.31M this session
```

Built for a tmux status line. Only `--oneline` ships in 0.1.0; the streaming
form says so rather than pretending.

## The store is the API

Everything warden knows lives in append-only JSONL under `~/.warden/`. No
database, no driver, no migration story to get wrong.

```
~/.warden/
  events/2026-08.jsonl     # normalized events, partitioned by UTC month
  prompts/2026-08.jsonl    # prompt text, separated so it can be disabled
  state/ingest.jsonl       # per-source cursors, append-only, last-wins
  config.toml
```

One event per line:

```json
{
  "v": 1,
  "id": "9f2c…",
  "ts": 1754300000000,
  "agent": "claude-code",
  "provider": "anthropic",
  "model": "claude-sonnet-4-6",
  "project": "acme-api",
  "session_id": "…",
  "turn_id": "…",
  "role": "assistant",
  "input_tok": 412,
  "output_tok": 1180,
  "cache_read_tok": 84210,
  "cache_write_tok": 0,
  "stop_reason": "end_turn",
  "cost_est": 0.0412,
  "is_sidechain": false,
  "tool_calls": [{ "tool_name": "Read", "tool_target": "src/db/migrate.rs" }]
}
```

`prompts/` holds `{"event_id", "text", "text_hash"}`; `text_hash` is always
written, so duplicate detection works even with text storage off.

This shape is a committed surface:

- Fields may be **added** freely. Readers must tolerate unknown fields.
- Fields are never renamed, retyped, or removed without bumping `v`.
- A field an adapter cannot populate is **absent**, never `0`. `duration_ms` is
  missing from Claude Code's logs, so warden omits it rather than lying.
- Cache tokens stay separate from input tokens. In an agentic loop cache reads
  dominate volume at a fraction of the price; collapsing them makes every cost
  figure wrong.
- `cost_est` is an estimate, always labelled `~`, always computed from your
  config rather than prices baked into the binary.
- Appends are line-atomic. A reader that catches a torn final line skips it.

Every command also takes `--json`, which wraps rows in a versioned envelope:

```json
{
  "warden_version": "0.1.0",
  "record_version": 1,
  "report": "projects",
  "period": { "from": "…", "to": "…" },
  "rows": [ … ],
  "notes": ["cost figures are estimates"]
}
```

Pin on `record_version`. Read `notes` — that is where warden explains numbers
you might not be able to reconcile.

### …which means you don't need warden

The whole point of JSONL is that the data outlives the tool. warden has no
report for "when in the day do I burn money", but the store answers it anyway:

```bash
jq -rs '
  map(select(.cost_est > 0))
  | group_by(.ts / 1000 | strftime("%H"))
  | map({hour: (.[0].ts / 1000 | strftime("%H")), cost: (map(.cost_est) | add)})
  | sort_by(.cost) | reverse | .[:5][]
  | "\(.hour):00  $\(.cost * 100 | round / 100)"
' ~/.warden/events/*.jsonl
```

```
11:00  $43.62
09:00  $41.99
15:00  $30.48
08:00  $28.83
20:00  $28.16
```

Timestamps are epoch **milliseconds**; `strftime` wants seconds, hence the
`/ 1000`. Everything in the store is UTC.

## Config

`~/.warden/config.toml`, entirely optional:

```toml
[general]
data_dir = "~/.warden"
index_prompt_text = true

[sources.claude-code]
enabled = true
path = "~/.claude/projects"

# Per million tokens. User-editable because prices drift and models get
# deprecated — warden ships no prices of its own, and a model with no
# configured rate shows `–` rather than a fake $0.00.
[pricing.anthropic]
"claude-sonnet-4-6" = { input = 3.0, output = 15.0, cache_read = 0.3 }
"claude-opus-4-1"   = { input = 15.0, output = 75.0, cache_read = 1.5, cache_write = 18.75 }
```

`warden doctor` names every model it saw with no configured price.

## Privacy

warden reads the most sensitive data on the machine: proprietary source, business
context, occasionally a credential someone pasted into a chat.

- **Local only.** No telemetry, no network calls, no API key. Not a setting —
  there is no network code in the crate and no dependency capable of a request.
- **Read-only at the source.** `~/.claude/projects` is never written to.
- **Prompt text is separable.** `index_prompt_text = false` stores only
  `text_hash`, so duplicate detection still works and no prompt text is ever
  written to disk. `warden suggest` then reports the repeats and says it cannot
  show you the wording, rather than showing you a blank.
- **`warden purge --prompts`** deletes `prompts/` outright. The store is
  otherwise append-only, so purge is one of the very few commands that removes
  anything: it requires `--yes` or an interactive confirmation, it touches
  nothing but `prompts/`, and it tells you exactly what it took.

  ```
  $ warden purge --prompts --yes --data-dir /tmp/warden-copy
  REMOVED                   FILES  PROMPT RECORDS  BYTES
  /tmp/warden-copy/prompts      2  5,638           14.3 MB
  ```

- `~/.warden/` is created `0700`. Plain-text JSONL on disk is more exposed than
  an opaque database file — it isn't encryption either way, but it is more
  obviously readable, so the permissions matter.

## Status

0.1.0 — MVP. One adapter (Claude Code). Codex and Cursor adapters, skill
*writing*, an MCP server, and prompt clustering are deferred; see `docs/MVP.md`
§10 for the order and the reasoning.

## Licence

MIT. See `LICENSE`.
