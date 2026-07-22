#!/usr/bin/env python3
"""CI guard hook for Claude Code PreToolUse and PermissionRequest.

Intercepts manual `gh pr checks` / `gh run view --log-failed` /
`gh run list --status failure` patterns and redirects the agent to the
`dragonfly ci` subcommands that already replace them.

Reads tool input from stdin (JSON with .tool_input.command), outputs a
permission decision when a rule matches.

Registered on both events because PreToolUse fires after the permission
dialog: a deny there makes the user approve the command only to watch the
hook reject it. PermissionRequest fires while the dialog is still pending,
so a deny cancels the prompt before the user sees it. PreToolUse stays
registered to catch commands that are allowlisted (e.g. Bash(gh:*)) and
therefore never raise a dialog.
"""

import json
import re
import sys


# Each rule: (regex over the raw command, deny message).
# Order matters — first match wins.
RULES = [
    # 1) `gh pr checks --watch ...` (any form) — replaced by ci watch.
    (
        r"\bgh\s+pr\s+checks\b.*--watch",
        "Use `dragonfly ci watch` instead of `gh pr checks --watch --fail-fast`.\n"
        "It auto-reconnects on dropped connections and prints a single final\n"
        "summary (no need for `until ...; sleep 5; done` retry loops).",
    ),

    # 2) `gh pr checks` piped to grep/awk/sed/head/tail — that's exactly what
    #    `ci status` produces in bounded, deduped, provider-tagged form.
    (
        r"\bgh\s+pr\s+checks\b[^|]*\|\s*(grep|awk|sed|head|tail|cut)\b",
        "Use `dragonfly ci status` instead of `gh pr checks | grep/awk/head/...`.\n"
        "Default output hides passed+skipped, exits 1 on any failure, and tags each\n"
        "row by provider (github / buildkite / wiz / spacelift). Pass `--all` to see\n"
        "passed and skipped too, `--pr <N>` for a different PR.",
    ),

    # 3) `gh run list --branch ... --status failure` — the exact pattern that
    #    produced empty failure files (Buildkite/Wiz/Spacelift not in run list).
    (
        r"\bgh\s+run\s+list\b.*--status\s+failure",
        "Use `dragonfly ci failures` instead of `gh run list --status failure`.\n"
        "`gh run list` only returns GitHub Actions workflow runs — Buildkite, Wiz,\n"
        "Spacelift etc. are missing, which is what caused the empty-failure-file bug.\n"
        "`ci failures` sources from `gh pr checks --json` so every provider is covered.",
    ),

    # 4) Hand-rolled flakiness loop:
    #    `git log origin/main ... | while read sha; do gh api .../check-runs ...`
    (
        r"git\s+log\s+origin/main[^\n]*\bgh\s+api\b.*check-runs",
        "Use `dragonfly ci flaky <check-name>` instead of hand-rolling a\n"
        "`for sha in $(git log origin/main); do gh api .../check-runs done` loop.\n"
        "Default window is 20 commits (`--limit N` to widen), and it prints a verdict\n"
        "(consistently-passing / consistently-failing / flaky / no-data).",
    ),

    # 5) `gh run list ... --json ... attempt ...` — the retries-inspection pattern.
    (
        r"\bgh\s+run\s+list\b.*--json\b[^']*\battempt\b",
        "Use `dragonfly ci retries` instead of `gh run list --json ...attempt...`.\n"
        "It filters to the PR's head SHA, sorts retried runs to the top with a\n"
        "`← retried` marker, and prints aligned columns (NAME / ATT / RESULT / TIME / RUN_ID).",
    ),

    # 6) `gh run rerun <id> --failed` — let the agent use `ci rerun <name>` so it
    #    doesn't have to look up the run ID first. Use `ask` here (not deny) —
    #    sometimes the agent already has the right run ID and bypassing is fine.
    (
        r"\bgh\s+run\s+rerun\b.*--failed",
        "ASK_RERUN",
    ),

    # 7) Reading review comments / threads via raw gh.
    (
        r"\bgh\s+api\b[^\n]*\b(?:pulls?/\d+/comments|reviewThreads)\b",
        "Use `dragonfly pr comments` to read review threads + top-level reviews.\n"
        "It returns the cleaned `<review-threads>` / `<pr-reviews>` / `# PR` sections\n"
        "(including stable thread IDs), and matches the format of the pre-collected\n"
        "data files — no GraphQL by hand. Pass `--pr <N>` for a different PR.",
    ),

    # 8) `gh pr view ... --json reviews/comments/reviewThreads` — same content,
    #    same wrapper. Allow `gh pr view` without those JSON fields (metadata reads).
    (
        r"\bgh\s+pr\s+view\b[^|]*--json\s+[A-Za-z,]*(?:reviews|reviewThreads|comments)\b",
        "Use `dragonfly pr comments` to read PR reviews / review threads.\n"
        "It returns the cleaned format with thread IDs included — the same shape as\n"
        "the pre-collected review-threads / review-pr files.\n"
        "`gh pr view --json` is fine for non-review metadata (title, headRefName,\n"
        "isDraft, statusCheckRollup, …).",
    ),

    # 9) GraphQL mutation to post a review-thread reply.
    (
        r"\bgh\s+api\s+graphql\b[^\n]*addPullRequestReviewComment",
        "Use `dragonfly pr thread comment --thread-id <PRRT_…> --body \"…\"`\n"
        "instead of hand-rolling the GraphQL `addPullRequestReviewComment` mutation.\n"
        "Thread IDs are in the pre-collected `review-threads` data (or `dragonfly pr comments`).",
    ),

    # 10) GraphQL mutation to resolve a review thread.
    (
        r"\bgh\s+api\s+graphql\b[^\n]*resolveReviewThread",
        "Use `dragonfly pr thread resolve --thread-id <PRRT_…>` instead of\n"
        "the GraphQL `resolveReviewThread` mutation. Always post a fix-commit reply\n"
        "(`pr thread comment`) before resolving, so the thread has context.",
    ),

    # 11) `gh pr edit ... --body / --body-file` — replaced by `pr description`.
    #     Other `gh pr edit` flags (--add-reviewer, --add-label, …) are unaffected.
    (
        r"\bgh\s+pr\s+edit\b[^|]*--body(?:-file)?\b",
        "Use `dragonfly pr description \"<body>\"` (or `pr description -` to read\n"
        "from stdin) instead of `gh pr edit --body / --body-file`.\n"
        "It always re-fetches the latest PR body first (so you don't clobber an out-of-band\n"
        "edit) and matches the description guide in the skill prompt.\n"
        "Other `gh pr edit` flags like `--add-reviewer` / `--add-label` are still fine.",
    ),

    # 12) `gh pr comment ... --body / --body-file` — replaced by `pr comment`.
    (
        r"\bgh\s+pr\s+comment\b[^|]*--body(?:-file)?\b",
        "Use `dragonfly pr comment --body \"<body>\"` (or `--body -` to read from\n"
        "stdin) instead of `gh pr comment --body / --body-file`.\n"
        "It defaults to the current branch's PR (`--pr <N>` for an explicit number)\n"
        "and signs the comment with a Dragonfly footer so reviewers can tell\n"
        "agent-authored comments from human ones.\n"
        "For a review-thread reply, use `pr thread comment` instead.",
    ),
]


ASK_RERUN_REASON = (
    "Prefer `dragonfly ci rerun <check-name>` — it resolves the\n"
    "check name to the run ID for you and refuses to act on non-GHA\n"
    "providers (Buildkite/Wiz cannot be rerun from the GH API).\n"
    "If you already have the run ID and know what you're doing,\n"
    "approve to proceed."
)


def check_command(cmd: str) -> tuple[str, str] | None:
    for pattern, msg in RULES:
        if not re.search(pattern, cmd):
            continue
        if msg == "ASK_RERUN":
            return "ask", ASK_RERUN_REASON
        return "deny", msg
    return None


def build_output(event: str, decision: str, reason: str) -> dict | None:
    if event == "PermissionRequest":
        # "ask" has no PermissionRequest form: staying silent lets the pending
        # dialog show, which is the same outcome.
        if decision != "deny":
            return None
        return {
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny", "message": reason},
            }
        }
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        return
    cmd = data.get("tool_input", {}).get("command", "")
    if not cmd:
        return
    result = check_command(cmd)
    if result:
        output = build_output(data.get("hook_event_name", "PreToolUse"), *result)
        if output:
            json.dump(output, sys.stdout)


if __name__ == "__main__":
    main()
