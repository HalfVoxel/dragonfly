# PR description guide

Pick a sensible subset of sections per PR. A tiny fix may only need `# Summary` and `# Changes`, while a CLI feature usually also wants `# Example`.
This guide complements the repo rules in `AGENTS.md`, and overrides them where they conflict.

## Layout

Use the sections below, in this order, and skip the ones that don't fit.

# Summary

Explain what the affected systems do, and how the problem ties into them. Write for a reader who knows the product but not the codebase. Avoid code identifiers. Use terms from ./GLOSSARY.md where relevant. 2-5 sentences, prose only, no bullets.

Start with a short paragraph of 1-2 sentences, at most 25 words: the problem, then what the PR does about it. Exceed this (up to 35 words) only when a shorter TL;DR would read poorly or need strained grammar. It should work as a high-level summary of the whole PR.

Use paragraphs if necessary to keep it scannable (\n\n).

# Changes

1-3 bullets on what the PR does, at the same high level as the Summary. List only changes: motivation and context belong in the Summary.

# Rollout order

What one should think about before merging this PR, e.g. "Merge only after <linked PR> has been fully deployed (go-api and web). If not, old pods …". Do not list other PRs in the stack that depend on this one; their dependencies go in their own PRs. 1-2 sentences: start with what one must or must not do, then why.

# Example

Include when usage is worth showing (e.g. CLI changes, new APIs). Use a fenced code block.

# Feature flags

List any flag that gates the change, as a link to Confidence, e.g. "All functionality gated behind [new-trajectory/constructPrompt](…) (rolled out to 5%)."

# Links

For bug fixes or to motivate a feature: Grafana, Braintrust trace or similar links that show the bug happening.

# Low level

For a reader who knows the codebase but not this area. Open directly with a prose paragraph (1-4 sentences) of the background needed to evaluate the change. Code identifiers are welcome here. Then:

## Important behavioral changes

0-3 bullets. Behavior changes a reviewer might object to.

## Changes

3-6 bullets, one concrete change each. Keep them scannable.

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
- Keep it short. If a bullet doesn't add information, cut it.
- No emojis. No "Co-Authored-By" footers in the body. No Slack/Cursor/external-tool links.
- Skip CI-self-evident items (lint passes, typecheck passes, "CI green").
