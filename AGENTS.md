# AGENTS.md

`docs/PLAN.md` is the contract. Do not change its decisions. Edit only the paths your task owns. When a frozen contract (plan section 6) must change, put the change and its reason at the top of your final report.

## Toolchain and tasks

- Rust from `rust-toolchain.toml`, edition 2024, the Cargo workspace of plan section 5.
- Pin the latest stable version of every crate, tool and GitHub Action exactly. Pin actions by full commit SHA. Check the current version before you add one. Prefer a crate already in `[workspace.dependencies]`.
- Tasks live in `tasks.toml`, never in `mise.toml`. Run them with `mise run <task>`: `check`, `test`, `lint`, `build`, `bench`, `gen`, `fmt`.
- Keep Cargo's default per-worktree `target/` and the default `~/.cargo` and rustup homes. Never set `CARGO_TARGET_DIR`, `CARGO_HOME` or `RUSTUP_HOME`.
- Write and edit files with file tools, not with sed, perl or shell redirection.
- After a change to `spec/openapi/`, run `mise run gen` and commit `crates/spec/src/ops_gen.rs`.

## Code

- `unsafe_code` is forbidden.
- Clippy `pedantic` is clean with `-D warnings`.
- `unwrap_used` and `expect_used` are denied outside tests. `print_stdout` and `dbg_macro` are denied.
- Allow a lint only on the item that needs it, with `#[expect(..., reason = "...")]`.
- Libraries use one `thiserror` enum per crate. `ApiError` is the error at the HTTP edge. No `anyhow` in library crates.
- No code comments, except `///` docs on public items that state a contract the signature does not show, and `Given`, `When`, `Then` markers in tests. Reasoning goes in commit messages.
- No global mutable state: no `static` with interior mutability, no `OnceLock` or `LazyLock` that a test can change. A fixed `Regex` or table may be a `LazyLock` that nothing writes.
- Timeouts, limits, clocks and environment lookups are fields or parameters. Read the environment through a `Fn(&str) -> Option<String>`.
- Parse an input once and pass the parsed form. Avoid per-request allocations on hot paths. Benchmark each hot path with `criterion`.

## Tests

- Never wait for real time. Use the manual `Clock` for wall time, and `#[tokio::test(start_paused = true)]` with `tokio::time` for timers, deadlines and pauses.
- Wait on the event you need (a channel, a drained queue), never on a duration.
- A test server binds its listener on port 0 before it serves. Use an in-process `hyper` service where the code takes a connector.
- Tests are independent and run in parallel under `cargo nextest`: no order between tests, no shared fixture that a test changes.
- Put temporary files only under `tempfile::TempDir`. An expensive shared fixture is a `LazyLock` that no test changes.
- Assert behaviour (status, error variant, fields and values), never the wording of a message a person reads.
- Take expected values from GitHub's behaviour and the plan, never from the code under test.
- Add tests for distinct behaviours and risks, not for volume.

## Git

- Work on the branch named in your task.
- Sign and sign off every commit: `git commit -sS`.
- Use conventional commit titles in lowercase: `type(scope): summary`, for example `feat(store): add the undo log`.
- Never add AI attribution: no `Co-Authored-By` line and no "Generated with" line.
- Push with `git push -u upstream <branch>` as its own command. The remote is `upstream`. Never create or push to a remote named `origin`.
- Never push to `main`, never force-push `main`, never merge, never open a pull request. The coordinator merges.

## Done

- Every check of your task's definition of done ran and passed on your final pushed commit.
- `mise run check` is green: `cargo fmt --check`, Clippy, `cargo deny check`, `cargo nextest run` and doc tests.
