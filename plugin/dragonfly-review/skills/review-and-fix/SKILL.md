---
name: review-and-fix
description: Review the current branch to find clear improvements to apply before presenting a completed task to a human. Use after the end of an implementation session to tidy up the code before a human looks at it.
---

# Dragonfly review and fix

Review the current branch's changes by spawning a few subagents, then
fix what are clear improvements and hand back a list of what needs a decision. The
reviewers are read-only, and you apply every edit yourself.

Use `dragonfly-review:review` instead when the user wants to see findings
before anything changes, or when the PR needs a deeper revew.

## Phase 0 — Warm the context

1. `git fetch origin` so remote refs are current.

## Phase 1 — Fan out four reviewers

Spawn all subagents **in a single message** (parallel Agent tool calls).
The subagents automatically some receive guidance and a diff of the current PR.
They receive only the diff for comitted changes.

| `subagent_type` | Focus to pass in the prompt |
| --- | --- |
| `review-agent` | Correctness: bugs, broken invariants, missing error/nil handling, races, resource leaks, security holes. Trace call chains beyond the diff hunks. |
| `review-agent` | Simplification: duplicate code, oversized functions, repetitive patterns, dead code, needless indirection. Non-test code only — the test reviewer owns tests. |
| `test-reviewer` | Nothing: it needs no additional guidance. |
| `comment-reviewer` | Nothing: it needs no additional guidance. |

## Phase 2 — Triage every finding: fix or defer

The rule: **fix only what a competent reviewer would agree on without
discussion.** One correct change, no judgement call, no alternatives worth
weighing. Everything else is deferred.

For each potential fix:

- Read files you need to clearly understand the issue and the fix, don't blindly trust the subagents.
- See if you agree with the subagent assesments.
- Err on the side of simplification. If you have to do large workarounds for some behavior, it might be good to take a step back and think about if the system as a whole could be written more elegantly.
- Do not commit, do not push, do not reply to or resolve PR threads. Leave the tree dirty for the user to read.

## Phase 3 — Report

Two terse sections:

1. **Fixed** — one line per change: `file:line`, what changed, which reviewer raised it.
2. **Deferred** — one line per finding, severity order: `file:line`, the finding, why it needs a decision, and the options if there are two.

If there are any non-trivial deferred findings, launch the /grill-me-with-docs skill.

## Meta-feedback

If you hit friction caused by this process itself (missing subcommand,
ambiguous instruction, data you had to re-derive), log it:
`dragonfly --feedback "..."` — short and concrete. Never use it for findings
about the code under review.
