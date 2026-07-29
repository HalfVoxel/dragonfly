<p align="center">
  <img src="logo-transparent.png" alt="dragonfly logo" width="220" />
</p>

# Dragonfly

A Rust CLI that helps with reviewing and monitoring pull requests.

The cli pushes the current branch, waits for CI, collects the failure logs and review threads, and then hands the whole package to Claude Code with a large prompt so the agent can review and help land the PR.

## What it does

1. **Push** — Pushes the latest commit.
2. **Watch CI** — streams `gh pr checks` and pulls logs for any failing jobs.
3. **Collect context** — changed files, commits, recent Claude Code sessions on the branch, unresolved PR review threads, and merge-conflict status against `origin/main`.
4. **Hand off to Claude** — `exec`s `claude` with a curated prompt and the bundled dragonfly skill, so the agent fixes failures and replies to review comments.

## Benefits

Why use this instead of just letting claude do everything itself?

* It automatically spawns multiple review subagents which finds a lot more issues than claude does on its own.
* Reading existing files is faster for claude than invoking git commands itself.
* Claude often runs the wrong git commands (e.g. diffs against `main` when it should use `origin/main`)
* Claude often doesn't realize it's in a graphite stacked PR by itself.
* Claude is automatically prompted to follow a strict sequence of phases, which covers much more than it does by itself, unprompted.
* Dangerous git commands require approval via a hook, even if claude runs the rest with dangerously-skip-permissions.
* Responds to review comments in a concise way if asked. Always labels ai comments with a footer.

## Install

```bash
cargo install --path .
```

The binary is `dragonfly`.

## Usage

```bash
dragonfly                  # push, wait for CI, launch Claude to fix
dragonfly --force          # force-push (after rebase)
dragonfly --areas          # print the PR area analysis and exit
dragonfly prompt           # build & print the Claude prompt without launching
```

PR review-thread helpers (used by the skill, but callable directly):

```bash
dragonfly pr thread comment --thread-id <id> --body "..."
dragonfly pr thread resolve --thread-id <id>
dragonfly pr description -          # read body from stdin
dragonfly pr comment --body "..."   # post a top-level PR comment (current branch's PR)
dragonfly pr comment --body -       # ...or read the body from stdin
dragonfly pr comments               # print review threads, reviews, and meta (cleaned)
dragonfly pr comments --pr 12345    # explicit PR number
dragonfly --feedback "..."  # append a note to ~/.dragonfly/feedback
```

## Claude Code plugins

This repo is a Claude Code plugin marketplace
(`claude plugin marketplace add HalfVoxel/dragonfly`):

- [`dragonfly-review`](plugin/dragonfly-review/) — the fan-out PR review flow
  as an installable skill + reviewer subagents + context-injection hook, for
  use from any Claude Code session (no orchestrator run required).
- [`dragonfly-watcher`](plugin/dragonfly-watcher/) — a channel that pushes CI
  results and new PR comments into the running session.

## Requirements

- `git`, `gh` (GitHub CLI, authenticated)
- `claude` (Claude Code) on `PATH`
- Rust 2024 edition toolchain to build
