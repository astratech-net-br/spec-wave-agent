# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`spec-wave-agent` is a single-binary Rust daemon that runs on a developer's machine. It polls a GitHub repo for open issues labeled `agent:queued` (and typed `[STORY]`/`[TASK]`), acquires a distributed lease so only one agent works an issue at a time, and delegates implementation to `npx spec-wave implement <n>` (which invokes the dev's own Claude Code). It implements RFC-001 of the spec-wave workflow. Comments and log messages are in Portuguese.

## Layout and build

The entire implementation is `main.rs` at the repo root — there is no `src/` directory, and `Cargo.toml` has no `[[bin]]` path entry, so `cargo build` will not find the entry point as-is (Cargo expects `src/main.rs`). If a build is needed, either move/symlink the file to `src/main.rs` or add a `[[bin]] path` to `Cargo.toml`; check with the user before restructuring.

- Build (per INSTALL.md): `cargo build --release`, then copy `target/release/spec-wave-agent` to `/usr/local/bin/`
- There are no tests and no lint configuration.
- Runtime config lives at `~/.config/spec-wave-agent/config.toml` (see INSTALL.md for the schema). Required machine deps: authenticated `git`/`gh`, Node 18+, Claude Code.

## Architecture (all in main.rs)

The daemon is built around a **distributed lease implemented on git refs** — this is the core invariant to preserve when changing anything:

- **Lease = git ref as CAS.** Each claimed issue gets a ref `refs/heads/spec-wave-agent/claims/<n>` pointing to a commit containing only `lease.json` (`{issue, owner, generation, heartbeat}`). Commits are built with git plumbing (`hash-object`/`mktree`/`commit-tree`) in a bare-ish `lease-repo` under the workdir — no worktree.
  - *Acquire*: non-forced push of a new ref — fails atomically if the ref already exists (losing the race is a normal outcome, not an error).
  - *Renew / steal*: `git push --force-with-lease=<ref>:<expected-sha>` — CAS against the exact sha last observed. A lease whose heartbeat is older than `lease_ttl_secs` may be stolen; `generation` increments on every acquire/steal and acts as the fencing token.
- **Fencing.** A background heartbeat task renews the lease every `heartbeat_secs`. If a renew fails (someone stole the lease), the `implement` child process is killed immediately and the agent does **not** checkpoint, push, or release — the new owner controls the branch. This is the `RunEnd::LeaseLost` path; keep it side-effect-free.
- **Checkpoint/resume.** Work happens on branch `agent/issue-<n>` in a per-issue clone under the workdir. On failure or graceful shutdown (SIGTERM → `RunEnd::Interrupted`), the agent commits+pushes WIP ("checkpoint") and releases the lease so another agent can take over instantly. If the machine dies hard, takeover happens after the TTL via lease stealing, resuming from the last pushed commit.
- **Labels are UX only.** The `agent:queued` label drives the queue but correctness rests entirely on the lease; the label is removed only on success. Failed items keep the label (stay queued) and get an explanatory `gh issue comment`.
- **Concurrency = 1 per agent.** The main loop processes queued issues sequentially, FIFO by issue number.

Config invariant (from INSTALL.md): `lease_ttl_secs >= 4 × heartbeat_secs`, so network slowness doesn't cause spurious lease steals.
