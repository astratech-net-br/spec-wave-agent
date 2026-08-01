# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`spec-wave-agent` is a single-binary Rust daemon that runs on a developer's machine. It polls a GitHub repo for open issues labeled `spec-wave:dev-agent` **typed `[FEATURE]` or `[BUG]`** (bugs first — corrective work carries severity, new features don't), acquires a distributed lease so only one agent works a feature at a time, and delegates orchestration to Claude Code: the configurable `feature_prompt` (sent via stdin to `feature_command`, default `claude -p …`) instructs it to run `npx spec-wave order <feature>` for the story dependency order and `npx spec-wave implement <story>` per story, parallelizing independent stories with sub-agents. For a `[BUG]` the `bug_prompt` instructs a four-phase fix instead (reproduce with a failing test → root cause → minimal fix → regression test), and the completion marker carries the root cause back, which the agent posts as an issue comment. The Rust agent is deliberately thin — no DAG/scheduler logic lives here. It implements RFC-001 of the spec-wave workflow. Comments and log messages are in Portuguese — keep new ones in Portuguese too.

## Commands

- Build: `cargo build --release` (or `make build`)
- Tests: `cargo test` — unit tests are inline `#[cfg(test)]` modules; `tests/lease_integration.rs` exercises the lease protocol against a local bare git repo (no network). Single test: `cargo test roubo_apos_expirar`
- Run in foreground: `RUST_LOG=debug ./target/release/spec-wave-agent` (config required at `~/.config/spec-wave-agent/config.toml`, see `packaging/config.example.toml`)
- Install as service: `make install install-config install-systemd` (Linux) / `install-launchd` (macOS)

## Structure

Lib + bin crate: `src/lib.rs` exposes the modules (so integration tests can import them); `src/main.rs` is orchestration only (tracing init, preflight checks, heartbeat task, main poll loop, shutdown). Modules: `config.rs` (load/validate/remote_url/agent_id), `shell.rs` (`run`/`run_ok` + `retry_backoff`), `lease.rs` (the invariant-bearing module), `queue.rs` (`poll_queue` + pure `parse_queue`, returning `QueueItem { kind, number }`), `runner.rs` (workspace, checkpoint, `run_feature_executor` with prompt-via-stdin and child output streaming).

## Architecture

The daemon is built around a **distributed lease implemented on git refs** — this is the core invariant to preserve when changing anything:

- **Lease = git ref as CAS.** Each claimed issue gets a ref `refs/heads/spec-wave-agent/claims/<n>` pointing to a commit containing only `lease.json` (`{issue, owner, generation, heartbeat}`). Commits are built with git plumbing (`hash-object`/`mktree`/`commit-tree`) in a `lease-repo` under the workdir — no worktree.
  - *Acquire*: non-forced push of a new ref — fails atomically if the ref already exists (losing the race is a normal outcome, not an error).
  - *Renew / steal*: `git push --force-with-lease=<ref>:<expected-sha>` — CAS against the exact sha last observed. A lease whose heartbeat is older than `lease_ttl_secs` may be stolen; `generation` increments on every acquire/steal and acts as the fencing token.
- **Fencing.** A background heartbeat task renews the lease every `heartbeat_secs`. `renew()` distinguishes `RenewError::Lost` (CAS rejected / owner-generation mismatch / ref gone → fence immediately, NEVER retry) from `Transient` (network error → retry allowed). On `Lost`, or when transient retries exhaust a budget of `lease_ttl_secs − 2×heartbeat_secs` since the last successful renew, the `implement` child is killed immediately and the agent does **not** checkpoint, push, or release — the new owner controls the branch (`RunEnd::LeaseLost`; keep this path side-effect-free). The budget guarantees self-fencing strictly before any legal steal (a steal requires `ttl` without a heartbeat *written to the remote*).
- **Retries** (`shell::retry_backoff`) are only for idempotent read/network ops: queue poll, workspace clone/fetch. Never wrap the lease CAS pushes in blind retries.
- **Checkpoint/resume.** Work happens on branch `agent/issue-<n>` in a per-issue clone under the workdir. On failure or graceful shutdown (SIGTERM → `RunEnd::Interrupted`), the agent commits+pushes WIP ("checkpoint") and releases the lease so another agent can take over instantly. Hard machine death → takeover after TTL via lease stealing, resuming from the last pushed commit.
- **One item at a time.** The main loop claims the first item it can from the polled queue (ordered `(kind, number)` — bugs before features); after finishing it (any outcome), it re-polls fresh rather than continuing the stale list. The item's `QueueKind` selects command, prompt and timeout in `executor_round`; everything else — rounds, checkpoint, fencing, `kill_tree` — is deliberately identical for both types.
- **Labels are UX only.** The `spec-wave:dev-agent` label drives the queue but correctness rests entirely on the lease; the label is removed only on success (after a safety-net checkpoint push). Failed items keep the label (stay queued) and get an explanatory `gh issue comment` on the feature.
- **Executor + process-tree kill.** `runner::run_feature_executor` spawns `feature_command` (argv split by `config::split_command` — quotes supported, no shell) as a **process-group leader** (`process_group(0)`), writes the rendered `feature_prompt` to its stdin, and streams stdout/stderr as `[#<feature>][out|err] …`. Kill paths use `kill_tree` (SIGKILL to `-pid`, the whole group) because the executor spawns `npx spec-wave` which spawns an inner Claude — killing only the direct child would orphan grandchildren and break fencing. The pipe handles are `take()`n out of the child so the kill never waits on the readers.
- **Testability seams**: `Config.remote_url` overrides the `https://github.com/{repo}.git` default (integration tests use a local bare repo as origin); `LeaseRepo::open` sets a local git identity (`commit-tree` needs one).

Config invariants are enforced by `Config::validate()` at boot — notably `lease_ttl_secs >= 4 × heartbeat_secs`.
