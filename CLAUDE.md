# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`spec-wave-agent` is a single-binary Rust daemon that runs on a developer's machine. It polls a GitHub repo for open issues labeled `spec-wave:dev-agent` (and typed `[STORY]`/`[TASK]`), acquires a distributed lease so only one agent works an issue at a time, and delegates implementation to `npx spec-wave implement <n>` (which invokes the dev's own Claude Code). It implements RFC-001 of the spec-wave workflow. Comments and log messages are in Portuguese — keep new ones in Portuguese too.

## Commands

- Build: `cargo build --release` (or `make build`)
- Tests: `cargo test` — unit tests are inline `#[cfg(test)]` modules; `tests/lease_integration.rs` exercises the lease protocol against a local bare git repo (no network). Single test: `cargo test roubo_apos_expirar`
- Run in foreground: `RUST_LOG=debug ./target/release/spec-wave-agent` (config required at `~/.config/spec-wave-agent/config.toml`, see `packaging/config.example.toml`)
- Install as service: `make install install-config install-systemd` (Linux) / `install-launchd` (macOS)

## Structure

Lib + bin crate: `src/lib.rs` exposes the modules (so integration tests can import them); `src/main.rs` is orchestration only (tracing init, preflight checks, heartbeat task, main poll loop, shutdown). Modules: `config.rs` (load/validate/remote_url/agent_id), `shell.rs` (`run`/`run_ok` + `retry_backoff`), `lease.rs` (the invariant-bearing module), `queue.rs` (`poll_queue` + pure `parse_queue`), `runner.rs` (workspace, checkpoint, `implement` with child output streaming).

## Architecture

The daemon is built around a **distributed lease implemented on git refs** — this is the core invariant to preserve when changing anything:

- **Lease = git ref as CAS.** Each claimed issue gets a ref `refs/heads/spec-wave-agent/claims/<n>` pointing to a commit containing only `lease.json` (`{issue, owner, generation, heartbeat}`). Commits are built with git plumbing (`hash-object`/`mktree`/`commit-tree`) in a `lease-repo` under the workdir — no worktree.
  - *Acquire*: non-forced push of a new ref — fails atomically if the ref already exists (losing the race is a normal outcome, not an error).
  - *Renew / steal*: `git push --force-with-lease=<ref>:<expected-sha>` — CAS against the exact sha last observed. A lease whose heartbeat is older than `lease_ttl_secs` may be stolen; `generation` increments on every acquire/steal and acts as the fencing token.
- **Fencing.** A background heartbeat task renews the lease every `heartbeat_secs`. `renew()` distinguishes `RenewError::Lost` (CAS rejected / owner-generation mismatch / ref gone → fence immediately, NEVER retry) from `Transient` (network error → retry allowed). On `Lost`, or when transient retries exhaust a budget of `lease_ttl_secs − 2×heartbeat_secs` since the last successful renew, the `implement` child is killed immediately and the agent does **not** checkpoint, push, or release — the new owner controls the branch (`RunEnd::LeaseLost`; keep this path side-effect-free). The budget guarantees self-fencing strictly before any legal steal (a steal requires `ttl` without a heartbeat *written to the remote*).
- **Retries** (`shell::retry_backoff`) are only for idempotent read/network ops: queue poll, workspace clone/fetch. Never wrap the lease CAS pushes in blind retries.
- **Checkpoint/resume.** Work happens on branch `agent/issue-<n>` in a per-issue clone under the workdir. On failure or graceful shutdown (SIGTERM → `RunEnd::Interrupted`), the agent commits+pushes WIP ("checkpoint") and releases the lease so another agent can take over instantly. Hard machine death → takeover after TTL via lease stealing, resuming from the last pushed commit.
- **One task at a time.** The main loop claims the first issue it can from the polled FIFO (by number); after finishing it (any outcome), it re-polls the queue fresh rather than continuing the stale list.
- **Labels are UX only.** The `spec-wave:dev-agent` label drives the queue but correctness rests entirely on the lease; the label is removed only on success. Failed items keep the label (stay queued) and get an explanatory `gh issue comment`.
- **Child streaming.** `implement` pipes the child's stdout/stderr through `BufReader::lines()` tasks logging `[#<issue>][out|err] …`. The pipe handles are `take()`n out of the child, so `child.kill()` in the fencing/shutdown select arms stays immediate — don't reintroduce anything that makes kill wait on the readers.
- **Testability seams**: `Config.remote_url` overrides the `https://github.com/{repo}.git` default (integration tests use a local bare repo as origin); `LeaseRepo::open` sets a local git identity (`commit-tree` needs one).

Config invariants are enforced by `Config::validate()` at boot — notably `lease_ttl_secs >= 4 × heartbeat_secs`.
