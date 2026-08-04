# warden Architecture Documentation

> Generated: 2026-08-04 · Commit: f621965 · Version: 0.1.0 (crate `warden-cli`, binary `warden`)
> Re-read this file at the start of any session touching this codebase. Update it when the architecture changes (new adapter, new report, changed record shape, restructured layer).

## 1. How to Read This Document

For any AI agent (or new contributor) making changes to warden. It is self-contained and it is the architecture source of truth: everything below is stated here rather than by reference. An earlier `docs/MVP.md` spec drove the initial build and has since been removed; its load-bearing decisions live in this file, and the rationale for individual choices lives in the module-level doc comments. Sections 1–7 are universal; 8–11 cover the CLI, store, and honesty rules that are specific to warden.

## 2. Overview

warden is a **local, read-only CLI** that reads a coding agent harness's own session logs (Claude Code today), normalizes them into an append-only JSONL store you own (`~/.warden/`), and reports over that store — where tokens go, which files are expensive, which tools fail. It then reads the store the other way round: repeated work becomes a suggested skill or slash command.

Two hard properties, both structural rather than configurable:

- **No network.** warden never calls a model provider, needs no API key, and has no dependency capable of making a request. Adding one would break the crate's central claim.
- **Read-only against sources.** Adapters open harness log files for reading and never write, rename, or truncate anything under a source tree. The only thing warden writes is its own store.

Flow: `sources → adapters → ingest → store (JSONL) → scanner → reports/suggest → output (table | JSON envelope)`.

## 3. Technology Stack

- **Rust 2021**, `rust-version = "1.87"`, tracks stable in CI.
- Runtime deps, all thin and deliberate: `clap` 4 (derive) for the CLI, `serde` + `serde_json` for records and the JSON envelope, `chrono` for all time handling, `toml` 0.8 for config, `sha2` 0.10 for content-derived ids. `tempfile` 3 is a dev-dependency.
- **No async runtime, no database, no HTTP client.** Do not add one without an explicit decision — the absence is the design.
- Distributed twice from one source of truth: **crates.io** as `warden-cli`, and **PyPI** as `warden-cli` via `maturin` with `bindings = "bin"` (the wheel ships the compiled binary plus a tiny Python launcher). `Cargo.toml` owns version/description/license/keywords; `pyproject.toml` declares them `dynamic` so the two can never disagree.

## 4. Project Structure

```
src/
  main.rs             thin dispatcher: parse Cli, build Context, call a command
  lib.rs              module list only (crate name `warden`)
  cli.rs              clap types (Cli, Command) + TimeWindow / --since parsing
  config.rs           config.toml model, pricing table, cost estimation
  ingest.rs           resumable, idempotent ingest engine
  suggest.rs          exact-duplicate prompt detection + SKILL.md drafting
  doctor.rs           "why is this number empty?" health model
  adapters/           mod.rs: Adapter trait, Kpi, Capabilities, registry
                      claude_code.rs: ~/.claude/projects/**/*.jsonl parser
  store/              mod.rs re-exports the whole store surface
                      paths.rs   data-dir resolution, monthly partitions
                      record.rs  Event / PromptRecord / IngestCursor
                      id.rs      content-derived event ids, text hashes
                      scanner.rs the single read path over events/
                      writer.rs  append-only writer, 0700 dirs
  commands/           one module per subcommand + shared Env
  reports/            mod.rs: dispatch, Totals/Cost/Notes, testkit
                      summary, projects, models, sessions, tools, compare,
                      files, query
  output/             mod.rs: Report + emit()  (the only --json branch)
                      table.rs   flat table renderer, Cell enum, ANSI
                      envelope.rs versioned JSON envelope
python/warden/        wheel-only launcher: __main__.py execs the binary,
                      _find_warden.py locates it via the dist's RECORD
python/tests/         pytest suite for the discovery shim
assets/             † logo/favicon images for repo branding
.claude/            † local Claude Code settings
specs/              † per-change specs
ARCHI.md            † this file
AGENTS.md           † procedure companion to this file
```

† excluded from both the crate and the wheel (`Cargo.toml`'s `exclude`, `[tool.maturin]`'s `exclude`).

Organizing principle: **one direction of dependency.** `cli → commands → {reports, suggest, doctor, ingest} → store → adapters/config`. Nothing lower reaches back up; nothing outside `store::scanner` opens an event file.

## 5. Core Architecture Principles

1. **Absence is not zero.** Any figure warden cannot derive is `None` in a record, absent in JSON, and `Cell::Unsupported` (a dim `–`) in a table. Never substitute `0` or `0.0` for "unknown". This is the single most load-bearing rule in the codebase.
2. **The store is the API.** Append-only JSONL, no database, no migrations. Users are expected to `jq` it directly, so the record shape is a committed surface (§9).
3. **One read path.** Every report consumes `store::Scanner`; no report opens a partition file itself. That is what would let a derived cache be added later without touching each report.
4. **One presentation layer.** Every report builds an `output::Report` and goes out via `output::emit`. No command branches on `--json` itself, so the table and JSON surfaces cannot drift.
5. **Cost is derived at read time**, from `config.toml`, never baked in at ingest. Editing a price re-prices events already in the store with no re-ingest.
6. **Idempotence by construction.** Event ids are a truncated SHA-256 over identifying content, so re-ingest produces lines already present and drops them. Re-running ingest yields a byte-identical store.

## 6. Build System & Toolchain

Commands, verbatim from `.github/workflows/ci.yml` (`RUSTFLAGS: -D warnings` is set for the whole job):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Python-side (second CI job, ubuntu only): `uv venv .venv && uv pip install --python .venv/bin/python .` then `uv pip install --python .venv/bin/python pytest` then `.venv/bin/python -m pytest python/tests -q`. That job also asserts the wheel's version, `warden --version`, and `python -m warden --version` all match the crate version — maturin resolving the version from `Cargo.toml` is a load-bearing invariant, not incidental.

**Testing conventions:** every test is an inline `#[cfg(test)] mod tests` in the file it covers (no `tests/` directory for Rust integration tests). Fixtures come from two crate-internal helpers, `reports::testkit` and `suggest::testkit`, which build `Event`s and temp stores — use them rather than hand-rolling a store. Tests must render with `Style::plain()` so no ANSI leaks into assertions.

**Release pipeline** (`.github/workflows/`): `release-plz.yml` on push to `main` opens/updates a release PR and, once merged, publishes to crates.io and pushes a `v*` tag; that tag triggers `release-pypi.yml`, which builds the wheel matrix (manylinux/musllinux × x86_64/aarch64, macOS, Windows) plus an sdist and publishes via PyPI Trusted Publishing. The workflows carry long comments explaining why specific choices are load-bearing (the `RELEASE_PLZ_TOKEN`, the absent `dry_run` input, separate caches) — read them before editing; they are warnings, not commentary.

## 7. Configuration

`~/.warden/config.toml`, entirely optional — a missing file means "all defaults". It lives *inside* the store, so `main::context` resolves the root first, loads the config, then re-resolves the root because `general.data_dir` may redirect it.

```toml
[general]
data_dir = "~/.warden"        # None → ~/.warden; `~` is expanded
index_prompt_text = true      # false → store text_hash only, no prompt text

[sources.claude-code]
enabled = true
path = "~/.claude/projects"   # None → the adapter's own default

[pricing.anthropic]           # per million tokens
"claude-sonnet-4-6" = { input = 3.0, output = 15.0, cache_read = 0.3 }
```

**warden ships no prices.** An unpriced model yields `None`, never `0.0`; `cache_write` defaults to the `input` rate when unset. `warden doctor` names every model it saw without a configured price.

Precedence for the store root: `--data-dir` > `general.data_dir` > `~/.warden` (`HOME`, falling back to `USERPROFILE`). No environment variables configure warden.

## 8. Command Structure

Global flags apply to every subcommand: `--json`, `--since <7d|24h|90m|2w|2026-01-01>`, `--project <name>`, `--data-dir <path>`, `--no-ingest`, `--no-sidechain`.

| Command | Notes |
|---|---|
| `ingest` | Scan sources, append new events. Runs implicitly before every report unless `--no-ingest`; its progress goes to **stderr** so stdout stays one parseable document. |
| `report <name>` | One of the fixed set named in `reports::NAMES`: `summary`, `projects`, `models`, `sessions`, `tools`, `compare`, `files`. `compare` is the one report needing a bounded window. |
| `query --group-by <dims>` | Rollup over `project`, `model`, `agent`, `provider`, `day`, `session`, `role`. |
| `watch --oneline` | Single status-bar line (tmux). Only `--oneline` ships in 0.1.0; the streaming form says so rather than faking it. |
| `suggest [--draft <id>]` | Repeated prompts; `--draft` prints a `SKILL.md` to **stdout and writes nothing**. |
| `doctor` | What warden can see, and why a column is blank. |
| `purge --prompts [--yes\|--force]` | The only command that rewrites files; `--yes` is required when stdin is not a TTY. |

**Conventions to follow when adding a command:** add the variant to `cli::Command` *and* to `Command::name()`; put the implementation in `src/commands/<name>.rs`; take `&Env<'_>` (the resolved global flags, borrowed); call `env.pre_ingest()` if it reads the store; build a `Report` and finish with `output::emit`. Adding a report means adding a module under `src/reports/`, registering it in `reports::run`, and extending `reports::NAMES` — the fixed report set is intentional, and arbitrary querying belongs in `query`.

**Exit codes and streams:** `main` returns `ExitCode::SUCCESS` or `ExitCode::FAILURE` only — there are no distinguished error codes. Errors print as `warden: {err}` on stderr. stdout carries the report and nothing else; ANSI escapes are emitted only for a table on a TTY (`Style::auto()`).

## 9. The Store

```
~/.warden/                     # created 0700
  events/YYYY-MM.jsonl         # normalized events, partitioned by UTC month
  prompts/YYYY-MM.jsonl        # prompt text, separable from events
  state/ingest.jsonl           # per-source cursors, append-only, last-wins
  config.toml
```

One `Event` per line (`store::record::Event`, `RECORD_VERSION = 1`): `v`, `id`, `ts` (epoch **milliseconds**, UTC, decides the partition), `agent`, `provider`, `role`, plus optional `model`, `project`, `session_id`, `turn_id`, `input_tok`, `output_tok`, `cache_read_tok`, `cache_write_tok`, `duration_ms`, `stop_reason`, `cost_est`, `is_sidechain`, and nested `tool_calls: [{tool_name, tool_target}]`. `prompts/` holds `{event_id, text?, text_hash}` — `text_hash` is always written, so duplicate detection still works with text storage off.

Committed compatibility contract (applies to both the record and the `--json` envelope):

- Fields may be **added** freely; readers must tolerate unknown fields (`Event` deserialization does).
- Fields are never renamed, retyped, or removed without bumping `RECORD_VERSION`.
- A field an adapter cannot populate is **absent**, never `0`. An explicit `0` means "measured zero".
- Cache tokens stay separate from input tokens — cache reads dominate agentic volume at a fraction of the price, so collapsing them makes every cost figure wrong.
- Appends are line-atomic (one `write` per whole line); a reader that catches a torn final line skips it rather than failing.

`--json` wraps rows in `{warden_version, record_version, report, period, rows, notes}`. Consumers pin on `record_version`. `period` bounds are ISO-8601 UTC with millisecond precision via the single `envelope::iso8601` renderer, and an unbounded end is `null`, never a fabricated date.

**Ingest** (`src/ingest.rs`) keeps a cursor per source file (path, mtime, byte offset); an unchanged file is not opened at all, a cursor is written only after a file is read to its last complete line, and a partial trailing line is reported as skipped and picked up next run. A run with `--project` deliberately writes **no** cursors, because a cursor must only ever mean "this file is fully ingested".

## 10. Adapters

`adapters::Adapter` turns one source log line into an `Event` and *declares* its `Capabilities` — the set of `Kpi`s it can populate (`Tokens`, `CacheTokens`, `Cost`, `Prompts`, `ToolCalls`, `StopReason`, `DurationMs`, `Sidechain`). Reports grey out a column the adapter does not declare instead of printing a misleading `0`, and `doctor` explains which of the three causes applies (not implemented / not supported by the source / no configured price).

`claude_code.rs` is the only real adapter: it reads `~/.claude/projects/**/*.jsonl` through `serde_json::Value`, ingests only `assistant` and `user` records, skips unknown record types without counting them as unparseable, and does **not** declare `DurationMs` because Claude Code logs no per-turn wall clock. Codex and Cursor are registered as `NotImplementedAdapter` stubs so `doctor` can say so out loud. The transcript schema is undocumented and drifts, so keep this parser tolerant: skip and count, never fail the run.

**Conventions to follow when adding an adapter:** implement the `Adapter` trait (`src/adapters/mod.rs`) for the new source. Declare exactly the `Kpi`s that source can actually populate as its `Capabilities`: under-declaring greys out a column it could have filled, over-declaring prints a number the source never really gave you. Then register it in `adapters::registry()` — every report and `doctor` depend on that call to see the adapter at all, and it is easy to miss because everything else about the adapter works without it.

## 11. Reports, Honesty Rules, and Suggest

`reports::mod` enforces three rules centrally rather than per report: an underivable figure is `Cell::Unsupported`; an absent token count contributes nothing (usage is logged once per request, not repeated on siblings) and `Notes` reports how many records carried usage; and anything that would make a number irreconcilable — sidechain events, unpriced models, skipped lines — becomes a note. Estimated money prints with a trailing `~` and a legend; a row mixing priced and unpriced models is marked `~+` (partial), not totalled as if complete. `SYNTHETIC_MODEL` (`<synthetic>`) counts toward volume but is excluded from cost. Sidechain (subagent) events are **included by default** — they are real spend.

`suggest.rs` has exactly one detector: group prompts by `text_hash`, report groups of `MIN_OCCURRENCES` (2) or more. No embeddings, no fuzzy matching, therefore no false positives. Short prompts (≤60 chars) are suggested as slash-command aliases, longer ones as skills. Client transcript furniture (slash-command expansions, compaction notices, interrupt markers) is set aside and counted in the notes, never silently dropped. Ids are the prompt hash truncated to 8 chars, so they are stable across runs and machines.

## 12. Summary & Key Architectural Decisions

- **No network code, ever.** No provider calls, no API key, no HTTP dependency.
- **Read-only against source logs.** Only `~/.warden/` is written.
- **`None`/absent, never `0`,** for anything warden cannot derive — in records, JSON, and tables.
- **`store::Scanner` is the only read path** over `events/`; `output::emit` is the only `--json` branch.
- **The named reports (`reports::NAMES`) are a fixed set**; arbitrary grouping lives in `query`.
- **Pricing comes only from `config.toml`,** applied at read time; no prices in the binary.
- **Record and envelope fields may be added, never renamed/retyped/removed** without bumping `RECORD_VERSION`.
- **Ingest must stay idempotent and resumable**; never write a cursor for a partially-ingested file.
- **`suggest --draft` writes nothing to disk** — stdout only, in 0.1.0.
- **Tests live inline** next to the code, use `testkit`, and render with `Style::plain()`.
- **`Cargo.toml` is the single source of version truth** for both the crate and the wheel.
