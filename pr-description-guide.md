# PR description guide

Pick a sensible subset of sections per PR. A tiny fix may only need `# Summary` + `# Why`, while a CLI feature should usually have all four including `# Example`.
This guide complements the existing repo rule in `AGENTS.md`, but overrides it when there are conflicts.

## Sections (use `#` headings)

Pick the ones that fit the change; skip ones that don't. Order them as listed below.

- **`# Summary`** — 1-3 bullets. What changed and the headline why.
- **`# Important behavioral changes`** — 0-3 bullets. If this PR is a refactor and it introduces some important behavioral change that reviewers may object to. Mention it here.
- **`# Problem`** — 2-4 bullets. Motivation, prior behavior, what hurt without this.
- **`# Example`** (or `# Examples`) — only when appropriate (CLI updates, new APIs, config changes that benefit from showing usage). Contains a fenced code block.
- **`# What changed`** — 3-6 bullets. Concrete changes. One sentence per bullet.
- **`# Who`** — If the features are behind a feature flag, list it here. E.g. "* All functionality gated behind the new-trajectory/constructPrompt feature flag (rolled out to 5%)". Make the feature flag name a link to confidence.
- **`# Links`** — For bugfixes, it's useful to include grafana urls / braintrust trace links or similar that show the bug happening.

## PR examples

You can find 3 PR description examples at:

- __DRAGONFLY_ROOT__/pr-descriptions/bug-fix.md
- __DRAGONFLY_ROOT__/pr-descriptions/cli-feature.md
- __DRAGONFLY_ROOT__/pr-descriptions/refactor.md

## Graphs (encouraged)

Including a relevant rendered graph in the PR description is **encouraged**, especially when the change:

- Fixes a bug — show the panel that captures the bug (error rate, latency spike, panic count) so reviewers can see the problem.
- Improves a metric or touches a hot path — show the baseline panel for the affected endpoint / tool / node.

A reference for the production Grafana setup, the catalog of dashboards, and how to convert any panel URL into a `/render/d-solo/...` PNG lives at __DRAGONFLY_ROOT__/grafana_dashboards.md.

Read that file when you need to render a panel. The `GRAFANA_TOKEN` and `GRAFANA_HOST` env vars are already exported in this environment, so you can `curl` the render endpoint directly — no setup required. Save the PNG under `/tmp/`.

To embed the PNG in a PR body or comment, upload it with the `gh image` extension and use the markdown reference it prints:

```bash
gh image /tmp/panel.png
# -> ![panel.png](https://github.com/user-attachments/assets/...)
```

It pulls the session token from browser cookies automatically — no setup. Then update the PR with `gh pr edit <number> --body-file <new-body.md>`. Do **not** put the raw `/render/d-solo/...` URL in the PR body — it requires the service-account token and won't render for anyone else.

## Hard rules

- **No "Test plan" / "Test checklist" / "Testing" section.** Reviewers don't need it; CI runs the tests.
- Bullets are one sentence each, no trailing prose underneath.
- Keep it short. If a bullet doesn't add information, cut it.
- No emojis. No "Co-Authored-By" footers in the body. No Slack/Cursor/external-tool links.
- Skip CI-self-evident items (lint passes, typecheck passes, "CI green").
