# dragonfly-review

Dragonfly's fan-out PR review as a Claude Code plugin: the
`dragonfly-review:review` and `dragonfly-review:review-and-fix` skills
spawn specialist reviewer subagents
(`review-agent` per concern, `comment-reviewer`, `dedup-reviewer`,
`test-reviewer` when tests changed, and a `design-reviewer` seeded with
the verified bugs), each fed pre-collected diff context by
a SubagentStart hook that shells out to `dragonfly prompt`.

## Install

```
cargo install --git https://github.com/HalfVoxel/dragonfly --locked
claude plugin marketplace add HalfVoxel/dragonfly
claude plugin install dragonfly-review@dragonfly
```

The `dragonfly` binary must be on PATH (cargo installs to `~/.cargo/bin`;
make sure that's on PATH). Without it the SubagentStart hook cannot inject
`<dragonfly-context>`, and it cannot stop the spawn either (SubagentStart
hooks only warn). Both skills therefore run `dragonfly prompt` in the
orchestrator first, so a missing binary fails there, before any reviewer
starts. PR-area scoring additionally needs the `kit` LLM helper on PATH and
is silently absent without it.

## What's included

| Component | Purpose |
| --- | --- |
| `skills/review` | Orchestrator: scope, comment triage, fan-out, report. Review-only: it never pushes, and replies to review threads only after user approval. |
| `skills/review-and-fix` | Fixed four-reviewer fan-out (correctness, simplification, tests, comments), then applies the unambiguous fixes and defers the rest. Never commits or pushes. |
| `agents/` | The five reviewer subagents (read-only tools; `model: opus`) |
| `hooks/` | SubagentStart hook injecting `<dragonfly-context>` via `dragonfly prompt` |

`hooks/review-context.py` is a vendored copy of the repo's
`hooks/review-context.py` (plugins cannot reference files outside their
root), and `skills/review/SKILL.md` shares its policy blocks with
`DRAGONFLY_SKILL` in `src/skill.rs`. Sync deliberate changes to either
one both ways.

The five `agents/*.md` bodies are **generated** from the repo's
`agents/` files — edit those, then run `dragonfly gen-plugin-agents`.
Frontmatter here is hand-maintained (the plugin pins `tools:` for
read-only enforcement and `model: opus`); only the body is derived.
`src/gen_plugin.rs` declares the body deltas, and
`plugin_agents_match_generated` fails the build when a copy goes stale.

## Notes

- If you previously wired these agents/hooks manually in `~/.claude`, remove
  those copies; otherwise both hook registrations fire and the context is
  injected twice.
- The skill and subagents call a handful of `dragonfly` subcommands
  (`prompt`, `--areas`, `pr comments`, `dedup`), all read-only.
  Allowlist them in your permissions settings to avoid prompts, e.g.
  `Bash(dragonfly prompt:*)`, `Bash(dragonfly --areas)`,
  `Bash(dragonfly pr comments:*)`, `Bash(dragonfly dedup:*)`.
