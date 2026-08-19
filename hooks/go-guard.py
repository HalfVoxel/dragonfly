#!/usr/bin/env python3
"""Go build guard hook for Claude Code PreToolUse and PermissionRequest.

Denies local `go build` / `go test` / `go vet` and the repo wrappers
(`test-api`, `lint-go`, `golangci-lint`) inside a lovable checkout, and points
the agent at `rgo`, which runs them on the dev-aron build server instead.

Escape hatches, for when the work genuinely has to be local (cgo, darwin/arm64
behavior, a sandbox the server cannot reach):

    LOCAL_BUILD=true test-api ./pkg/...   # env prefix, passed through untouched
    test-api --local ./pkg/...            # marker flag, stripped before running

Reads the tool call as JSON on stdin. Registered on both events because
PreToolUse fires after the permission dialog: a deny there makes the user
approve a command only to watch the hook reject it. PermissionRequest fires
while the dialog is pending, so a deny cancels the prompt first. PreToolUse
stays registered to catch allowlisted commands that never raise a dialog.
"""

import json
import re
import subprocess
import sys

MARKER_FLAG = "--local"
ENV_ESCAPE = re.compile(r"\bLOCAL_BUILD=(true|1)\b")

# A guarded command must start a command position: line start, or after a
# separator, or after env assignments. Without that anchor "rgo test" matches
# via its trailing "go test".
CMD_START = r"(?:^|[\n;&|(]|\&\&|\|\|)\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*"

RULES = [
    (
        re.compile(CMD_START + r"go\s+(?:build|test|vet)\b"),
        "Run Go builds and tests on the build server: `rgo vet ./go/api/...`,\n"
        "`rgo compile ./go/api/...` (fastest compile check, no linking), or\n"
        "`rgo test ./pkg/...` (paths relative to go/api).",
    ),
    (
        re.compile(CMD_START + r"test-api\b"),
        "Use `rgo test <pkgs>` instead of `test-api <pkgs>` — same wrapper, same\n"
        "paths (relative to go/api), run on dev-aron.",
    ),
    (
        re.compile(CMD_START + r"(?:lint-go|golangci-lint|staticcheck)\b"),
        "Use `rgo lint` (lints this branch's changed packages) or\n"
        "`rgo golangci <pkgs>` instead of running the linter locally.",
    ),
]

WHY = (
    "\nWhy: this laptop routinely sits at load 50+ from concurrent agents, so a local\n"
    "run starves everything else; dev-aron is idle with 16 threads and warm caches.\n"
    "If the work truly must be local (cgo, darwin/arm64 behavior, a local sandbox),\n"
    "prefix `LOCAL_BUILD=true` or pass `--local` and the guard steps aside."
)


QUOTED = re.compile(r"""'[^']*'|"[^"]*\"""", re.S)


def without_quoted(command: str) -> str:
    """Blank out quoted spans so their contents cannot look like a command.

    Without this, `grep -E "go list|go build"` denies itself: the `|` inside the
    regex reads as a shell separator followed by a guarded command.
    """
    return QUOTED.sub(" ", command)


def in_lovable_checkout(cwd: str) -> bool:
    """True when cwd is inside the lovable monorepo, where rgo applies."""
    try:
        root = subprocess.run(
            ["git", "-C", cwd, "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, timeout=5,
        )
        if root.returncode != 0:
            return False
        remote = subprocess.run(
            ["git", "-C", root.stdout.strip(), "remote", "get-url", "origin"],
            capture_output=True, text=True, timeout=5,
        )
        return "lovablelabs/lovable" in remote.stdout
    except (OSError, subprocess.SubprocessError):
        return False


def emit(event: str, decision: str, reason: str, updated_input=None) -> None:
    out = {
        "hookSpecificOutput": {
            "hookEventName": event,
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }
    if updated_input is not None:
        # updatedInput REPLACES tool_input rather than merging into it, so the
        # caller passes the whole object back or fields like description vanish.
        out["hookSpecificOutput"]["updatedInput"] = updated_input
    print(json.dumps(out))
    sys.exit(0)


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        sys.exit(0)

    event = payload.get("hook_event_name") or "PreToolUse"
    tool_input = payload.get("tool_input") or {}
    command = tool_input.get("command") or ""
    if not command:
        sys.exit(0)

    scannable = without_quoted(command)
    matched = next((msg for pattern, msg in RULES if pattern.search(scannable)), None)
    if not matched:
        sys.exit(0)

    # An rgo invocation runs the guarded command on the server already; its own
    # arguments must not be re-matched.
    if re.match(CMD_START + r"rgo\b", scannable) or " rgo " in f" {scannable} ":
        sys.exit(0)

    if ENV_ESCAPE.search(command):
        sys.exit(0)

    if MARKER_FLAG in command.split():
        # Strip the marker: it is a signal to this hook, not a flag `go` accepts.
        stripped = " ".join(w for w in command.split() if w != MARKER_FLAG)
        if event != "PreToolUse":
            sys.exit(0)
        emit(event, "allow", f"{MARKER_FLAG} given: running locally as requested.",
             {**tool_input, "command": stripped})

    if not in_lovable_checkout(payload.get("cwd") or "."):
        sys.exit(0)

    emit(event, "deny", matched + WHY)


if __name__ == "__main__":
    main()
