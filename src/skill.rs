pub const PUSH_AND_FIX_SKILL: &str = r#"# Push and Fix CI

Push the current branch, monitor CI, and fix relevant issues that arise — including review bot comments.

When thinking, always start by listing which phase you are on.
Like: "Phase 4.1 Identify the failure".
But no need to show this to the user.

## Arguments

- Flags: "$ARGUMENTS" (pass `--force` to allow force push after rebase confirmation)

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

**No remote yet**: Just `git push -u origin HEAD`.

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
# Wait a few seconds for checks to register, then watch for changes
sleep 10 && gh pr checks --watch --fail-fast
```

Always prefer using watch, but if you need to poll, you can so like this:

```bash
# Check status
gh pr checks
```

Start the watch command in the background, and while waiting, start phases 5 and 6 to save time.
Remember to check back on the CI status if you are done with everything else.

## Phase 4: Fix CI Failures

When checks fail:

1. **Identify the failure**: Get the failed check details.
   ```bash
   gh pr checks
   ```

2. **Get failure logs**: For each failed check, fetch the logs.
   ```bash
   # List workflow runs for the PR's head SHA
   gh run list --branch $(git branch --show-current) --limit 5

   # View specific failed run
   gh run view <RUN_ID> --log-failed
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

After CI passes (or in parallel while waiting), check for review bot comments:

```bash
# Get PR number
gh pr view --json number --jq '.number'


gh pr view <number> --json reviews

# Fetch all review threads including bot comments
gh api graphql -f query='
query($owner: String!, $repo: String!, $pr: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
          isOutdated
          path
          line
          comments(first: 50) {
            nodes {
              author { login }
              body
              createdAt
            }
          }
        }
      }
    }
  }
}' -f owner=OWNER -f repo=REPO -F pr=PR_NUMBER

# Also check review comments via REST
gh api repos/{owner}/{repo}/pulls/{pr}/comments --jq '.[] | select(.user.type == "Bot" or (.user.login | test("bot|amp|review"; "i"))) | {user: .user.login, path: .path, line: .line, body: .body}'
```

Replace OWNER, REPO, and PR_NUMBER with actual values from `gh repo view --json owner,name` and the PR number.

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
    push-and-check pr thread comment --thread-id PRRT_... --body "Fixed in abc1234"
    push-and-check pr thread resolve --thread-id PRRT_...
    ```
    The thread IDs are available in the pre-collected review-threads data.

## Phase 6: Custom review

If CI is passing and the PR is not marked as ready for review yet,
then use a subagent to review the pr using the /pr-review skill.

CUSTOM_REVIEW_PLACEHOLDER

Ask the user which issues they want you to fix.
After fixing, present the changes to the user, and allow for them to review manually before comitting and pushing.

After changes have been approved, re-run the review subagent to see if it finds more issues.

## Phase 7: Final Status

Report a summary:
- **Push**: How the push went (normal / force-with-lease)
- **CI**: Final status of all checks
- **Fixes applied**: List of commits made to fix CI or bot issues. Remember that CI issues unrelated to this PR should not be fixed, but the user should be informed.
- **Remaining items**: Any unresolved bot comments or issues needing user input
- **PR URL**: Link to the PR

If everything is green and clean, say so concisely.

## Phase 8: Ready for review

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

To check the test status on main run

```
git log origin/main --format='%H' -20 | while read sha; do
  result=$(gh api "repos/lovablelabs/lovable/commits/$sha/check-runs?per_page=100" --jq '.check_runs[] | select(.name == "test-go") | .conclusion' 2>/dev/null)
  echo "${sha:0:7} ${result:-no-run}"
done
```

* A verified flaky is sometimes passing and sometimes failing (check the latest hour or so).
* Not all tests run for all PRs and commits. Always verify that the test was not skipped (and thus looks like a success).
* Many CI jobs (e.g. test-go) contain many individual tests. Inspect relevant runs on main to see if the same exact test is passing/failing.
* The test might have been recently fixed on main. If recent commits are passing on main, then check if there are relevant commits that we don't have in the PR branch. Check merge conflicts and offer to rebase if there are none, or they seem simple enough to fix.
* If the test is passing on main, rebasing is always a good idea if possible.

### Check retries for CI
To check how many times a given CI test has been retried, use:

```
  gh run list --branch "$(git branch --show-current)" --limit 5 --json databaseId,attempt,conclusion,name --jq '.[] | select(.name == "Test") | "\(.databaseId) attempt:\(.attempt) \(.conclusion)"'
```

Before re-running a test, always check if it has already been run more than once for the latest commit on the current PR. If so, ask the user before scheduling it yet another time.

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
"#;
