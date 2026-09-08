#!/usr/bin/env python3
"""Git guard hook for Claude Code PreToolUse and PermissionRequest.

Reads tool input from stdin (JSON with .tool_input.command),
checks the command against git rules, and outputs a permission decision.

Registered on both events because PreToolUse fires after the permission
dialog: a deny there makes the user approve the command only to watch the
hook reject it. PermissionRequest fires while the dialog is still pending,
so a deny cancels the prompt before the user sees it. PreToolUse stays
registered to catch commands that are allowlisted (e.g. Bash(git:*)) and
therefore never raise a dialog.

Set GIT_GUARD=off to disable. Required for headless runs (`claude -p`): with no
human to answer, an "ask" decision is a hard block, so goalie-triage agents
could not commit at all.
"""

import json
import os
import re
import shlex
import subprocess
import sys

# Prefixing a command with this token acknowledges the hard-reset warning.
HARD_RESET_OVERRIDE = "GIT_GUARD_HARD_RESET"

RULES = [
    # (pattern, exclude_pattern, decision, reason)
    (r"\bgit\s+add\b[^|&;]*?(\s--all\b|\s-[a-z]*A[a-z]*\b)", None, "deny",
     "git add -A / --all is not allowed. Stage files explicitly by path (e.g. `git add path/to/file`) so unintended changes are never committed."),
    # `--abort/--continue/--quit` clean up an in-progress merge rather than create
    # one; `--ff-only` moves a branch pointer without a merge commit (stack
    # bookkeeping) and refuses otherwise; `git merge-base`/`merge-file` don't
    # match (no space after `merge`).
    (r"\bgit\s+merge\s+", r"\bgit\s+merge\s+([^\s;|&]+\s+)*--(abort|continue|quit|ff-only)\b", "deny",
     "git merge is not allowed. Rebase instead: `git fetch origin && git rebase origin/main`."),
    # (r"\bgit\s+commit\b", None, "ask", "git commit requires confirmation"),
    # (r"\bgit\s+(rebase|reset)\b", r"\bgit\s+rebase\s+--continue\b", "ask", "git rebase/reset requires confirmation"),
    # --hard only: mixed resets are routine and must stay frictionless. A
    # model-facing "verify" speed bump, never a user dialog.
    (r"\bgit\s+reset\b[^|&;]*\s--hard\b", rf"\b{HARD_RESET_OVERRIDE}=1\b", "verify",
     "git reset --hard discards ALL uncommitted changes to tracked files, not just the commit being reset. "
     "Check `git status` for unrelated dirty files this would destroy (stash them, or prefer a mixed reset), "
     f"then re-run this exact command prefixed with {HARD_RESET_OVERRIDE}=1 to proceed."),
]


SEGMENT_SPLIT = re.compile(r"\s*(?:;|&&|\|\||\||\n)\s*")

# Branch-creating and conflict-side checkouts never touch a dirty file.
CHECKOUT_SAFE_FLAGS = {"-b", "-B", "--orphan", "--ours", "--theirs", "--detach"}
# Flags that consume the next token, so it must not be mistaken for a path.
CHECKOUT_ARG_FLAGS = {"-b", "-B", "--orphan", "--conflict", "--pathspec-from-file"}


def checkout_dirty_paths(segment: str, cwd: str) -> list[str] | None:
    """Return the paths a `git checkout` segment would overwrite that carry
    uncommitted changes, or None when the segment is not a path checkout.

    Returns [] for a path checkout whose targets are clean. A path that cannot
    be resolved statically (shell variable, backtick) is reported as dirty:
    the guard cannot prove it safe.
    """
    try:
        tokens = shlex.split(segment)
    except ValueError:
        return None
    # Wrappers (`do`, `direnv exec .`, `FOO=1`) may precede the git invocation.
    starts = [i for i in range(len(tokens) - 1) if tokens[i] == "git" and tokens[i + 1] == "checkout"]
    if not starts:
        return None
    args = tokens[starts[0] + 2:]
    if CHECKOUT_SAFE_FLAGS & set(args):
        return None
    if "--" in args:
        paths = args[args.index("--") + 1:]
        refs = [a for a in args[: args.index("--")] if not a.startswith("-")]
        has_ref = bool(refs)
    else:
        positional = []
        skip = False
        for a in args:
            if skip:
                skip = False
                continue
            if a in CHECKOUT_ARG_FLAGS:
                skip = True
            elif a.startswith("-"):
                continue
            else:
                positional.append(a)
        # Without `--`, git treats a lone argument that names a ref as a
        # branch switch; anything that exists on disk (or a list) is a pathspec.
        if not positional:
            return None
        first = positional[0]
        if len(positional) == 1 and not os.path.exists(os.path.join(cwd, first)):
            return None
        if os.path.exists(os.path.join(cwd, first)):
            has_ref, paths = False, positional
        else:
            has_ref, paths = True, positional[1:]
    if not paths:
        return None
    if any(re.search(r"[$`]", p) for p in paths):
        return paths
    try:
        out = subprocess.run(
            ["git", "-C", cwd, "status", "--porcelain", "--untracked-files=no", "--", *paths],
            capture_output=True, text=True, timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return paths
    if out.returncode != 0:
        return []
    dirty = []
    for line in out.stdout.splitlines():
        if len(line) < 4:
            continue
        index_state, worktree_state = line[0], line[1]
        # `git checkout -- <path>` restores from the index, so only unstaged
        # edits are lost; `git checkout <ref> -- <path>` rewrites the index too.
        if worktree_state != " " or (has_ref and index_state != " "):
            dirty.append(line[3:])
    return dirty


def track_cd(segment: str, cwd: str) -> str:
    try:
        tokens = shlex.split(segment)
    except ValueError:
        return cwd
    if len(tokens) == 2 and tokens[0] == "cd":
        return os.path.normpath(os.path.join(cwd, os.path.expanduser(tokens[1])))
    return cwd


def check_command(cmd: str, cwd: str) -> tuple[str, str] | None:
    # Per segment, so an exclude flag on one command (`git merge --abort`)
    # cannot whitewash a blocked sibling in the same line.
    for segment in SEGMENT_SPLIT.split(cmd):
        cwd = track_cd(segment, cwd)
        for pattern, exclude, decision, reason in RULES:
            if re.search(pattern, segment):
                if exclude and re.search(exclude, segment):
                    continue
                return decision, reason
        dirty = checkout_dirty_paths(segment, cwd)
        if dirty:
            return "ask", (
                "git checkout would discard uncommitted changes in: "
                + ", ".join(dirty)
            )
    return None


def build_output(event: str, decision: str, reason: str) -> dict | None:
    if event == "PermissionRequest":
        # "ask" has no PermissionRequest form: staying silent lets the pending
        # dialog show, which is the same outcome. "verify" stays silent too -
        # it is model-facing feedback and must never surface in a user dialog.
        if decision != "deny":
            return None
        return {
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny", "message": reason},
            }
        }
    # "verify" is a deny on the wire; the reason carries the override recipe,
    # so the model can retry immediately without a human in the loop.
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny" if decision == "verify" else decision,
            "permissionDecisionReason": reason,
        }
    }


def main():
    if os.environ.get("GIT_GUARD") == "off":
        return
    data = json.load(sys.stdin)
    cmd = data.get("tool_input", {}).get("command", "")
    result = check_command(cmd, data.get("cwd") or os.getcwd())
    if result:
        output = build_output(data.get("hook_event_name", "PreToolUse"), *result)
        if output:
            json.dump(output, sys.stdout)


if __name__ == "__main__":
    main()
