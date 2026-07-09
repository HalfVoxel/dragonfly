#!/usr/bin/env bash
#
# Compare comment-reviewer prompt variants against each other on the current
# PR. Runs every comment-reviewer-* agent plus the general review-agent
# (focused on comments) over the SAME pre-built <dragonfly-context>, then runs
# a final pass that compares the outputs.
#
# Each variant runs as a headless `claude -p` session whose system prompt is
# the agent's markdown body (with any `@<path>` imports expanded inline) and
# whose user message is the shared context.
# This is a fidelity approximation of the production subagent path: same
# context, same per-agent prompt, fair across variants. It does NOT exercise
# the SubagentStart hook — the context is built once here and reused, instead
# of being injected per-subagent.
#
# Two distinct repos are involved:
#   - DF_REPO: this tooling repo (dragonfly) — holds agents/, code-comments.md,
#     and the dragonfly binary. Resolved from the script's own location.
#   - TARGET:  the PR repo to review — the git repo you invoke this from.
#
# Usage (run from inside the PR's checkout):
#   ~/cloud/Programming/dragonfly/scripts/compare-comment-reviewers.sh
#   DF_MODEL=sonnet  ...compare-comment-reviewers.sh
#   DF_SKIP_BUILD=1  ...compare-comment-reviewers.sh
#
# Output: <dragonfly>/target/comment-reviewer-eval/<timestamp>/ with one file
# per variant and the shared context. The final comparison pass execs into an
# interactive claude session (streamed live, not saved to a file).

set -euo pipefail

log() { printf '\033[36m[compare]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31m[compare] error:\033[0m %s\n' "$*" >&2; exit 1; }

# ── Resolve the two roots ───────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DF_REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET="$(git rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$TARGET" ] || die "not inside a git repo — cd into the PR's checkout first"

AGENTS_DIR="$DF_REPO/agents"
COMMENT_GUIDELINES="$DF_REPO/code-comments.md"
MODEL="${DF_MODEL:-opus}"
# Read-only tool set: variants read source and the /tmp per-file diffs, but
# never mutate the tree. /tmp (where dragonfly writes the diff files) and an
# optional ~/.dragonfly glossary are added so Read can reach outside TARGET.
ALLOWED_TOOLS=(Read Grep Glob)
ADD_DIRS=(/tmp)
[ -d "$HOME/.dragonfly" ] && ADD_DIRS+=("$HOME/.dragonfly")

# Variants to compare. review-agent is the general reviewer steered onto
# comments via its user message; comment-reviewer-4 is the control that runs
# WITHOUT the precomputed context; the rest are dedicated comment reviewers.
VARIANTS=(comment-reviewer comment-reviewer-2 comment-reviewer-3 comment-reviewer-4 review-agent)

ts="$(date +%Y%m%d-%H%M%S)"
OUTDIR="$DF_REPO/target/comment-reviewer-eval/$ts"
mkdir -p "$OUTDIR"

command -v claude >/dev/null || die "\`claude\` not on PATH"
[ -f "$COMMENT_GUIDELINES" ] || die "missing $COMMENT_GUIDELINES"

log "tooling repo: $DF_REPO"
log "target PR repo: $TARGET ($(git -C "$TARGET" branch --show-current))"

# ── Resolve the dragonfly binary ────────────────────────────────────────────
# Build the debug binary unless told to skip; it bundles the context builder.
if [ -z "${DF_SKIP_BUILD:-}" ]; then
  log "building dragonfly (cargo build)..."
  cargo build --manifest-path "$DF_REPO/Cargo.toml" >&2
fi
DRAGONFLY="$DF_REPO/target/debug/dragonfly"
[ -x "$DRAGONFLY" ] || DRAGONFLY="$(command -v dragonfly || true)"
[ -n "$DRAGONFLY" ] && [ -x "$DRAGONFLY" ] || die "dragonfly binary not found (build it or set PATH)"

# ── Build the shared <dragonfly-context>, in the TARGET repo ────────────────
# Two flavors:
#  - CONTEXT:        per-file diffs as /tmp references (variants 2/3, review-agent).
#  - CONTEXT_INLINE: full diffs inlined as <diff name=".."> blocks. Only
#    comment-reviewer uses this, so the comparison also measures inlined-vs-
#    referenced diffs. Must run inside TARGET — that is the cwd dragonfly
#    diffs and keys its cache by.
CONTEXT="$OUTDIR/context.md"
CONTEXT_INLINE="$OUTDIR/context-inline.md"
log "building review context (referenced + inlined)..."
( cd "$TARGET" && "$DRAGONFLY" prompt review-agent ) \
  >"$CONTEXT" 2>"$OUTDIR/context.stderr.log" \
  || die "dragonfly prompt review-agent failed (see $OUTDIR/context.stderr.log)"
( cd "$TARGET" && "$DRAGONFLY" prompt review-agent --inline-diffs ) \
  >"$CONTEXT_INLINE" 2>"$OUTDIR/context-inline.stderr.log" \
  || die "dragonfly prompt review-agent --inline-diffs failed (see $OUTDIR/context-inline.stderr.log)"
if grep -q "no per-file diffs against base" "$CONTEXT"; then
  die "no diff against base in $TARGET — checkout the PR branch there first"
fi
log "context: $(wc -l <"$CONTEXT") lines referenced, $(wc -l <"$CONTEXT_INLINE") lines inlined"

# Format a variant's captured cost (USD) for display, or "$?" if unknown.
fmt_cost() {
  local c; c="$(cat "$OUTDIR/$1.cost" 2>/dev/null || true)"
  if [ -n "$c" ]; then printf '$%.4f' "$c"; else printf '$?'; fi
}

# An agent's system prompt: frontmatter stripped, @-imports inlined. Delegates
# to `dragonfly expand-agent` so the harness and the bundled --agents
# registration share one @-expansion implementation (see expand_at_refs in
# src/main.rs). A relative @-path otherwise dangles under --append-system-prompt
# and the agent burns a turn failing to Read it.
agent_prompt() {
  "$DRAGONFLY" expand-agent "$1"
}

# ── User message per variant ────────────────────────────────────────────────
# review-agent is steered onto comments; comment-reviewer-4 is told it has no
# precomputed context and must find the PR itself; the rest run as-is.
task_for() {
  case "$1" in
    review-agent)
      cat <<'EOF'
Your assigned concern for this review is COMMENTS AND DOCUMENTATION ONLY.
Review the PR's comments and doc-comments against the project's comment
guidelines (appended to your instructions). Ignore correctness, performance,
and security issues unless they surface as a misleading or inaccurate comment.

The shared review context follows. Produce your review.
EOF
      ;;
    comment-reviewer-4)
      cat <<'EOF'
You have NOT been given any precomputed context (no diff, no file index, no
conventions). You are in the working directory of the checked-out PR branch.
Find the PR's changes yourself with git, then review the comments and
documentation per your instructions.
EOF
      ;;
    *)
      cat <<'EOF'
Review the current PR per your instructions, focused on comments and
documentation. The shared review context follows. Produce your review.
EOF
      ;;
  esac
}

run_variant() {
  local name="$1" file="$AGENTS_DIR/$1.md"
  local out="$OUTDIR/$name.md" err="$OUTDIR/$name.stderr.log"
  local timef="$OUTDIR/$name.time" costf="$OUTDIR/$name.cost" jsonf="$OUTDIR/$name.json"
  [ -f "$file" ] || { echo "MISSING agent file: $file" >"$out"; echo 0 >"$timef"; : >"$costf"; return; }

  # System prompt with @-imports already inlined. review-agent does not
  # @-reference the comment guidelines; append them so it still has the style
  # we steer it onto (the comment-reviewer variants @-ref them, so they are
  # already inlined).
  local sys
  sys="$(agent_prompt "$file")"
  case "$sys" in
    *code-comments.md*) : ;;
    *) sys="$sys"$'\n\n# Comment guidelines (code-comments.md)\n\n'"$(cat "$COMMENT_GUIDELINES")" ;;
  esac

  # Per-variant context: comment-reviewer gets the inlined diffs;
  # comment-reviewer-4 (the control) gets no shared context plus git access so
  # it can discover the PR itself; everyone else gets the /tmp-reference form.
  local -a tools=("${ALLOWED_TOOLS[@]}")
  local prompt
  case "$name" in
    comment-reviewer-4)
      tools+=("Bash(git:*)")
      prompt="$(task_for "$name")"
      ;;
    comment-reviewer)
      prompt="$(task_for "$name")"$'\n\n'"$(cat "$CONTEXT_INLINE")"
      ;;
    *)
      prompt="$(task_for "$name")"$'\n\n'"$(cat "$CONTEXT")"
      ;;
  esac

  local start end
  start="$(date +%s)"
  printf '%s' "$prompt" \
    | ( cd "$TARGET" && claude -p \
        --model "$MODEL" \
        --append-system-prompt "$sys" \
        --allowedTools "${tools[@]}" \
        --add-dir "${ADD_DIRS[@]}" \
        --output-format json ) \
        >"$jsonf" 2>"$err" \
    || true
  end="$(date +%s)"
  echo "$((end - start))" >"$timef"

  # Split the JSON envelope: .result is the review text, .total_cost_usd the
  # spend. Degrade gracefully if claude errored and produced no valid JSON.
  if jq -e . >/dev/null 2>&1 <"$jsonf"; then
    jq -r '.result // "(no .result in json output)"' "$jsonf" >"$out"
    jq -r '.total_cost_usd // empty' "$jsonf" >"$costf"
  else
    echo "(claude produced no valid JSON; see $err and $jsonf)" >"$out"
    : >"$costf"
  fi
}

# ── Fan out: one headless run per variant, in parallel ──────────────────────
log "running ${#VARIANTS[@]} variants in parallel (model: $MODEL)..."
pids=()
for v in "${VARIANTS[@]}"; do
  run_variant "$v" &
  pids+=("$!")
  log "  started $v (pid $!)"
done
for pid in "${pids[@]}"; do wait "$pid" || true; done
log "all variants finished"

# ── Compare ─────────────────────────────────────────────────────────────────
CMP_INPUT="$OUTDIR/_compare_input.md"
{
  echo "# Comment-reviewer variant outputs to compare"
  echo
  echo "Each section is one agent's review of the SAME PR, with its wall-clock"
  echo "runtime and dollar cost in the header. Variant prompts:"
  echo "- comment-reviewer:   detailed dedicated comment reviewer (full context, diffs INLINED in-prompt)"
  echo "- comment-reviewer-2: minimal (verify against guidelines, confidence >=80)"
  echo "- comment-reviewer-3: 'comments must earn their keep', glossary-aware"
  echo "- comment-reviewer-4: same prompt as comment-reviewer but WITHOUT precomputed context (discovers the PR itself)"
  echo "- review-agent:       general reviewer steered onto comments"
  echo
  for v in "${VARIANTS[@]}"; do
    t="$(cat "$OUTDIR/$v.time" 2>/dev/null || echo '?')"
    echo "## === $v  (runtime: ${t}s, cost: $(fmt_cost "$v")) ==="
    echo
    cat "$OUTDIR/$v.md"
    echo
  done
} >"$CMP_INPUT"

read -r -d '' CMP_SYS <<'EOF' || true
You are evaluating prompt variants for a code-comment reviewer. You are given
several agents' reviews of the SAME pull request, each preceded by its
wall-clock runtime and dollar cost. Compare them objectively. You are NOT
reviewing the PR yourself — you are judging the reviewers.

Judge each variant on BOTH dimensions, and keep them separate:
- Accuracy: are the findings correct, real, in scope, and free of false
  positives? Did it miss anything its peers caught?
- Style: are the suggested comment rewrites elegant, concise and readable, and
  does the reviewer's own writeup follow good comment style (lead with the
  rule, explain why-not-what, scannable, no fluff)? A reviewer can be accurate
  but stylistically poor, or stylish but wrong — call both out.

Produce:

1. **Findings matrix** — a markdown table: rows = distinct issues raised by ANY
   variant (one row per real issue, deduped across variants by file:line +
   substance), columns = the variants, cell = ✅ raised / — missed. Add a final
   "Consensus" column (how many variants raised it).

2. **Unique catches** — per variant, issues only it raised. Note whether each
   looks genuine or like a false positive.

3. **Likely false positives / noise** — issues that read as wrong, out of
   scope, or low-value nitpicks, attributed to the variant(s) that raised them.

4. **Accuracy & style scorecard** — per variant: a short verdict on accuracy
   and a separate one on style, plus signal-to-noise, output-format adherence,
   verbosity, and efficiency — runtime and cost (fast/cheap vs the others, and
   whether the extra time/spend bought better findings). Call out
   comment-reviewer-4 specifically: did running without precomputed context
   cost it accuracy, speed, or coverage?

5. **Recommendation** — which variant performed best on THIS PR overall
   (weighing accuracy, style, runtime, and cost) and why, plus one concrete
   prompt tweak that would most improve the runner-up.

Be specific and cite file:line. Keep it scannable.
EOF

# Print where the raw per-variant outputs landed first: `exec` below replaces
# this process with claude, so nothing after it runs.
echo
echo "Per-variant outputs: $OUTDIR"
for v in "${VARIANTS[@]}"; do
  t="$(cat "$OUTDIR/$v.time" 2>/dev/null || echo '?')"
  printf '  %-22s %4ss  %9s  %s\n' "$v" "$t" "$(fmt_cost "$v")" "$OUTDIR/$v.md"
done
echo

# Hand the terminal to an interactive claude session seeded with the judging
# instructions (system prompt) and the assembled outputs (first message), so
# the comparison streams live and you can interrogate it afterwards. exec, so
# the script process becomes claude rather than capturing and reprinting it.
log "exec into claude for the comparison pass..."
exec claude \
  --model "$MODEL" \
  --append-system-prompt "$CMP_SYS" \
  "$(cat "$CMP_INPUT")"
