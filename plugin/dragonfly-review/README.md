# dragonfly-review

Dragonfly's fan-out PR review as a Claude Code plugin: the
`dragonfly-review:review` skill spawns specialist reviewer subagents
(`review-agent` per concern, `comment-reviewer`, `dedup-reviewer`, and
`test-reviewer` when tests changed), each fed pre-collected diff context by
a SubagentStart hook that shells out to `dragonfly prompt`.

## Install

```
cargo install --git https://github.com/HalfVoxel/dragonfly --locked
claude plugin marketplace add HalfVoxel/dragonfly
claude plugin install dragonfly-review@dragonfly
```

The `dragonfly` binary must be on PATH (cargo installs to `~/.cargo/bin`;
make sure that's on PATH). Without it the hook fails open and the agents
fall back to plain `git diff` with degraded context. PR-area scoring and
duplicate-function hints additionally need the `kit` LLM helper on PATH
and are silently absent without it.

## What's included

| Component | Purpose |
| --- | --- |
| `skills/review` | Orchestrator: scope, comment triage, fan-out, report. Review-only: it never pushes, and replies to review threads only after user approval. |
| `agents/` | The four reviewer subagents (read-only tools; `model: inherit`) |
| `hooks/` | SubagentStart hook injecting `<dragonfly-context>` via `dragonfly prompt` |

`hooks/review-context.py` is a vendored copy of the repo's
`hooks/review-context.py` (plugins cannot reference files outside their
root), the four `agents/*.md` are adapted copies of the repo's `agents/`
files (`comment-reviewer` and `test-reviewer` inline the
`code-comments.md` guide their repo counterparts @-reference), and
`skills/review/SKILL.md` shares its policy blocks with `DRAGONFLY_SKILL`
in `src/skill.rs`. Sync deliberate changes to any of them both ways.

## Notes

- If you previously wired these agents/hooks manually in `~/.claude`, remove
  those copies; otherwise both hook registrations fire and the context is
  injected twice.
- The skill and subagents call a handful of `dragonfly` subcommands
  (`prompt`, `--areas`, `pr comments`, `dedup …`), all read-only except
  `dedup dismiss`, which records persistent not-a-duplicate verdicts.
  Allowlist them in your permissions settings to avoid prompts, e.g.
  `Bash(dragonfly prompt:*)`, `Bash(dragonfly --areas)`,
  `Bash(dragonfly pr comments:*)`, `Bash(dragonfly dedup:*)`.
