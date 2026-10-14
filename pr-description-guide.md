# PR description guide

Pick a sensible subset of sections per PR. A tiny fix may only need `## Summary` and `## Changes`, while a CLI feature usually also wants `## Example`.
This guide complements the repo rules in `AGENTS.md`, and overrides them where they conflict.

## Title

Use semantic commit format: `type(scope): description`, e.g. `fix(auth): resolve OAuth token refresh race`. Types: `feat`, `fix`, `docs`, `chore`, `refactor`, `style`, `perf`, `test`, `ci`, `revert`. The description is imperative ("add", not "added"), starts lowercase and has no trailing period. Keep the whole title within 100 characters: the squash merge makes it the commit subject on `main`. A GitHub-generated `Revert "…"` title stays as-is.

## Layout

Use the sections below, in this order, and skip the ones that don't fit. In the PR body, each section is a `##` heading and the subsections of Low level are `###` headings.

### Summary

Explain what the affected systems do, and how the problem ties into them. Write for a reader who knows the product but not the codebase. Avoid code identifiers. Use terms from ./GLOSSARY.md where relevant. 2-5 sentences, prose only, no bullets.

Start with a short paragraph of 1-2 sentences, at most 25 words: the problem, then what the PR does about it. Exceed this (up to 35 words) only when a shorter TL;DR would read poorly or need strained grammar. It should work as a high-level summary of the whole PR.

Use paragraphs if necessary to keep it scannable (\n\n).

### Changes

1-3 bullets on what the PR does, at the same high level as the Summary. List only changes: motivation and context belong in the Summary.

### Rollout order

What one should think about before merging this PR, e.g. "Merge only after <linked PR> has been fully deployed (go-api and web). If not, old pods …". Do not list other PRs in the stack that depend on this one; their dependencies go in their own PRs. 1-2 sentences: start with what one must or must not do, then why.

A github/graphite PR stack will always merge in the specified order. Don't re-specify the order here as it's redundant, unless there are more constraints like time between merges.

### Screenshots

Required for any change with UI impact (a new screen, a changed component, a copy or layout tweak): before/after screenshots, side by side in a table, plus a recording when the change involves interaction or motion. A page or component under `web/` or `packages/ui/` also shows the after state at phone width (390 px). Give every image alt text that says what it shows.

### Example

Include when usage is worth showing (e.g. CLI changes, new APIs). Use a fenced code block.

### Feature flags

List any flag that gates the change, as a link to Confidence, e.g. "All functionality gated behind [new-trajectory/constructPrompt](…) (rolled out to 5%)."

When you use the repo's `| Flag | Treatment | Effect on app |` table, every Flag cell is a Confidence link as well, overriding AGENTS.md's plain-text rule.

### Links

For bug fixes or to motivate a feature: Grafana, Braintrust trace or similar links that show the bug happening. A performance, eval or prompt change links the before/after run or score.

Linear tickets go here too, one line per ticket with the id as a link: `Closes [DX-123](https://linear.app/lovable/issue/DX-123)` when the merge completes the ticket, `Part of [DX-123](…)` when work remains after merge, `Related to [DX-123](…)` for a ticket the PR does not close. An integration automatically updates Linear status based on these lines.

### Low level

For a reader who knows the codebase but not this area. Open directly with a prose paragraph (1-4 sentences) of the background needed to evaluate the change. Code identifiers are welcome here. Then:

#### Important behavioral changes

0-3 bullets. Behavior changes a reviewer might object to.

#### Changes

1-6 bullets, one concrete change each. Keep them scannable.

## Revert PRs

The body starts with the literal line `This reverts commit <sha>` (`git revert` writes it; keep it), followed by one line on what regressed or why you are reverting. The high-risk gate's incident auto-pass matches only that line, a `Revert "` title or a `revert-<n>-` branch; "Reverts #NNNNN" alone does not count. The usual sections follow.

## PR examples

- __DRAGONFLY_ROOT__/pr-descriptions/bug-fix.md: small fix with Links.
- __DRAGONFLY_ROOT__/pr-descriptions/cli-feature.md: CLI change with an Example.
- __DRAGONFLY_ROOT__/pr-descriptions/refactor.md: staged refactor with a Rollout order.
- __DRAGONFLY_ROOT__/pr-descriptions/stacked-prerequisite.md: first PR of a stack, readers before the writer.
- __DRAGONFLY_ROOT__/pr-descriptions/large-fix.md: incident follow-up with Important behavioral changes.

## Graphs (encouraged)

Including a relevant rendered graph in the PR description is **encouraged**, especially when the change:

- Fixes a bug: show the panel that captures the bug (error rate, latency spike, panic count) so reviewers can see the problem.
- Improves a metric or touches a hot path: show the baseline panel for the affected endpoint / tool / node.

A reference for the production Grafana setup, the catalog of dashboards, and how to convert any panel URL into a `/render/d-solo/...` PNG lives at __DRAGONFLY_ROOT__/grafana_dashboards.md.

Read that file when you need to render a panel. The `GRAFANA_TOKEN` and `GRAFANA_HOST` env vars are already exported in this environment, so you can `curl` the render endpoint directly, no setup required. Save the PNG under `/tmp/`.

To embed the PNG in a PR body or comment, upload it with the `gh image` extension and use the markdown reference it prints:

```bash
gh image /tmp/panel.png
# -> ![panel.png](https://github.com/user-attachments/assets/...)
```

It pulls the session token from browser cookies automatically. Then update the PR with `gh pr edit <number> --body-file <new-body.md>`. Do **not** put the raw `/render/d-solo/...` URL in the PR body: it requires the service-account token and won't render for anyone else.

## Hard rules

- **No "Test plan" / "Test checklist" / "Testing" section.** Reviewers don't need it; CI runs the tests.
- Separate paragraphs with a blank line, as a single newline renders as the same paragraph.
- Bullets are one sentence each, no trailing prose underneath.
- Link a feature flag (to `https://app.confidence.spotify.com/flags/<name>`) and a Linear ticket at their first mention, wherever that is. Later mentions can stay plain, except in the Feature flags and Links sections, where every mention is a link.
- Put a long machine report (benchmark output, eval tables, logs) inside `<details>` under the prose, never inline.
- Keep it short. If a bullet doesn't add information, cut it.
- No emojis. No "Co-Authored-By" footers in the body. No Slack/Cursor/external-tool links.
- Skip CI-self-evident items (lint passes, typecheck passes, "CI green").
