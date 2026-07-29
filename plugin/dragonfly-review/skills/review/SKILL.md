---
name: review
description: Dragonfly PR review — fan out specialist review subagents over the current branch's diff before pushing or marking a PR ready. Use when the user asks to review the PR/branch/diff, before any git push of substantive changes, or before creating a PR. Runs in the main session and spawns review-agent subagents (one per concern) plus comment-reviewer, dedup-reviewer, and (when tests changed) test-reviewer subagents, all fed pre-collected diff/context by the dragonfly CLI. Review-only — it reports findings and fixes them only after the user picks.
---

# Dragonfly review

Review the current branch's changes (the PR) using Dragonfly's fan-out review flow. You orchestrate; the `review-agent` / `comment-reviewer` / `test-reviewer` / `dedup-reviewer` subagents (this plugin's agents — their types may appear namespaced, e.g. `dragonfly-review:review-agent`) do the reading. Review-only: report findings first, fix only what the user picks.

The `dragonfly` CLI is required (install: `cargo install --git https://github.com/HalfVoxel/dragonfly --locked`). If it's missing, fall back to plain `git`/`gh` and a single `review-agent` with the diff scope in the prompt.

## Phase 0 — Establish scope and warm the context

1. If the invocation names a specific area or file list, focus the fan-out there; otherwise review the whole branch diff.
2. `git fetch origin` so remote refs are current.
3. Warm the shared review context (background it; takes ~10–60s cold — it runs PR-area scoring, CLAUDE.md/AGENTS.md chunk ranking, and duplicate-function detection):
   ```bash
   dragonfly prompt review-agent > /dev/null
   ```
   This populates a 4-minute per-cwd cache that the SubagentStart hook serves to every subagent, so the fan-out pays the build cost once. Stderr reports cache hit/build timing.
4. While that runs, do Phase 1. Then get the area breakdown (cached by HEAD SHA after the warm):
   ```bash
   dragonfly --areas
   ```
   Each area carries `potential_for_bugs` and `potential_for_simplification` (1–10). Base-ref detection (origin/main, or the Graphite parent for stacked PRs) is built in — never diff against local `main` yourself.

## Phase 1 — Existing review feedback

If a PR exists for this branch (`gh pr view` succeeds):

- Run `dragonfly pr comments` for cleaned review threads (with thread IDs), top-level reviews, and PR metadata. Do not hand-assemble `gh api` GraphQL.
- For every unresolved bot or human comment: **read the flagged code yourself — never blindly trust the bot.** Common false positives: style suggestions that conflict with project conventions, "unused" warnings for code used via reflection/DI, security warnings on intentional patterns, suggestions that would break existing behavior.
- Triage each into: valid (and what the fix should be) or dismiss (and why). Keep thread IDs so replies/resolves are possible later. Do not reply or resolve until the user approves; then use `dragonfly pr thread comment --thread-id … --body "Fixed in <sha>"` followed by `dragonfly pr thread resolve --thread-id …`.
- Reply wording: to humans, only concise informational statements — "Fixed in <sha>" or "Fixed in <sha>, X now does Y" — never rationale or conversation. To bots, rationale is allowed when dismissing a false positive, so they don't re-report it later.

## Phase 2 — Fan out reviewers

Spawn all subagents **in a single message** (parallel Agent tool calls). Their `<dragonfly-context>` (diffs, areas, relevant-context chunks; the dedup-reviewer instead gets the inlined duplicate-function hints) arrives automatically via the SubagentStart hook — **do NOT re-inline the diff or file index in prompts.** Pass only the concern, the target area/files, and the area's scores.

Pick the fan-out from the area breakdown:

- **Correctness** — one `review-agent` per area with `potential_for_bugs` ≥ 6. For those, instruct it to be aggressive: trace all code paths the change touches and follow call chains beyond the diff hunks.
- **Simplification** — one `review-agent` if any area has `potential_for_simplification` ≥ 6, pointed at the highest-scoring areas.
- **Deployment edge cases** — one `review-agent` if the diff touches wire formats, API contracts, schemas, enums, or feature flags. Remind it: deploys are gradual (~10 minutes), so old and new frontends/backends coexist and can call each other in mixed orders (e.g. old frontend → new backend → old backend).
- **Comments & docs** — always one `comment-reviewer` (a single instance is sufficient; it typically needs no extra instructions).
- **Test quality** — one `test-reviewer` if the PR adds or modifies test files (structure, duplicate or low-value tests, assertion style, coverage gaps). A single instance is sufficient; it typically needs no extra instructions.
- **Duplication** — always one `dedup-reviewer`. Its hook context inlines the full duplicate-function hint list; it validates each hinted pair (dismissing false positives itself via `dragonfly dedup dismiss`) and also hunts for duplication the hint pipeline cannot see: repeats within the PR itself, non-Go code, sub-function copies. Don't have other subagents re-check the hints, and don't read the hints yourself.

Small PR (all scores < 6, few files): one `review-agent` over the whole diff + the `comment-reviewer` + the `dedup-reviewer`.

## Phase 3 — Aggregate and report

Merge subagent findings into a single markdown report (omit empty sections):

1. **Issues** — numbered, ordered by severity (`🔴 Critical` / `🟡 Medium` / `🟢 Low`). Each: file:line, one-two sentence explanation, concrete suggested fix, confidence. Dedupe overlapping findings across subagents.
2. **Review-comment triage** — per existing thread: valid (suggested action) or dismiss (reason); include thread IDs.
3. **Pre-existing issues (informational)** — wrong on the base, not caused by this PR; never fix without the user's say-so.
4. **Dedup candidates** — the `dedup-reviewer`'s verified genuine duplicates (function, existing counterpart, suggested direction) and its broader duplication findings. It dismisses false positives itself (`dragonfly dedup dismiss`; persists across worktrees — `dragonfly dedup exclusions` lists them), so only its genuine/unsure verdicts reach this report.
5. **Verdict** — one line, e.g. "2 critical, 3 medium — not ready to push" or "No blocking issues found".

Ask which issues to fix as free-form text (not AskUserQuestion). After fixing approved issues, re-run the relevant `review-agent` at most once.

## Phase 4 — Stamp the review

After reporting (the stamp means "reviewed", not "approved"), record the reviewed HEAD (kept as a review log; also consumed by the optional push-gate hook if it's ever re-enabled):

```bash
mkdir -p ~/.dragonfly/reviewed
echo "$(date -u +%FT%TZ) branch=$(git rev-parse --abbrev-ref HEAD) issues=<N>" > ~/.dragonfly/reviewed/$(git rev-parse HEAD)
```

Fixing issues produces a new SHA, which correctly requires a fresh review.

## Meta-feedback

If you hit friction caused by this process itself (missing subcommand, ambiguous instruction, data you had to re-derive), log it: `dragonfly --feedback "..."` — short and concrete. Never use it for PR-level findings.
