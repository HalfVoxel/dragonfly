pub const CODE_COMMENTS_GUIDE: &str = include_str!("../code-comments.md");

pub const DRAGONFLY_SKILL: &str = r#"# Dragonfly

Push the current branch, monitor CI, and fix relevant issues that arise — including review bot comments.

When thinking, always start by listing which phase you are on.
Like: "Phase 4.1 Identify the failure".
But no need to show this to the user.

CODE_COMMENTS_PLACEHOLDER

## Phase 1: Safe Push

### Determine push strategy

First, understand the current state:

```bash
# Fetch latest remote state
git fetch
# Get current branch and remote tracking info
git status -b --porcelain=v2
```

The above will display:

```
# branch.oid <commit> | (initial)    Current commit.
# branch.head <branch> | (detached)  Current branch.
# branch.upstream <upstream-branch>  If upstream is set.
# branch.ab +<ahead> -<behind>       If upstream is set and the commit is present.
```

**Normal push**: If the branch tracks a remote and is ahead (fast-forward), do `git push`.

**Force push (rebase case)**: Only use `git push --force-with-lease` when ALL of these are true:
1. The user passed `--force` OR you detect a diverged state (local and remote have diverged)
2. The local branch contains all the *content* from the remote (i.e., it was a rebase, not a reset that dropped commits). Verify by checking that `git log @{upstream} --not HEAD` shows no commits whose changes are missing from HEAD — compare with `git diff @{upstream}...HEAD` or check that the remote's patches are all present.
3. Confirm with the user before force pushing if there's any ambiguity.

If the push fails or the situation is ambiguous, ask the user how to proceed.

## Phase 2: Check for conflicts with origin/main

Check if merging the PR would result in any conflicts. But do not actually merge.
If so, inform the user about them.

```
git merge-tree --write-tree --name-only HEAD origin/main
```

Inspect the merge conflicts and see if they seem like they'd be easy to resolve.
Also check what commit(s) caused the conflicts in main, as this can be useful context (it might have been a revert, for example).
If they are easy to resolve, offer to rebase and fix the merge conflicts.

## Phase 3: Wait for CI

After pushing, monitor CI checks:

```bash
# Auto-reconnecting watch — prints a single final summary when CI settles
dragonfly ci watch
```

For a one-shot poll instead of watching:

```bash
dragonfly ci status            # failing + pending only (exits 1 if any failing)
dragonfly ci status --all      # show every check (passed + skipped too)
```

Both subcommands print one line per check with a provider tag (`github`,
`buildkite`, `wiz`, `spacelift`, …), so you can see at a glance which system
is gating the PR. Prefer these over hand-assembling `gh pr checks | grep`.

Start the watch command in the background, and while waiting, start phases 5 and 6 to save time.
Remember to check back on the CI status if you are done with everything else.

## Phase 4: Fix CI Failures

When checks fail:

1. **Identify the failure**: Get the failed check details.
   ```bash
   dragonfly ci status
   ```

2. **Get failure logs**: Fetch logs for **every** failed check, across providers
   (GitHub Actions, Buildkite, Spacelift, Wiz, etc.) in one shot.
   ```bash
   dragonfly ci failures
   ```
   Each failed check gets its own section with extracted error lines and a link.
   Non-GHA providers fall back to the check-run output GitHub stores plus the URL,
   so you always see *something* even when full logs require provider credentials.

   Raw `gh run view <ID> --log-failed` is still available if you need the full
   GitHub Actions log for a single job.

3. **Decide if it's flaky or pre-existing on main**: don't fix unrelated regressions.
   ```bash
   dragonfly ci flaky test-go   # checks the same name on the last 20 main commits
   ```

4. **Check retry history before re-running**:
   ```bash
   dragonfly ci retries         # lists this head's GHA runs + attempt counts
   dragonfly ci rerun test-go   # rerun-failed-jobs for that named check
   ```

3. **Diagnose and fix**: Read the relevant source files, understand the failure.

4. **Only fix issues related to this PR**: See if this issue is in any way related to this PR, or if it's unrelated. DO NOT implement a fix if unrelated, but instead inform the user.
Pre-existing issues on main should not be fixed before asking the user.

5. **Fix it**: If relevant to this PR and NOT pre-existing, fix the issue. Ask the user for clarifications if any are needed.

5.1 **Run local verification** before pushing again:
   - For Go files: `lint-go` and/or `test-api`
   - For frontend files: `lint-web` and/or `test-web`
   - Format edited files: `format <files>`

5.2. **Commit the fix** with a descriptive message (e.g., `fix(scope): resolve CI lint failure`).

6. **Push and re-monitor**: Go back to Phase 2.

Repeat until CI is green or you need user input.

## Phase 5: Review Bot Comments

Inline review threads (including bot comments) and top-level PR reviews are
already in the pre-collected data — see the `review-threads`, `review-pr`, and
`pr-meta` files in the index. Read those first.

If you need to re-check after pushing a fix or suspect the snapshot is stale,
re-fetch live with the dragonfly helper — it returns the same cleaned
`<review-threads>` / `<pr-reviews>` / `# PR` sections that you already see in
the pre-collected files (thread IDs included), so you don't need to write
GraphQL by hand:

```bash
dragonfly pr comments              # current branch's PR
dragonfly pr comments --pr 12345   # explicit PR number
```

### Handling bot feedback

For each bot comment:

1. **Read the flagged code yourself** — do NOT blindly trust the bot. Read the actual source file and surrounding context.
2. **Validate the concern**: Is the bot correct? Common false positives:
   - Style suggestions that conflict with project conventions
   - "Unused" warnings for code used via reflection/DI
   - Security warnings for intentional patterns
   - Suggestions that would break existing behavior
3. **Should this have been cought by a test?** If not, could an existing test be extended to catch this? Tests are good, but use good judgement and avoid adding tests for every little thing.
4. **If the bot is correct and the fix is obvious**: Fix it, commit, and note what you fixed.
5. **If the bot is correct but the fix is non-trivial**: Explain the issue to the user and ask how they'd like to proceed.
6. **If the bot is wrong**: Skip it and briefly explain why.
7. **Offer to mark the specific bot threads as resolved**: Clearly state the titles of the threads you are resolving in bold.
    When resolving bot threads, first add a comment to each thread saying in which commit this was fixed, then resolve it:
    ```bash
    dragonfly pr thread comment --thread-id PRRT_... --body "Fixed in abc1234"
    dragonfly pr thread resolve --thread-id PRRT_...
    ```
    The thread IDs are available in the pre-collected review-threads data.

    Prefer replying on an existing thread over a top-level comment — it keeps discussion next to the code. Only post a top-level comment when no thread fits (cross-cutting summary, meta question, review-only roll-up):
    ```bash
    dragonfly pr comment --body "..."   # or --body - to read from stdin
    ```

## Phase 6: Custom review

If CI is passing and the PR is not marked as ready for review yet, run a
custom review via the bundled `review-agent` subagent. Invoke it through
the Agent tool with `subagent_type: "review-agent"`. The subagent's
initial context is populated automatically by a SubagentStart hook —
it receives a `<dragonfly-context>` block with the commits, files-
changed summary, per-file diff file paths under `/tmp/psc-diff-*.md`,
and any scored `<relevant-context>` chunks. **You do NOT need to inline
the diff or repeat the file index in the subagent prompt.** Pass only
the per-concern scope: which area to focus on, what kind of bug to
hunt for, and any context the hook can't provide.

If you think there's some risk of deployment edge cases, start a `review-agent` to evaluate this.
The system is deployed gradually, with new backends spinning up over a period of ~10 minutes,
replacing the old ones. This means old or new frontends can communicate with old and new backends
for a short time. Potentially even having data flows like
old frontend -> new backend -> old frontend -> old backend or similar.

CUSTOM_REVIEW_PLACEHOLDER

Instruct the subagents to write a numbered list of issues they find.
Ask the user which issues they want you to fix by writing the aggregated list of issues using markdown. Do not use the AskUserQuestion tool, allow the user to answer in free form text.
After fixing, present the changes to the user, and allow for them to review manually before comitting and pushing.

After changes have been approved, re-run `review-agent` to see if it finds more issues.

## Phase 7: PR description

If the PR has no substantial description, write one using:

```
dragonfly pr description "..."
```

This phase may be done in parallel with waiting on CI, if CI has passed at least once before.

If the PR has a description, validate that it still makes sense and is up to date with the latest changes.
However, you should not include fixes to the PR itself in its description. It should be about what the PR as a whole aims to do.

Before submitting a PR description, you *must* always check the latest PR description via the `gh` cli, to ensure it hasn't been updated from elsewhere.

### PR description guide

Pick a sensible subset of sections per PR. A tiny fix may only need `# Summary` + `# Why`, while a CLI feature should usually have all four including `# Example`.
This guide complements the existing repo rule in `AGENTS.md`, but overrides it when there are conflicts.

#### Sections (use `#` headings)

Pick the ones that fit the change; skip ones that don't. Order them as listed below.

- **`# Summary`** — 1-3 bullets. What changed and the headline why.
- **`# Important behavioral changes`** — 0-3 bullets. If this PR is a refactor and it introduces some important behavioral change that reviewers may object to. Mention it here.
- **`# Problem`** — 2-4 bullets. Motivation, prior behavior, what hurt without this.
- **`# Example`** (or `# Examples`) — only when appropriate (CLI updates, new APIs, config changes that benefit from showing usage). Contains a fenced code block.
- **`# What changed`** — 3-6 bullets. Concrete changes. One sentence per bullet.
- **`# Who`** — If the features are behind a feature flag, list it here. E.g. "* All functionality gated behind the new-trajectory/constructPrompt feature flag (rolled out to 5%)".
- **`# Links`** — For bugfixes, it's useful to include grafana urls / braintrust trace links or similar that show the bug happening.

#### PR examples

You can find 3 PR description examples at:

- ~/cloud/Programming/dragonfly/pr-descriptions/bug-fix.md
- ~/cloud/Programming/dragonfly/pr-descriptions/cli-feature.md
- ~/cloud/Programming/dragonfly/pr-descriptions/refactor.md

#### Graphs (encouraged)

Including a relevant rendered graph in the PR description is **encouraged**, especially when the change:

- Fixes a bug — show the panel that captures the bug (error rate, latency spike, panic count) so reviewers can see the problem.
- Improves a metric or touches a hot path — show the baseline panel for the affected endpoint / tool / node.

A reference for the production Grafana setup, the catalog of dashboards, and how to convert any panel URL into a `/render/d-solo/...` PNG lives at:

```
GRAFANA_DASHBOARDS_PATH
```

Read that file when you need to render a panel. The `GRAFANA_TOKEN` and `GRAFANA_HOST` env vars are already exported in this environment, so you can `curl` the render endpoint directly — no setup required. Save the PNG under `/tmp/`.

To embed the PNG in a PR body or comment, upload it with the `gh image` extension and use the markdown reference it prints:

```bash
gh image /tmp/panel.png
# -> ![panel.png](https://github.com/user-attachments/assets/...)
```

It pulls the session token from browser cookies automatically — no setup. Then update the PR with `gh pr edit <number> --body-file <new-body.md>`. Do **not** put the raw `/render/d-solo/...` URL in the PR body — it requires the service-account token and won't render for anyone else.

#### Hard rules

- **No "Test plan" / "Test checklist" / "Testing" section.** Reviewers don't need it; CI runs the tests.
- Bullets are one sentence each, no trailing prose underneath.
- Keep it short. If a bullet doesn't add information, cut it.
- No emojis. No "Co-Authored-By" footers in the body. No Slack/Cursor/external-tool links.
- Skip CI-self-evident items (lint passes, typecheck passes, "CI green").

## Phase 8: Final Status

Report a summary:
- **CI**: Final status of all checks
- **Fixes applied**: List of commits made to fix CI or bot issues. Remember that CI issues unrelated to this PR should not be fixed, but the user should be informed.
- **Remaining items**: Any unresolved bot comments or issues needing user input
- **PR URL**: Link to the PR

If everything is green and clean, say so concisely.

## Phase 9: Ready for review

If everything is green and clean, offer to make mark the PR as ready for review, if it isn't already.
If the user approves, investigate who would be the most reasonable person to review it. Look at who is the code owner of the files, and who has been the primary author as of late. Use git blame around important changes. Make sure to discard mechanical changes like formatting and linting fixes.

Suggest one recommendation, and possibly 1 or 2 alternatives.
If the user approves, add the user's choice of reviewer and only then mark the PR as ready.

If the PR contains mostly completely new code, search for conceptually related PRs by the user, and see who reviewed them.

The who-is-everyone tool can be useful to look up names.

```
gh pr edit <number> --add-reviewer <username>
gh pr ready <number>
```

If the PR was already marked as ready for review, check if it has a reviewer or not.

## Unrelated failing CI tests

If CI is failing due to a seemingly unrelated issue, it could be that:
* It has already been fixed on main, but we don't have it in this branch => rebase.
* The test is failing on main, and doesn't look flaky => existing issue, should not be fixed, but user shuld be informed.
* The test is verified flaky => re-run test, unless it's already been retried.
* The test failure is actually caused by this PR, but in a roundabout way => investigate if this seems to be the case.

### Checking tests on main

To check the test status on main:

```
dragonfly ci flaky <check-name>           # default: last 20 commits
dragonfly ci flaky test-go --limit 50
```

This prints pass/fail/skip counts for the named check on the last N commits of
`origin/main` and a verdict (consistently failing → pre-existing; mixed →
flaky; clean → likely caused by this PR).

* A verified flaky is sometimes passing and sometimes failing (check the latest hour or so).
* Not all tests run for all PRs and commits. Always verify that the test was not skipped (and thus looks like a success).
* Many CI jobs (e.g. test-go) contain many individual tests. Inspect relevant runs on main to see if the same exact test is passing/failing.
* The test might have been recently fixed on main. If recent commits are passing on main, then check if there are relevant commits that we don't have in the PR branch. Check merge conflicts and offer to rebase if there are none, or they seem simple enough to fix.
* If the test is passing on main, rebasing is always a good idea if possible.

### Check retries for CI

```
dragonfly ci retries
```

Lists the GitHub Actions runs for the current head SHA with attempt counts so
you can see "already retried, don't re-run". Before re-running a test, always
check if it has already been run more than once for the latest commit on the
current PR. If so, ask the user before scheduling it yet another time.

To rerun the failed jobs of a named check:

```
dragonfly ci rerun <check-name>
```

This resolves the name to the right workflow run and calls `gh run rerun
<id> --failed` for you. Note: Buildkite/Wiz/Spacelift checks cannot be rerun
through this command — visit the provider's URL.

After re-running, wait until the CI completes and check the results.

### Checking for changes against main

```
# Check what commits main have, that aren't on this branch
git log HEAD..origin/main --oneline

# Check what changes this PR did to a file
git log origin/main..HEAD -p -- path/to/file.go

# Check when text was added/removed on main
git log origin/main -S 'MySearchText' --format='%h %ad %s' --date=short --since='1 month ago'

# Check when text was added/removed on any branch
git log --all -S 'MySearchText' --format='%h %ad %s' --date=short -- path/to/optional/file
```

## Pre-existing issues

If CI fails due to, or bots report on, pre-existing bugs in the code (before this PR), then they should never be fixed without asking the user first.
Try rebasing on origin/main as this may fix the issue.

## Submitting feedback about this review process

While working through the phases, watch for friction that future runs could avoid:

- Something took an unnecessary amount of time to get right (e.g. you fumbled the same command twice, hunted for data that should have been pre-collected, or worked around a limitation of this prompt).
- A helper command or subcommand is missing that would have saved you a round-trip (e.g. a shortcut for a gh/git incantation you had to assemble by hand).
- A phase instruction is ambiguous or sent you down the wrong path.

When you notice one of these, submit a brief, concrete note via CLI:

```bash
dragonfly --feedback "Spent 3 round-trips resolving review threads because thread IDs weren't in the pre-collected data for outdated threads. A `dragonfly pr thread list` subcommand would help."
```

Keep each entry short and specific — describe the friction and (if obvious) what would fix it. Do NOT use this for PR-level issues or user-facing status; it's only for meta-feedback about your process.

"#;
