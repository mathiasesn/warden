# AGENTS.md — working on warden

**Read [`ARCHI.md`](ARCHI.md) first** — it is the architecture source of truth. This file is
*how to work* here; update ARCHI.md when the architecture changes. `cli.rs` and
`reports::NAMES` are authoritative for the user-facing surface (commands, flags, output);
[`README.md`](README.md) and ARCHI.md §8 are both derived — see "Adding things" below.

warden is a local, read-only Rust CLI (crate `warden-cli`, binary `warden`) over a coding
agent harness's own session logs → append-only JSONL in `~/.warden/`.

## Non-negotiables

Break one of these and the change is wrong, however well it works:

1. **`None`/absent, never `0`.** Anything warden cannot derive is `None` in a record, absent in
   JSON, `Cell::Unsupported` (a dim `–`) in a table. The most load-bearing rule in the codebase.
2. **One read path (`store::Scanner`), one presentation layer (`output::emit`).** Never open a
   partition file or branch on `--json` anywhere else. The one deliberate exception is `suggest
   --draft <id>`, which writes the `SKILL.md` draft straight to `io::stdout()` because the draft
   *is* the payload; the parser makes `--draft` and `--json` mutually exclusive so there is
   nothing to branch on.
3. **No network.** No provider calls, no API key, no dependency that can make a request.
4. **Read-only against sources.** warden writes only its own store (`purge` rewrites it by design).
5. **Add record/envelope fields freely; never rename, retype, or remove** without bumping
   `RECORD_VERSION`.
6. **Ingest stays idempotent and resumable.** Never write a cursor for a partially-ingested file.
7. **New runtime dependencies are a project decision — propose, don't add.** Network-capable,
   async, and storage deps are prohibited outright; the absence is the design.

Assume and finish for implementation choices. Stop and ask when a change would hit one of the
above — a new dependency, a `RECORD_VERSION` bump, a change to the report set.

## Checks before you claim done

While iterating, `cargo check --all-targets`. Before claiming done, run what CI runs
(`.github/workflows/ci.yml` is the authority; `RUSTFLAGS: -D warnings` is set job-wide). The
`check` job:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

The `python-packaging` job always builds the wheel and asserts crate, wheel, and binary
versions agree, then runs the Python discovery-shim tests — run it locally whenever you
touched `python/`, packaging, or anything version-related:

```bash
uv venv .venv && uv pip install --python .venv/bin/python .
.venv/bin/warden --version && .venv/bin/python -m warden --version
uv pip install --python .venv/bin/python pytest   # separate, later step: don't re-sync the env just verified
.venv/bin/python -m pytest python/tests -q
```

This subset is enough to catch a mismatch locally; `ci.yml` is the full assertion (it also
reads the wheel's own metadata version).

The `msrv` job pins the toolchain to 1.87 and runs `cargo check --all-features` (build-only;
`cargo test --all-features` already runs on stable above) — run it locally with the 1.87
toolchain installed whenever a change might use an API newer than the floor.

Versions are `release-plz`'s job — it owns bumping `Cargo.toml`'s `version` and the changelog.
Never hand-edit the version; `pyproject.toml`'s version fields stay `dynamic` on purpose.

## Verifying by hand

Unit tests are the verification path; prefer `reports::testkit` / `suggest::testkit` fixtures.
If you need to see real output, never exercise the binary against the default root — pass
`--data-dir <tmp>`, since running warden *writes* a store even though it only reads sources.

## Testing conventions

- Inline `#[cfg(test)] mod tests` in the file under test. There is no Rust integration-test
  (`tests/`) directory; do not create one — `python/tests/` is unrelated (Python discovery
  shim). Render with `Style::plain()` so no ANSI leaks into assertions.
- Cover the absence case, not just the happy path: a missing figure must assert `–`/absent,
  never `0`.

## Adding things

Recipes for a command, a report, and an adapter are in ARCHI.md §8 and §10 — follow them
exactly; the registration steps are easy to miss. What those sections leave implicit:

- The named report set is closed by policy. Adding one is a project decision, not a routine
  change — if the need is arbitrary grouping, it belongs in `query`.
- A change to the user-facing surface updates both `README.md` and `ARCHI.md` §8 — the code
  wins if they ever disagree.

## `specs/`

Gitignored (`specs/.gitignore` = `*`) and excluded from the crate and wheel; holds local
per-change specs. Opt-in — write one only when asked (e.g. `/plan-task`), never unprompted.
If a spec already covers the current change, read and follow it.

## House style

- Module-level doc comments carry the *why* for local decisions — read them before changing a
  module, and update them when the reasoning changes.
- Long comments in `.github/workflows/*` are warnings, not commentary. Read them before editing
  a workflow.
- Don't commit or push unless asked. Never touch `target/` or `.env`.
