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
4. **Hand off to Claude** — `exec`s `claude` with a curated prompt and the bundled push-and-fix skill, so the agent fixes failures and replies to review comments.

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

The binary is `push-and-check`.

## Usage

```bash
push-and-check                  # push, wait for CI, launch Claude to fix
push-and-check --force          # force-push (after rebase)
push-and-check --areas          # print the PR area analysis and exit
push-and-check prompt           # build & print the Claude prompt without launching
```

PR review-thread helpers (used by the skill, but callable directly):

```bash
push-and-check pr thread comment --thread-id <id> --body "..."
push-and-check pr thread resolve --thread-id <id>
push-and-check pr description -          # read body from stdin
push-and-check pr comments               # print review threads, reviews, and meta (cleaned)
push-and-check pr comments --pr 12345    # explicit PR number
push-and-check --feedback "..."  # append a note to ~/.dragonfly/feedback
```

## Requirements

- `git`, `gh` (GitHub CLI, authenticated)
- `claude` (Claude Code) on `PATH`
- Rust 2024 edition toolchain to build
