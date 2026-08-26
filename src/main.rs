use chrono::DateTime;
use clap::{Parser, Subcommand, ValueEnum};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};
use tokio::time::sleep;

mod dedup;
mod guide_chunks;
mod pr_score;
mod sessions;
mod skill;
mod status;
mod watch_mcp;

#[derive(Parser)]
#[command(name = "dragonfly")]
struct Cli {
    /// Force push (e.g. after rebase)
    #[arg(long)]
    force: bool,

    /// Run unattended: accept the rebase default, derive the PR title
    /// instead of prompting, run the agent via `claude -p`, and keep the
    /// PR a draft (Phase 9 is skipped entirely).
    #[arg(long)]
    non_interactive: bool,

    /// PR title used when --non-interactive needs to create a draft PR.
    /// Falls back to the newest commit subject.
    #[arg(long)]
    title: Option<String>,

    /// Additional guidance for the agent, appended to the end of the main
    /// agent prompt as a <user-guidance> block.
    #[arg(short = 'm', long = "message", value_name = "MESSAGE")]
    message: Option<String>,

    /// Only run PR area analysis and print the result
    #[arg(long)]
    areas: bool,

    /// Submit feedback about dragonfly itself (appended to ~/.dragonfly/feedback)
    #[arg(long, value_name = "MESSAGE")]
    feedback: Option<String>,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// PR review thread operations
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
    /// CI status, failure logs, watch, flakiness, retries, rerun.
    /// Replaces ad-hoc gh pr checks / gh run incantations with bounded, agent-friendly output.
    Ci {
        #[command(subcommand)]
        command: CiCommand,
    },
    /// Print a prompt instead of invoking the model. With no subcommand, runs
    /// the full dragonfly flow (push, CI wait, data collection) and
    /// prints the main agent prompt. With `initial-review`, prints just the
    /// initial code review prompt (system + inlined diff).
    Prompt {
        #[command(subcommand)]
        target: Option<PromptTarget>,
    },
    /// Duplicate-function hints for this PR. With no subcommand, lists
    /// existing functions semantically similar to functions this branch
    /// added or changed (LLM behavior summaries embedded and compared by
    /// cosine similarity; summaries/embeddings cached by content hash under
    /// ~/.dragonfly/dedup and shared with the team via GCS, so each function
    /// hits the LLM once org-wide).
    Dedup {
        #[command(subcommand)]
        command: Option<DedupCommand>,
        /// Cosine similarity threshold for candidates.
        #[arg(long, default_value_t = dedup::DEFAULT_THRESHOLD)]
        threshold: f64,
        /// Max matches listed per changed function.
        #[arg(long, default_value_t = dedup::DEFAULT_LIMIT)]
        limit: usize,
        /// Override the base ref for the changed-function diff. Default:
        /// auto-detected via `pr_base_ref` (graphite stack parent or
        /// origin/main).
        #[arg(long)]
        base: Option<String>,
        /// Emit JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// List CLAUDE.md / AGENTS.md guides relevant to the given file paths.
    /// Walks parent dirs of each path up to the git toplevel, follows
    /// `@`-references transitively, dedupes, and prints one absolute path per
    /// line. Reads paths from stdin if none are provided.
    #[command(hide = true)]
    Guides {
        /// File paths (relative or absolute). Reads stdin if empty.
        paths: Vec<PathBuf>,
    },
    /// Score guide chunks for the current branch's PR via `lov eval rag score`.
    /// Resolves changed files → relevant guides → per-heading chunks, then
    /// asks the knowledge-RAG scorer to rate each chunk against the PR's
    /// diff. Prints a TSV sorted by score (desc) so a threshold can be
    /// eyeballed before committing to the production 7.5 default.
    #[command(hide = true)]
    ScoreGuides {
        /// Write TSV to FILE instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Override the base ref for the diff. Default: auto-detected via
        /// `pr_base_ref` (graphite stack parent or origin/main). Useful
        /// when HEAD is a detached merge commit and origin/main already
        /// contains it — pass `HEAD^1` (the pre-merge main) instead.
        #[arg(long)]
        base: Option<String>,
        /// Only emit chunks scoring >= THRESHOLD, plus every ancestor
        /// chunk (same file, prefix breadcrumbs) of those. Default: no
        /// filter, all 0–10 rows kept.
        #[arg(long)]
        threshold: Option<f64>,
    },
    /// Print an agent markdown file's system prompt: frontmatter stripped and
    /// `@`-imports inlined (resolved against the file's directory). Lets
    /// scripts/compare-comment-reviewers.sh reuse the binary's @-expansion
    /// instead of reimplementing it.
    #[command(hide = true)]
    ExpandAgent {
        /// Path to an agent markdown file (e.g. agents/comment-reviewer.md).
        file: PathBuf,
    },
    /// MCP channel server: pushes CI check results and new PR comments into a
    /// running Claude Code session as `<channel source="pr-watch">` events.
    /// Spawned by plugin/dragonfly-watcher over stdio; not for manual use.
    #[command(hide = true)]
    WatchMcp {
        /// Explicit PR number. Default: the current branch's pushed commit
        /// (and its PR, once one exists).
        #[arg(long)]
        pr: Option<String>,
        /// Poll interval in seconds (min 5).
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// Skip GitHub polling and emit one synthetic event shortly after the
        /// handshake. For end-to-end plumbing tests.
        #[arg(long)]
        demo: bool,
    },
}

#[derive(Subcommand)]
enum PromptTarget {
    /// Print the initial-review prompt (system prompt + the inlined diff
    /// that would be sent as the user message). Same content `pr review`
    /// would feed the model — useful for iterating on the prompt without
    /// burning a model call.
    InitialReview,
    /// Print the dragonfly-context block injected into review-agent
    /// subagents by the SubagentStart hook. Bundles commit list, changed
    /// files, per-file diffs (as /tmp path references by default, or
    /// inlined with --inline-diffs), and the scored <relevant-context>
    /// block. Cached for 4 minutes per cwd+mode; concurrent callers
    /// serialize on a filesystem advisory lock so a parallel review-agent
    /// fan-out only pays the build cost once.
    ReviewAgent {
        /// Inline each changed file's full diff as `<diff name="...">...</diff>`
        /// blocks instead of writing them to /tmp and referencing the paths.
        #[arg(long)]
        inline_diffs: bool,
    },
    /// Print the dragonfly-context block injected into dedup-reviewer
    /// subagents by the SubagentStart hook. Tailored for duplication review:
    /// commit list, changed files, per-file diff paths, and the full
    /// duplicate-function hint list inlined.
    DedupReviewer,
}

#[derive(Subcommand)]
enum DedupCommand {
    /// Record that a changed function is NOT a duplicate of some or all of
    /// its currently listed matches. With no MATCH args, dismisses every
    /// match currently listed for FUNC. Pass `-` as FUNC to batch-dismiss
    /// from stdin: one `<changed-func> [match...]` line per verdict.
    /// Accepts full identities (`go/api/pkg/util.(Server).handleFoo`) or a
    /// bare function name when unambiguous. Dismissed pairs are excluded
    /// from all future listings, on every machine: they sync through the
    /// same GCS cache as the summaries and embeddings.
    Dismiss {
        /// The changed function, as printed by `dragonfly dedup`, or `-`
        /// to read batch lines from stdin.
        func: String,
        /// Specific matches to dismiss (default: all currently listed).
        matches: Vec<String>,
    },
    /// List recorded not-a-duplicate pairs for this repo.
    Exclusions {
        /// Emit JSON instead of the human listing.
        #[arg(long)]
        json: bool,
    },
    /// Sync the shared dedup index with GCS (also runs automatically inside
    /// `dragonfly dedup`): pull packs other machines uploaded, and seed the
    /// remote when it is empty. Packs carry behavior summaries and embedding
    /// vectors keyed by content hash, plus dismissed not-a-duplicate pairs —
    /// never source code. Configure the location with DRAGONFLY_DEDUP_GCS
    /// (gs://bucket/prefix, or `off`).
    Sync {
        /// Push the entire local index as one pack, not just seed-if-empty.
        /// Recovers entries whose original run died before its upload.
        #[arg(long)]
        full: bool,
    },
}

#[derive(Subcommand)]
enum PrCommand {
    /// Review thread operations
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// Set the PR description (body) for the current branch's PR.
    /// Use `-` to read the body from stdin.
    Description {
        /// PR description body (markdown). Pass `-` to read from stdin.
        body: String,
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Post a top-level PR conversation comment on the current branch's PR.
    /// This is the issue-comment thread (the main PR conversation), not a
    /// review-thread reply — for that, use `pr thread comment`.
    Comment {
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
        /// Comment body (markdown). Pass `-` to read from stdin.
        #[arg(long)]
        body: String,
    },
    /// Print PR review threads, top-level reviews, and metadata in the same
    /// cleaned format used by the pre-collected data (review-threads,
    /// review-pr, pr-meta). Defaults to the current branch's PR.
    Comments {
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Run an initial code review of the current branch (defaults to Gemini).
    /// Same call that runs automatically inside `dragonfly` when no
    /// prior review log exists for the PR. Gemini is weaker than Claude,
    /// so output is best treated as a first-pass hint, not a final verdict.
    Review {
        /// Override the review model (default: gemini).
        #[arg(long)]
        model: Option<Model>,
    },
}

/// Shorthand names for the LLMs the `--model` flag accepts. Resolved to
/// the underlying `provider/model` string at call time so flags stay short.
#[derive(Clone, Copy, ValueEnum)]
enum Model {
    /// vertex/gemini-3.1-flash-lite — cheap/fast, weak on subtle bugs.
    Gemini,
    /// anthropic/claude-haiku-4-5
    Haiku,
    /// anthropic/claude-sonnet-4-6
    Sonnet,
    /// anthropic/claude-opus-4-7
    Opus,
}

impl Model {
    fn kit_id(self) -> &'static str {
        match self {
            Self::Gemini => "vertex/gemini-3.1-flash-lite",
            Self::Haiku => "anthropic/claude-haiku-4-5",
            Self::Sonnet => "anthropic/claude-sonnet-4-6",
            Self::Opus => "anthropic/claude-opus-4-7",
        }
    }
}

#[derive(Subcommand)]
enum ThreadCommand {
    /// Reply to a review thread
    Comment {
        /// The review thread ID (e.g. PRRT_kwDOJyl9f8541jLH)
        #[arg(long)]
        thread_id: String,
        /// The reply body
        #[arg(long)]
        body: String,
    },
    /// Resolve a review thread
    Resolve {
        /// The review thread ID (e.g. PRRT_kwDOJyl9f8541jLH)
        #[arg(long)]
        thread_id: String,
    },
}

#[derive(Subcommand)]
enum CiCommand {
    /// Compact, deduped view of all checks (one line each). Hides passed+skipped by default.
    /// Exits non-zero if any check is failing.
    Status {
        /// Show every check, including passed and skipped.
        #[arg(long)]
        all: bool,
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
    },
    /// For each failed check (GitHub Actions, Buildkite, Spacelift, Wiz, …), print
    /// a per-check section with per-step conclusions and extracted error lines,
    /// then the link. Output is piped through an LLM (claude --model haiku) to
    /// distill into a terse summary when the dump is non-trivial (>=1000 bytes);
    /// the per-check raw logs (truncated at 16k each) are saved to
    /// /tmp/psc-ci-failures-full-*.md and referenced in the footer.
    Failures {
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
        /// Maximum bytes of full log per check (default 8000).
        #[arg(long, default_value = "8000")]
        max_bytes: usize,
        /// Skip the LLM distill step; print the raw per-check dump.
        #[arg(long)]
        raw: bool,
        /// Override the distiller model (default: haiku).
        #[arg(long)]
        model: Option<Model>,
    },
    /// Poll the checks of the last-pushed commit until all pass or one fails
    /// (fail-fast), then print a final summary (same shape as `ci status`).
    ///
    /// Anchors on the pushed SHA (`@{push}` / origin/<branch>, or the PR head
    /// with --pr), so `git push && dragonfly ci watch` watches exactly what
    /// was pushed even if someone else pushes mid-watch. Waits for slow
    /// checks like test-e2e but skips the Graphite mergeability check,
    /// deploy, and doc-review.
    Watch {
        /// Anchor on this PR's head commit instead of the current branch's
        /// pushed commit.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Look back N commits on origin/main for a given check name and report how
    /// often it passed vs failed. Useful for diagnosing flaky tests.
    Flaky {
        /// Check name (e.g. `test-go`, `test-spanner`).
        name: String,
        /// How many recent main commits to inspect.
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// List the workflow runs for the current PR's branch with attempt counts so the
    /// agent can see "already retried, don't re-run".
    Retries {
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Resolve a check name to its workflow run and call `gh run rerun <id> --failed`.
    Rerun {
        /// Failed check name (matched against `gh pr checks`).
        name: String,
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
    },
    /// Run an existing failure-log file through the haiku distiller and print
    /// the summary. Useful for benchmarking the distill prompt against cached
    /// `/tmp/psc-failures-*.md` files without needing a live PR.
    Distill {
        /// Path to a failure-log markdown file (e.g. /tmp/psc-failures-XXXX.md).
        file: PathBuf,
        /// Override the distiller model (default: haiku).
        #[arg(long)]
        model: Option<Model>,
    },
}

// ── Structs ──────────────────────────────────────────────────────────────────

struct ShResult {
    code: i32,
    stdout: String,
    stderr: String,
}

struct TempFile {
    path: PathBuf,
    lines: usize,
}

struct PushResult {
    branch: String,
    strategy: &'static str,
    code: i32,
    stdout: String,
    stderr: String,
}

struct CheckCounts {
    passed: usize,
    failed: usize,
    pending: usize,
    skipping: usize,
    pending_names: Vec<String>,
    /// Conclusion=`cancelled` runs. Held apart from `failed` because a
    /// cancellation usually means the run was superseded (cancel-superseded
    /// after a re-trigger); `ci watch` debounces these for a poll rather than
    /// fail-fasting on a phantom. Sorted, for cross-poll settle comparison.
    cancelled_names: Vec<String>,
    /// Conclusion=`stale` runs — a queued check the GHA scheduler abandoned
    /// when a newer run for the same head superseded it. Never a code failure.
    stale: usize,
}

struct PrInfo {
    number: Option<String>,
    url: Option<String>,
    is_draft: bool,
    /// GitHub login of the PR author. Empty when no PR exists or the field
    /// wasn't available. Compared against the viewer's login to decide
    /// whether the agent should run in review-only mode.
    author_login: String,
}

struct CiResult {
    files: Vec<TempFile>,
    #[allow(dead_code)]
    has_unresolved: bool,
    skip_ci: Option<String>,
    failed_names: Vec<String>,
}

struct CiWaitResult {
    ci_content: String,
    failures_content: Option<String>,
    failures_full_file: Option<TempFile>,
    failed_names: Vec<String>,
    lint_files: Vec<TempFile>,
}

struct FailureLogs {
    content: String,
    names: Vec<String>,
    /// Set when `content` is an LLM-distilled summary; points at the
    /// untruncated raw log on disk so the agent can drill in if needed.
    full_file: Option<TempFile>,
}

struct LintResult {
    name: String,
    code: i32,
    stdout: String,
    stderr: String,
}

struct ContextStrings {
    changed_files: String,
    main_commits: String,
    pr_commits: String,
}

struct MergeResult {
    content: String,
    has_conflicts: bool,
}

// ── Shell helpers ────────────────────────────────────────────────────────────

async fn sh(cmd: &str) -> Option<String> {
    let r = Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;
    if r.status.success() {
        Some(String::from_utf8_lossy(&r.stdout).trim().to_string())
    } else {
        None
    }
}

async fn sh3(cmd: &str) -> ShResult {
    let r = Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("failed to run command");
    ShResult {
        code: r.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&r.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&r.stderr).trim().to_string(),
    }
}

fn sh_bg(cmd: &str) -> Child {
    Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn command")
}

async fn sh_wait(child: Child) -> Option<String> {
    let out = child.wait_with_output().await.ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

async fn sh3_wait(child: Child) -> ShResult {
    let out = child
        .wait_with_output()
        .await
        .expect("failed to wait for command");
    ShResult {
        code: out.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    }
}

// ── Temp file helpers ────────────────────────────────────────────────────────

fn write_section(prefix: &str, content: &str, suffix: &str) -> TempFile {
    let f = tempfile::Builder::new()
        .prefix(&format!("psc-{prefix}-"))
        .suffix(suffix)
        .tempfile_in("/tmp")
        .expect("failed to create temp file");
    let (mut file, path) = f.keep().expect("failed to persist temp file");
    file.write_all(content.as_bytes())
        .expect("failed to write temp file");
    let lines =
        content.lines().count() + usize::from(!content.ends_with('\n') && !content.is_empty());
    TempFile { path, lines }
}

fn section(prefix: &str, content: &str) -> TempFile {
    write_section(prefix, content, ".md")
}

fn section_json(prefix: &str, content: &str) -> TempFile {
    write_section(prefix, content, ".json")
}

fn parse_json<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    serde_json::from_str(text).ok()
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

// ── Feedback ─────────────────────────────────────────────────────────────────

fn submit_feedback(message: &str) {
    let dir = home_dir().join(".dragonfly");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("Failed to create {}: {e}", dir.display());
        std::process::exit(1);
    }
    let path = dir.join("feedback");
    let now = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let branch = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-".into());
    let entry = format!("---\n{now} [{cwd} @ {branch}]\n{message}\n\n");

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(entry.as_bytes()) {
                eprintln!("Failed to write feedback: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Failed to open {}: {e}", path.display());
            std::process::exit(1);
        }
    }

    let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/logo-icon.png");
    let _ = std::process::Command::new("notify-send")
        .args([
            "--app-name=dragonfly",
            "--urgency=critical",
            &format!("--icon={icon}"),
            "dragonfly feedback",
            message,
        ])
        .status();

    println!("Feedback saved to {}", path.display());
}

// ── Push ─────────────────────────────────────────────────────────────────────

/// If the branch is behind origin/main and would rebase cleanly, rebase it.
/// New branches (no upstream) rebase automatically; branches with a remote
/// counterpart prompt first. Reuses the merge-tree probe that will later
/// drive the "Merge Conflict Check" prompt section. Returns true if a rebase
/// actually happened, so the caller can promote a normal push to a force-push.
///
/// Graphite branches are skipped entirely: `git rebase origin/main` rewrites
/// only the current branch and leaves Graphite's parent metadata and any
/// descendant branches pointing at orphaned commits. Restacking is the user's
/// job (`gt sync`), not this flow's.
async fn maybe_rebase_on_main(
    has_upstream: bool,
    merge_probe: &ShResult,
    review_only: bool,
    non_interactive: bool,
) -> bool {
    let behind = sh("git rev-list --count HEAD..origin/main")
        .await
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if behind == 0 {
        return false;
    }

    if is_graphite_branch().await {
        println!(
            "   Branch is {behind} behind origin/main; skipping auto-rebase (Graphite branch — run `gt sync` to restack)."
        );
        return false;
    }

    if review_only {
        println!(
            "   Branch is {behind} behind origin/main; skipping rebase prompt (review-only mode)."
        );
        return false;
    }

    let dirty = sh("git status --porcelain --untracked-files=no")
        .await
        .unwrap_or_default();
    if !dirty.is_empty() {
        println!(
            "   Branch is {behind} behind origin/main; skipping auto-rebase (working tree dirty)."
        );
        return false;
    }

    if merge_probe.code != 0 {
        println!(
            "   Branch is {behind} behind origin/main; skipping auto-rebase (would conflict)."
        );
        return false;
    }

    let proceed = if !has_upstream {
        println!("   Branch is {behind} behind origin/main — rebasing (new branch)...");
        true
    } else if non_interactive {
        println!(
            "   Branch is {behind} behind origin/main and rebase is clean — rebasing (non-interactive)."
        );
        true
    } else {
        let prompt =
            format!("Branch is {behind} behind origin/main and rebase is clean. Rebase now?");
        let _hold = status::hold();
        match dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(true)
            .interact()
        {
            Ok(yes) => yes,
            Err(e) => {
                println!("   Rebase prompt cancelled: {e}");
                false
            }
        }
    };
    if !proceed {
        return false;
    }

    let r = sh3("git rebase origin/main").await;
    if r.code == 0 {
        println!("✅ Rebased on origin/main");
        return true;
    }
    println!("⚠️  Rebase on origin/main failed:");
    if !r.stdout.is_empty() {
        println!("{}", r.stdout);
    }
    if !r.stderr.is_empty() {
        println!("{}", r.stderr);
    }
    let _ = sh3("git rebase --abort").await;
    false
}

async fn push(force: bool, review_only: bool, non_interactive: bool) -> (PushResult, ShResult) {
    println!("   Fetching remote...");
    let bg_fetch = sh_bg("git fetch");

    let branch = sh("git branch --show-current").await.unwrap_or_default();
    if branch.is_empty() {
        eprintln!("❌ Not on a branch. Aborting.");
        std::process::exit(1);
    }
    println!("   Branch: {branch}");

    sh_wait(bg_fetch).await;

    // One merge-tree probe drives both the rebase decision below and the
    // prompt's "Merge Conflict Check" section (returned to the caller).
    let bg_merge = sh_bg(MERGE_TREE_PROBE_CMD);
    let upstream = sh("git rev-parse --abbrev-ref @{upstream} 2>/dev/null").await;
    let merge_probe = sh3_wait(bg_merge).await;

    let rebased = maybe_rebase_on_main(
        upstream.is_some(),
        &merge_probe,
        review_only,
        non_interactive,
    )
    .await;
    let force = force || rebased;
    // After a successful rebase HEAD sits on top of origin/main, so the
    // pre-rebase merge probe is stale. The new state is trivially clean.
    let merge_probe = if rebased {
        ShResult {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    } else {
        merge_probe
    };

    // A bare `git push` only does the right thing when @{upstream} is
    // origin/<branch>. With no upstream, or an upstream pointing elsewhere
    // (a branch cut from main keeps @{upstream} = origin/main), bare push
    // targets the wrong ref and 128s against protected main. Both cases push
    // -u to the same-name branch instead. --force-with-lease when a rebase
    // moved HEAD so an existing same-name branch fast-forwards safely.
    let proper_upstream = upstream
        .as_deref()
        .is_some_and(|u| u == format!("origin/{branch}"));
    if !proper_upstream {
        let why = match upstream.as_deref() {
            Some(u) => format!("upstream is {u}, not origin/{branch}"),
            None => "no upstream".to_string(),
        };
        let cmd = if force {
            format!("git push -u --force-with-lease origin HEAD:{branch}")
        } else {
            format!("git push -u origin HEAD:{branch}")
        };
        println!("   {why} — pushing -u to origin/{branch}...");
        let r = sh3(&cmd).await;
        return (
            PushResult {
                branch,
                strategy: "new",
                code: r.code,
                stdout: r.stdout,
                stderr: r.stderr,
            },
            merge_probe,
        );
    }

    let ab = sh("git rev-list --left-right --count HEAD...@{upstream}").await;
    let (ahead, behind) = ab
        .as_deref()
        .and_then(|s| {
            let mut parts = s.split_whitespace();
            Some((
                parts.next()?.parse::<i64>().ok()?,
                parts.next()?.parse::<i64>().ok()?,
            ))
        })
        .unwrap_or((0, 0));

    if ahead == 0 && behind == 0 {
        println!("✅ Already up to date with remote.");
        return (
            PushResult {
                branch,
                strategy: "up-to-date",
                code: 0,
                stdout: "Already up to date".into(),
                stderr: String::new(),
            },
            merge_probe,
        );
    }

    let needs_force = behind > 0;
    if needs_force && !force {
        let msg = if ahead > 0 {
            format!("Diverged (+{ahead} -{behind})")
        } else {
            format!("Local is {behind} behind remote")
        };
        eprintln!("⚠️  {msg}. Pass --force to force push.");
        std::process::exit(1);
    }

    let label = if needs_force {
        format!("+{ahead} -{behind}")
    } else {
        format!("{ahead} ahead")
    };
    let kind = if needs_force { "Force push" } else { "Push" };
    let cmd = if needs_force {
        "git push --force-with-lease"
    } else {
        "git push"
    };
    println!("   {kind} ({label})...");
    let r = sh3(cmd).await;
    (
        PushResult {
            branch,
            strategy: if needs_force {
                "force-with-lease"
            } else {
                "fast-forward"
            },
            code: r.code,
            stdout: r.stdout,
            stderr: r.stderr,
        },
        merge_probe,
    )
}

// ── Reviews ──────────────────────────────────────────────────────────────────

const REVIEW_THREADS_QUERY: &str = r#"query($owner: String!, $repo: String!, $pr: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          comments(first: 50) {
            nodes {
              id
              author { login }
              body
              createdAt
            }
          }
        }
      }
    }
  }
}"#;

#[derive(Deserialize, Default)]
struct GqlThreadsResponse {
    data: Option<GqlData>,
}
#[derive(Deserialize, Default)]
struct GqlData {
    repository: Option<GqlRepo>,
}
#[derive(Deserialize, Default)]
struct GqlRepo {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GqlPR>,
}
#[derive(Deserialize, Default)]
struct GqlPR {
    #[serde(rename = "reviewThreads")]
    review_threads: Option<GqlThreads>,
}
#[derive(Deserialize, Default)]
struct GqlThreads {
    nodes: Vec<GqlThread>,
}
#[derive(Deserialize)]
struct GqlThread {
    id: String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "isOutdated")]
    is_outdated: bool,
    path: Option<String>,
    line: Option<u64>,
    comments: Option<GqlComments>,
}
#[derive(Deserialize, Default)]
struct GqlComments {
    nodes: Vec<GqlComment>,
}
#[derive(Deserialize)]
struct GqlComment {
    id: String,
    author: Option<GqlAuthor>,
    body: String,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
}
#[derive(Deserialize)]
struct GqlAuthor {
    login: String,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn cdata(s: &str) -> String {
    // CDATA cannot contain "]]>", so split if needed
    format!("<![CDATA[{}]]>", s.replace("]]>", "]]]]><![CDATA[>"))
}

/// Strip noisy HTML/markdown from bot comment bodies, keeping only the useful text.
#[derive(Deserialize)]
struct PrReviewsWrapper {
    reviews: Vec<PrReview>,
}

#[derive(Deserialize)]
struct PrReview {
    author: Option<GqlAuthor>,
    state: String,
    body: String,
}

fn format_pr_reviews(json: &str) -> Option<String> {
    let wrapper: PrReviewsWrapper = serde_json::from_str(json).ok()?;
    let mut out = String::from("<pr-reviews>\n");
    let mut any = false;
    for r in &wrapper.reviews {
        let author = r
            .author
            .as_ref()
            .map(|a| a.login.as_str())
            .unwrap_or("unknown");
        let body = r.body.trim();

        // Skip empty reviews unless they have a meaningful state (APPROVED, CHANGES_REQUESTED)
        if body.is_empty() && r.state == "COMMENTED" {
            continue;
        }

        // Skip bot boilerplate
        if body.contains("BUGBOT_REVIEW")
            || body.contains("CURSOR_AUTOMATION_ID")
            || body.contains("Comment `@claude review`")
            || body.starts_with("<details>\n<summary>Stale comment")
            || body.starts_with("<details>\r\n<summary>Stale comment")
        {
            continue;
        }

        any = true;
        if body.is_empty() {
            out.push_str(&format!(
                "<review author=\"{author}\" state=\"{}\"/>\n",
                xml_escape(&r.state)
            ));
        } else {
            let cleaned = clean_bot_body(body);
            out.push_str(&format!(
                "<review author=\"{author}\" state=\"{}\">\n{}\n</review>\n",
                xml_escape(&r.state),
                cdata(&cleaned),
            ));
        }
    }
    out.push_str("</pr-reviews>");
    if any { Some(out) } else { None }
}

fn clean_bot_body(raw: &str) -> String {
    // Unescape HTML entities that GitHub API returns
    let s = html_escape::decode_html_entities(raw);

    let mut out = String::new();
    let mut in_strip = false;

    for line in s.lines() {
        let trimmed = line.trim();

        // Skip everything between <div>...</div> and <details>...</details> blocks (Cursor links, buttons)
        if trimmed.starts_with("<div>") || trimmed.starts_with("<details>") {
            in_strip = true;
            continue;
        }
        if in_strip {
            if trimmed.starts_with("</div>") || trimmed.starts_with("</details>") {
                in_strip = false;
            }
            continue;
        }

        // Skip HTML comment markers we don't need
        if trimmed.starts_with("<!-- DESCRIPTION START")
            || trimmed.starts_with("<!-- DESCRIPTION END")
            || trimmed.starts_with("<!-- BUGBOT_BUG_ID")
            || trimmed.starts_with("<!-- LOCATIONS START")
        {
            continue;
        }

        // Skip LOCATIONS block content (file#Lnn-Lnn lines between LOCATIONS START/END)
        if trimmed.starts_with("LOCATIONS END") {
            continue;
        }

        // Skip "Reviewed by" footer lines
        if trimmed.starts_with("<sup>") || trimmed.contains("Reviewed by") {
            continue;
        }

        // Skip bare location lines inside LOCATIONS blocks (already in path/line attrs)
        if trimmed.ends_with("-->") && !trimmed.starts_with("<!--") {
            continue;
        }

        // Skip lines that are just file#L references (from LOCATIONS block)
        if !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && !trimmed.starts_with('-')
            && trimmed.contains("#L")
            && !trimmed.contains(' ')
        {
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    // Collapse runs of 3+ blank lines into 2
    let re = Regex::new(r"\n{3,}").unwrap();
    re.replace_all(out.trim(), "\n\n").to_string()
}

fn format_threads_xml(threads: &[GqlThread]) -> String {
    let mut out = String::from("<review-threads>\n");
    for t in threads {
        let status = if t.is_resolved {
            "resolved"
        } else if t.is_outdated {
            "outdated"
        } else {
            "open"
        };
        let path = t.path.as_deref().unwrap_or("unknown");
        let line = t.line.map(|l| l.to_string()).unwrap_or_default();
        out.push_str(&format!(
            "<thread id=\"{}\" status=\"{status}\" path=\"{path}\" line=\"{line}\">\n",
            xml_escape(&t.id)
        ));
        if let Some(comments) = &t.comments {
            for c in &comments.nodes {
                let author = c
                    .author
                    .as_ref()
                    .map(|a| a.login.as_str())
                    .unwrap_or("unknown");
                let time = c.created_at.as_deref().unwrap_or("");
                let body = clean_bot_body(&c.body);
                out.push_str(&format!(
                    "  <comment id=\"{}\" author=\"{author}\" created=\"{time}\">\n{}\n  </comment>\n",
                    xml_escape(&c.id),
                    cdata(&body),
                ));
            }
        }
        out.push_str("</thread>\n");
    }
    out.push_str("</review-threads>");
    out
}

struct PrCommentsBundle {
    threads_xml: Option<String>,
    /// Raw JSON from `gh api graphql`, kept only when XML parsing failed.
    threads_raw_json: Option<String>,
    reviews_xml: Option<String>,
    /// Top-level PR conversation (issue) comments — human-written and bot
    /// status alike. Review threads + reviews miss these entirely, so a
    /// failing Lovmesh Plan Preview (a CI-equivalent ❌) was invisible.
    issue_comments_xml: Option<String>,
    meta: Option<String>,
    has_unresolved: bool,
}

#[derive(Deserialize)]
struct RestIssueComment {
    #[serde(default)]
    user: Option<RestUser>,
    #[serde(default)]
    body: String,
    #[serde(rename = "created_at", default)]
    created_at: String,
    #[serde(rename = "html_url", default)]
    html_url: String,
}
#[derive(Deserialize)]
struct RestUser {
    #[serde(default)]
    login: String,
}

/// Classify a PR conversation (issue) comment so output can collapse bot
/// boilerplate while keeping substantive comments. Returns (kind, collapse):
/// `collapse` true ⇒ render a one-line stub instead of the full body.
///
/// Actionable bot status (Lovmesh Plan Preview — a CI-equivalent warehouse
/// apply signal) is kept in full; codecov tables, catalog-freshness, the
/// claude review-trigger prompt, and pr-classification are collapsed. Anything
/// unrecognised (human comments) is kept in full.
fn classify_issue_comment(author: &str, body: &str) -> (&'static str, bool) {
    let b = body.to_ascii_lowercase();
    const IMPORTANT: &[&str] = &["lovmesh", "plan preview", "plan apply", "warehouse model"];
    if IMPORTANT.iter().any(|m| b.contains(m)) {
        return ("bot-status", false);
    }
    const BOILERPLATE: &[&str] = &[
        "codecov",
        "catalog-freshness",
        "catalog freshness",
        "pr classification",
        "pr-classification",
        "classified this pr",
        "review-trigger",
        "/claude review",
        "@claude review",
    ];
    let noisy_bot = matches!(
        author,
        "codecov[bot]" | "codecov-commenter" | "lovable-ci-bot"
    );
    if noisy_bot || BOILERPLATE.iter().any(|m| b.contains(m)) {
        return ("boilerplate", true);
    }
    ("comment", false)
}

/// Fetch the PR's top-level conversation comments and render them as an
/// `<issue-comments>` block, collapsing known bot boilerplate to a stub.
/// `gh api --paginate` merges the array pages, so it parses as one array.
async fn fetch_issue_comments(owner: &str, repo: &str, pr_number: &str) -> Option<String> {
    let r = sh3(&format!(
        "gh api 'repos/{owner}/{repo}/issues/{pr_number}/comments?per_page=100' --paginate"
    ))
    .await;
    if r.stdout.trim().is_empty() {
        return None;
    }
    let comments: Vec<RestIssueComment> = parse_json(&r.stdout)?;
    if comments.is_empty() {
        return None;
    }
    let mut out = String::from("<issue-comments>\n");
    for c in &comments {
        let author = c
            .user
            .as_ref()
            .map(|u| u.login.as_str())
            .unwrap_or("unknown");
        let (kind, collapse) = classify_issue_comment(author, &c.body);
        if collapse {
            let first = c
                .body
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            out.push_str(&format!(
                "  <comment author=\"{}\" kind=\"{kind}\" created=\"{}\" collapsed=\"true\">{}</comment>\n",
                xml_escape(author),
                c.created_at,
                xml_escape(truncate(first, 100)),
            ));
        } else {
            let body = clean_bot_body(&c.body);
            out.push_str(&format!(
                "  <comment author=\"{}\" kind=\"{kind}\" created=\"{}\" url=\"{}\">\n{}\n  </comment>\n",
                xml_escape(author),
                c.created_at,
                xml_escape(&c.html_url),
                cdata(&body),
            ));
        }
    }
    out.push_str("</issue-comments>");
    Some(out)
}

async fn fetch_pr_comments(owner: &str, repo: &str, pr_number: &str) -> PrCommentsBundle {
    let query_escaped = REVIEW_THREADS_QUERY.replace('\'', "'\\''");
    let bg_threads = sh_bg(&format!(
        "gh api graphql -f query='{query_escaped}' -f owner={owner} -f repo={repo} -F pr={pr_number}"
    ));
    let bg_pr_view = sh_bg(&format!(
        "gh pr view {pr_number} --json title,body,reviewDecision,reviews,reviewRequests"
    ));
    let issue_comments_fut = fetch_issue_comments(owner, repo, pr_number);

    let mut bundle = PrCommentsBundle {
        threads_xml: None,
        threads_raw_json: None,
        reviews_xml: None,
        issue_comments_xml: None,
        meta: None,
        has_unresolved: false,
    };

    let threads = sh3_wait(bg_threads).await;
    if !threads.stdout.is_empty() {
        if let Some(resp) = parse_json::<GqlThreadsResponse>(&threads.stdout) {
            let nodes = resp
                .data
                .and_then(|d| d.repository)
                .and_then(|r| r.pull_request)
                .and_then(|p| p.review_threads)
                .map(|t| t.nodes)
                .unwrap_or_default();
            bundle.has_unresolved = nodes.iter().any(|t| !t.is_resolved && !t.is_outdated);
            if !nodes.is_empty() {
                bundle.threads_xml = Some(format_threads_xml(&nodes));
            }
        } else {
            bundle.threads_raw_json = Some(threads.stdout);
        }
    }

    let pr_view = sh3_wait(bg_pr_view).await;
    if !pr_view.stdout.is_empty() {
        bundle.reviews_xml = format_pr_reviews(&pr_view.stdout);
        bundle.meta = format_pr_meta(&pr_view.stdout);
    }

    bundle.issue_comments_xml = issue_comments_fut.await;

    bundle
}

async fn collect_reviews(owner: &str, repo: &str, pr_number: &str) -> (Vec<TempFile>, bool) {
    let bundle = fetch_pr_comments(owner, repo, pr_number).await;
    let mut files = Vec::new();
    if let Some(xml) = &bundle.threads_xml {
        files.push(section("review-threads", xml));
    } else if let Some(raw) = &bundle.threads_raw_json {
        files.push(section_json("review-threads", raw));
    }
    if let Some(xml) = &bundle.reviews_xml {
        files.push(section("review-pr", xml));
    }
    if let Some(meta) = &bundle.meta {
        files.push(section("pr-meta", meta));
    }
    if let Some(xml) = &bundle.issue_comments_xml {
        files.push(section("issue-comments", xml));
    }
    (files, bundle.has_unresolved)
}

async fn pr_comments(pr_arg: Option<String>) {
    let pr_number = match pr_arg {
        Some(n) => n.trim().to_string(),
        None => match sh("gh pr view --json number --jq '.number'").await {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                eprintln!("Failed to find a PR for the current branch.");
                std::process::exit(1);
            }
        },
    };
    if pr_number.is_empty() || !pr_number.chars().all(|c| c.is_ascii_digit()) {
        eprintln!("Invalid PR number: {pr_number:?}");
        std::process::exit(1);
    }

    let url = match sh(&format!("gh pr view {pr_number} --json url --jq '.url'")).await {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("Failed to fetch PR URL for #{pr_number}.");
            std::process::exit(1);
        }
    };
    let parts: Vec<&str> = url.split('/').collect();
    if parts.len() < 5 {
        eprintln!("Unexpected PR URL: {url}");
        std::process::exit(1);
    }
    let (owner, repo) = (parts[3], parts[4]);

    let bundle = fetch_pr_comments(owner, repo, &pr_number).await;

    let mut sections: Vec<(&str, String)> = Vec::new();
    if let Some(meta) = bundle.meta {
        sections.push(("pr-meta", meta));
    }
    if let Some(xml) = bundle.reviews_xml {
        sections.push(("review-pr", xml));
    }
    if let Some(xml) = bundle.threads_xml {
        sections.push(("review-threads", xml));
    } else if let Some(raw) = bundle.threads_raw_json {
        sections.push(("review-threads (raw JSON — XML parse failed)", raw));
    }
    if let Some(xml) = bundle.issue_comments_xml {
        sections.push(("issue-comments", xml));
    }

    if sections.is_empty() {
        eprintln!("No review threads, reviews, comments, or metadata found for PR #{pr_number}.");
        return;
    }

    for (i, (label, body)) in sections.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("<!-- {label} -->");
        println!("{}", body.trim_end());
    }
}

#[derive(Deserialize, Default)]
struct PrViewMeta {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
    #[serde(rename = "reviewRequests", default)]
    review_requests: Vec<PrReviewRequest>,
}

#[derive(Deserialize, Default)]
struct PrReviewRequest {
    #[serde(default)]
    login: Option<String>,
    #[serde(default, rename = "name")]
    team_name: Option<String>,
}

fn format_pr_meta(json: &str) -> Option<String> {
    let meta: PrViewMeta = serde_json::from_str(json).ok()?;
    let mut out = String::new();
    out.push_str(&format!("# PR\n\nTitle: {}\n", meta.title.trim()));
    let decision = meta
        .review_decision
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("none");
    out.push_str(&format!("Review decision: {decision}\n"));

    let requested: Vec<String> = meta
        .review_requests
        .iter()
        .filter_map(|r| r.login.clone().or_else(|| r.team_name.clone()))
        .collect();
    if requested.is_empty() {
        out.push_str("Requested reviewers: none\n");
    } else {
        out.push_str(&format!("Requested reviewers: {}\n", requested.join(", ")));
    }

    let body = meta.body.trim();
    if body.is_empty() {
        out.push_str("\n## Body\n\n(empty)\n");
    } else {
        out.push_str(&format!("\n## Body\n\n{body}\n"));
    }
    Some(out)
}

/// Footer appended to every PR comment / thread reply posted through this CLI.
/// Lets reviewers tell agent-authored comments from human ones at a glance, and
/// the link points back at the tool so a curious reader can find the source.
const DRAGONFLY_FOOTER: &str =
    "\n\n<sup>via [Dragonfly](https://github.com/HalfVoxel/dragonfly) (Claude)</sup>";

async fn pr_thread_comment(thread_id: &str, body: &str) {
    let signed = format!("{body}{DRAGONFLY_FOOTER}");
    let r = sh3(&format!(
        "gh api graphql -f query='mutation($threadId: ID!, $body: String!) {{ \
            addPullRequestReviewThreadReply(input: {{pullRequestReviewThreadId: $threadId, body: $body}}) {{ \
                comment {{ id }} \
            }} \
        }}' -f threadId={thread_id} -f body='{}'",
        signed.replace('\'', "'\\''")
    ))
    .await;
    if r.code == 0 {
        println!("Replied to thread {thread_id}");
    } else {
        eprintln!("Failed to reply: {}", r.stderr);
        std::process::exit(1);
    }
}

async fn pr_set_description(pr_arg: Option<String>, body_arg: &str) {
    let body = if body_arg == "-" {
        let mut s = String::new();
        if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s) {
            eprintln!("Failed to read description from stdin: {e}");
            std::process::exit(1);
        }
        s
    } else {
        body_arg.to_string()
    };

    let Some(pr_number) = resolve_pr_number(pr_arg).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        std::process::exit(2);
    };

    // Use --body-file - so we don't have to escape arbitrary markdown for argv.
    let mut child = match std::process::Command::new("gh")
        .args(["pr", "edit", &pr_number, "--body-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to spawn gh: {e}");
            std::process::exit(1);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(body.as_bytes()) {
            eprintln!("Failed to write body to gh stdin: {e}");
            std::process::exit(1);
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Failed to wait for gh: {e}");
            std::process::exit(1);
        }
    };
    if out.status.success() {
        println!("Updated PR #{pr_number} description.");
    } else {
        eprintln!(
            "gh pr edit failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        std::process::exit(out.status.code().unwrap_or(1));
    }
}

async fn pr_comment(pr_arg: Option<String>, body_arg: &str) {
    let body = if body_arg == "-" {
        let mut s = String::new();
        if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s) {
            eprintln!("Failed to read comment body from stdin: {e}");
            std::process::exit(1);
        }
        s
    } else {
        body_arg.to_string()
    };

    let Some(pr_number) = resolve_pr_number(pr_arg).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        std::process::exit(2);
    };

    let signed = format!("{}{}", body.trim_end(), DRAGONFLY_FOOTER);

    let mut child = match std::process::Command::new("gh")
        .args(["pr", "comment", &pr_number, "--body-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to spawn gh: {e}");
            std::process::exit(1);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(signed.as_bytes()) {
            eprintln!("Failed to write body to gh stdin: {e}");
            std::process::exit(1);
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Failed to wait for gh: {e}");
            std::process::exit(1);
        }
    };
    if out.status.success() {
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if url.is_empty() {
            println!("Commented on PR #{pr_number}.");
        } else {
            println!("Commented on PR #{pr_number}: {url}");
        }
    } else {
        eprintln!(
            "gh pr comment failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        std::process::exit(out.status.code().unwrap_or(1));
    }
}

async fn pr_thread_resolve(thread_id: &str) {
    let r = sh3(&format!(
        "gh api graphql -f query='mutation($threadId: ID!) {{ \
            resolveReviewThread(input: {{threadId: $threadId}}) {{ \
                thread {{ isResolved }} \
            }} \
        }}' -f threadId={thread_id}"
    ))
    .await;
    if r.code == 0 {
        println!("Resolved thread {thread_id}");
    } else {
        eprintln!("Failed to resolve: {}", r.stderr);
        std::process::exit(1);
    }
}

// ── CI ───────────────────────────────────────────────────────────────────────

fn run_id_from_url(url: &str) -> u64 {
    let re = Regex::new(r"/(\d+)").unwrap();
    re.find_iter(url)
        .last()
        .and_then(|m| m.as_str().trim_start_matches('/').parse().ok())
        .unwrap_or(0)
}

// Graphite's mergeability check sits `pending` indefinitely — it never reports a
// terminal state — so any path that waits on it hangs forever. Excluded from
// every wait path, including `ci watch` (which otherwise waits for slow checks).
const GRAPHITE_MERGEABILITY: &str = "Graphite / mergeability_check";

// The core-prompt-review gate (job name `review`) fails until a human from
// @lovablelabs/ai approves the PR head — no CI fix can ever turn it green, so
// waiting on or auto-fixing it is pointless.
const CORE_PROMPT_REVIEW: &str = "review";

// QA.tech posts an external PR-review check that stays `pending` until its bot
// finishes (and never runs on many PRs), so no code change flips it green.
// Waiting on it hangs `ci watch`; it's non-blocking like the other review bots.
const QA_TECH_REVIEW: &str = "QA.tech / PR Review";

// The claude-code-action posts a "Claude Code Review" check that stays `pending`
// while its AI review runs (often many minutes), and the review is advisory so
// no code change flips it green. Waiting on it hangs `ci watch`/`pr review`;
// it's non-blocking like the other review bots.
const CLAUDE_CODE_REVIEW: &str = "Claude Code Review";

// The high-risk merge gate posts the commit status "high-risk-needs-approval":
// `pending` until a human approves a pr/risk/high PR, `success` after. No code
// change flips it, so waiting on it blocks forever on an unapproved PR.
const HIGH_RISK_APPROVAL: &str = "high-risk-needs-approval";

// The "Relevant Evals" workflow runs evals for 15+ minutes and its result is
// advisory. Its matrix job names embed the eval name and path ("Run relevant
// eval (<name>, <path>) / ..."), so they match by prefix (see
// [is_ignored_check]). The aggregate `Relevant eval status` job has
// `if: always()` and `needs: run-selected`, so it stays `pending` until every
// eval finishes — ignoring only the per-eval jobs would still block the wait.
const EVAL_RUN_PREFIX: &str = "Run relevant eval (*";
const EVAL_SELECT: &str = "Select relevant evals";
const EVAL_STATUS: &str = "Relevant eval status";

// Checks that are slow, flaky, or non-blocking — exclude from the wait so they
// don't keep `pending` above zero forever. Failures here are surfaced to the
// user but not auto-fixed as part of dragonfly.
const IGNORED_CHECKS: &[&str] = &[
    "Cursor Bugbot",
    "test-e2e",
    "doc-review",
    "deploy",
    GRAPHITE_MERGEABILITY,
    CORE_PROMPT_REVIEW,
    QA_TECH_REVIEW,
    CLAUDE_CODE_REVIEW,
    HIGH_RISK_APPROVAL,
    EVAL_RUN_PREFIX,
    EVAL_SELECT,
    EVAL_STATUS,
    "depthfirst Bot",
];

// `ci watch` waits for slow checks like test-e2e, but skips the never-terminating
// Graphite check plus deploy, doc-review, the human-approval gates, the eval
// suite, and the review bots (non-blocking, not worth blocking the watch on).
// See [GRAPHITE_MERGEABILITY].
const WATCH_IGNORED_CHECKS: &[&str] = &[
    "doc-review",
    "deploy",
    GRAPHITE_MERGEABILITY,
    CORE_PROMPT_REVIEW,
    QA_TECH_REVIEW,
    CLAUDE_CODE_REVIEW,
    HIGH_RISK_APPROVAL,
    EVAL_RUN_PREFIX,
    EVAL_SELECT,
    EVAL_STATUS,
];

// Gate-style checks: they report `fail` until a human/team action (not a CI
// fix) flips them — currently the core-prompt review gate. No code change
// turns these green. `ci status` tags them `🔒 needs-approval` and excludes
// them from the failure tally (so a clean PR doesn't read as failing on a
// gate alone); `ci failures` reports them separately instead of trying to
// fetch fixable logs that don't exist. Without this, `ci status` (which shows
// the gate) and `ci failures` (which drops it via [IGNORED_CHECKS]) contradict.
const GATE_CHECKS: &[&str] = &[CORE_PROMPT_REVIEW, HIGH_RISK_APPROVAL];

fn is_gate_check(name: &str) -> bool {
    GATE_CHECKS.contains(&name)
}

// Ignore-list entries ending in `*` match by prefix; everything else matches
// exactly. Matrix-generated names like "Run relevant eval (<name>, <path>) /
// ..." vary per PR, so exact lists can never cover them.
fn is_ignored_check(name: &str, ignored: &[&str]) -> bool {
    ignored.iter().any(|entry| match entry.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => *entry == name,
    })
}

/// Drop "skipping" rows from `gh pr checks` output. The agent doesn't need
/// them in its CI temp file — they're already counted separately.
fn strip_skipping(out: &str) -> String {
    out.lines()
        .filter(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            parts.get(1).map(|s| s.trim()) != Some("skipping")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_checks(out: &str, ignored: &[&str]) -> CheckCounts {
    let mut checks: HashMap<&str, (u64, &str)> = HashMap::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            let name = parts[0].trim();
            if is_ignored_check(name, ignored) {
                continue;
            }
            let status = parts[1].trim();
            let run_id = if parts.len() >= 4 {
                run_id_from_url(parts[3])
            } else {
                0
            };
            if checks.get(name).is_none_or(|prev| run_id > prev.0) {
                checks.insert(name, (run_id, status));
            }
        }
    }

    let mut counts = CheckCounts {
        passed: 0,
        failed: 0,
        pending: 0,
        skipping: 0,
        pending_names: Vec::new(),
        cancelled_names: Vec::new(),
        stale: 0,
    };
    for (name, &(_, status)) in &checks {
        match status {
            "pass" => counts.passed += 1,
            "fail" => counts.failed += 1,
            "skipping" => counts.skipping += 1,
            _ => {
                counts.pending += 1;
                counts.pending_names.push((*name).to_string());
            }
        }
    }
    counts.pending_names.sort();
    counts
}

async fn get_ci_start_epoch(branch: &str, head_sha: &str) -> f64 {
    let out = sh(&format!(
        "gh run list --branch {branch} --limit 10 \
         --json startedAt,headSha \
         --jq '[.[] | select(.headSha == \"{head_sha}\") | .startedAt] | min'"
    ))
    .await;

    if let Some(s) = out.filter(|s| s != "null") {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
            return dt.timestamp() as f64;
        }
    }
    now_epoch()
}

async fn get_changed_files(base_ref: &str) -> Vec<String> {
    sh(&format!("git diff --name-only {base_ref}...HEAD"))
        .await
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default()
}

fn start_local_lints(changed_dirs: &std::collections::HashSet<&str>) -> Vec<(String, Child)> {
    let mut linters = Vec::new();
    if changed_dirs.contains("go") {
        linters.push(("lint-go".into(), sh_bg("lint-go")));
    }
    if changed_dirs.contains("app") {
        linters.push((
            "lint-web".into(),
            sh_bg("cd app && pnpm install --silent && lint-web"),
        ));
    }
    linters
}

async fn poll_linters(linters: Vec<(String, Child)>) -> (Vec<(String, Child)>, Vec<LintResult>) {
    let mut running = Vec::new();
    let mut finished = Vec::new();
    for (name, mut proc) in linters {
        match proc.try_wait() {
            Ok(Some(status)) => {
                let out = proc.wait_with_output().await.ok();
                let (stdout, stderr) = out
                    .map(|o| {
                        (
                            String::from_utf8_lossy(&o.stdout).trim().to_string(),
                            String::from_utf8_lossy(&o.stderr).trim().to_string(),
                        )
                    })
                    .unwrap_or_default();
                finished.push(LintResult {
                    name,
                    code: status.code().unwrap_or(1),
                    stdout,
                    stderr,
                });
            }
            _ => running.push((name, proc)),
        }
    }
    (running, finished)
}

fn format_ci_status(counts: &CheckCounts, ci_start: f64, local_running: usize) -> String {
    let mins = ((now_epoch() - ci_start) / 60.0) as u64;
    let mut status = String::new();
    if counts.passed > 0 {
        status += &format!("  ✅ {} passed", counts.passed);
    }
    if counts.failed > 0 {
        status += &format!("  ❌ {} failed", counts.failed);
    }
    if counts.pending > 0 || local_running > 0 {
        let mut parts = Vec::new();
        if counts.pending > 0 {
            parts.push(format!("{} in cloud", counts.pending));
        }
        if local_running > 0 {
            parts.push(format!("{local_running} local"));
        }
        status += &format!("  ⏳ {}", parts.join(" + "));
        if !counts.pending_names.is_empty() && counts.pending_names.len() <= 2 {
            status += &format!(" ({})", counts.pending_names.join(", "));
        }
    }
    if !counts.cancelled_names.is_empty() {
        status += &format!("  🔁 {} cancelled", counts.cancelled_names.len());
    }
    if counts.stale > 0 {
        status += &format!("  🔁 {} stale", counts.stale);
    }
    if counts.skipping > 0 {
        status += &format!("  ⏭️ {} skipped", counts.skipping);
    }
    format!("  [{mins}m] {}", status.trim())
}

async fn wait_for_ci(
    pr_number: &str,
    branch: &str,
    first_check: Option<(CheckCounts, i32, String)>,
    base_ref: &str,
) -> CiWaitResult {
    println!("   Waiting for CI checks...");
    let head_sha = sh("git rev-parse HEAD").await.unwrap_or_default();

    let (mut counts, mut _check_rc, mut out) = if let Some((c, rc, o)) = first_check {
        (c, rc, o)
    } else {
        let r = sh3(&format!("gh pr checks {pr_number}")).await;
        let mut c = parse_checks(&r.stdout, IGNORED_CHECKS);
        let observed = c.passed + c.failed + c.pending + c.skipping;
        if r.code != 0 && c.failed == 0 && observed == 0 {
            c.pending = c.pending.max(1);
        }
        (c, r.code, r.stdout)
    };

    // Verify failures are for current HEAD
    if counts.failed > 0 {
        let has_head_failure = sh(&format!(
            "gh run list --branch {branch} --status failure --limit 5 \
             --json headSha --jq '[.[] | select(.headSha == \"{head_sha}\")] | length'"
        ))
        .await;
        if has_head_failure.as_deref().unwrap_or("0") == "0" {
            counts.pending += counts.failed;
            counts.failed = 0;
        }
    }

    // Start local linters if CI pending
    let mut linters = Vec::new();
    let mut lint_files = Vec::new();
    let mut lint_results: Vec<LintResult> = Vec::new();
    if counts.pending > 0 && counts.failed == 0 {
        let changed = get_changed_files(base_ref).await;
        let changed_dirs: std::collections::HashSet<&str> =
            changed.iter().filter_map(|f| f.split('/').next()).collect();
        linters = start_local_lints(&changed_dirs);
        if !linters.is_empty() {
            let names: Vec<_> = linters.iter().map(|(n, _)| n.as_str()).collect();
            println!("   Running locally: {}", names.join(", "));
        }
    }

    let rc;
    if counts.failed > 0 {
        println!(
            "   ❌ {} failed, ✅ {} passed",
            counts.failed, counts.passed
        );
        rc = 1;
    } else if counts.pending == 0 {
        println!("   ✅ {} passed", counts.passed);
        rc = 0;
    } else {
        let ci_start = get_ci_start_epoch(branch, &head_sha).await;
        let mut prev_line = String::new();
        let mut lint_failed = false;

        rc = loop {
            let line = format_ci_status(&counts, ci_start, linters.len());
            if line != prev_line {
                print!("\r{line}    ");
                std::io::stdout().flush().ok();
                prev_line = line;
            }

            if counts.failed > 0 {
                println!();
                break 1;
            }
            if counts.pending == 0 {
                println!();
                break 0;
            }

            // Check local linters
            if !linters.is_empty() {
                let (still_running, finished) = poll_linters(linters).await;
                linters = still_running;
                for lr in &finished {
                    if lr.code != 0 {
                        lint_failed = true;
                        println!("\n   ❌ {} failed locally", lr.name);
                    }
                }
                lint_results.extend(finished);
            }

            if lint_failed {
                println!("   Skipping remaining CI wait — local lint failures to fix first.");
                break 1;
            }

            sleep(std::time::Duration::from_secs(15)).await;
            let r = sh3(&format!("gh pr checks {pr_number}")).await;
            counts = parse_checks(&r.stdout, IGNORED_CHECKS);
            _check_rc = r.code;
            out = r.stdout;
            let observed = counts.passed + counts.failed + counts.pending + counts.skipping;
            if _check_rc != 0 && counts.failed == 0 && observed == 0 {
                counts.pending = counts.pending.max(1);
            }
        };
    }

    // Kill remaining linters
    for (_, mut proc) in linters {
        proc.kill().await.ok();
        proc.wait().await.ok();
    }

    // Write lint result files
    for lr in &lint_results {
        if lr.code != 0 {
            let mut content = format!("# Local Lint: {}\n\nExit code: {}\n", lr.name, lr.code);
            if !lr.stdout.is_empty() {
                content += &format!("```\n{}\n```\n", lr.stdout);
            }
            if !lr.stderr.is_empty() {
                content += &format!("Stderr:\n```\n{}\n```\n", lr.stderr);
            }
            lint_files.push(section("lint", &content));
        }
    }

    let mut ci_content = format!("# CI Checks\n\nPR: #{pr_number}\nExit code: {rc}\n");
    if rc != 0 {
        ci_content += "Note: stopped at first failure; some checks may still be running.\n";
    }
    ci_content += &format!("```\n{}\n```\n", strip_skipping(&out));

    if rc == 0 {
        println!("✅ CI passed!");
        return CiWaitResult {
            ci_content,
            failures_content: None,
            failures_full_file: None,
            failed_names: vec![],
            lint_files,
        };
    }

    if lint_results.iter().any(|lr| lr.code != 0) {
        println!("❌ Local lint failures detected");
        return CiWaitResult {
            ci_content,
            failures_content: None,
            failures_full_file: None,
            failed_names: vec![],
            lint_files,
        };
    }

    println!("❌ CI failures detected");
    let _ = branch; // branch no longer needed; failure list comes from `gh pr checks --json`
    let logs = collect_failure_logs(pr_number, &head_sha).await;
    CiWaitResult {
        ci_content,
        failures_content: Some(logs.content),
        failures_full_file: logs.full_file,
        failed_names: logs.names,
        lint_files,
    }
}

// ── Failure logs ─────────────────────────────────────────────────────────────

fn extract_failure_summary(log: &str) -> String {
    let re = Regex::new(
        r"(?i)FAIL|--- FAIL|panic:|Error:|error:|ERROR|fatal:|undefined:|cannot |could not |timed out|exit status"
    ).unwrap();

    let lines: Vec<&str> = log
        .lines()
        .map(|line| line.splitn(4, '\t').last().unwrap_or(line).trim_end())
        .collect();

    const CONTEXT: usize = 3;
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let lo = i.saturating_sub(CONTEXT);
            let hi = (i + CONTEXT + 1).min(lines.len());
            for slot in &mut keep[lo..hi] {
                *slot = true;
            }
        }
    }

    let mut out: Vec<&str> = Vec::new();
    let mut prev_kept = false;
    for (i, &k) in keep.iter().enumerate() {
        if k {
            if !prev_kept && !out.is_empty() {
                out.push("--");
            }
            out.push(lines[i]);
            prev_kept = true;
        } else {
            prev_kept = false;
        }
    }
    out.join("\n")
}

const DISTILL_INSTRUCTIONS: &str = r#"You are given CI failure logs collected from a GitHub PR. Extract the most relevant information so a developer can quickly understand what failed and why.

# Output rules

- Find the error(s) and the most relevant context and output them verbatim with light summarization to skip irrelevant details.
- Output markdown. Do NOT output JSON. Do NOT wrap your output in a fenced code block.
- Open with a one-line headline naming the error. If failures fall into multiple distinct buckets, list each bucket on its own line.
- Distinguish infrastructure failures (runner outages, network issues) from PR-introduced failures (compile errors, failing tests, lint, type errors).
- Group similar failures across checks. Example: "5 cases: use slices.Sort instead of manual sort (go modernization lint)".
- For each unique failure bucket, include the exact error line that pinpoints the problem. Strip timestamps, runner names, and any boilerplate. Just the error itself.
- If a check has a real PR-introduced failure (e.g. type errors, failing tests), list each distinct error once with file:line:col when available.
- DO NOT write a root cause analysis or remediation advice. NO sentences like "Fix the code above", "Remove the unused field", "Suggested next steps", "Check your network", or any "this means / this is because" inference. Just surface the raw failure lines verbatim and stop. The developer will decide what to do.
- If a test failed across multiple re-runs, surface EVERY distinct FAIL line (one per attempt) and the final summary line (e.g. `2 runs, 32693 tests, 198 skipped, 2 failures`). Don't show only the first attempt.
- Some checks are pure aggregators of other checks (names ending in `-result`, or whose only failure line is `echo "X failed"` / `Process completed with exit code 1`). Label these as `(aggregator of: X, Y)` and don't repeat the underlying errors — they're already covered by the upstream check.
- Drop "Set up job", "Prepare workflow", "Download action repository", and other setup noise unless it IS the failure.
- Keep total output under ~60 lines. Be concise. The developer has access to the full log if they really need it.

# Example

## Good

'''
## Infrastructure: GitHub Actions download failures (codeload.github.com)
- 11 checks: Failed to download `actions/checkout@v4` (codeload.github.com)
- 2 checks: Failed to download `docker/setup-qemu-action@v3` (codeload.github.com)

All checks fail with a similar error:
```
An action could not be found at the URI 'https://codeload.github.com/[action]/tar.gz/[hash]'
Failed to download archive after 1 attempts.
```

Affected jobs: test-firestore, Build & Deploy Preview, label-pr
'''

## Bad

'''
## GitHub Actions download failures (codeload.github.com)

Root cause: check your internet connection. The URL 'https://codeload.github.com/[action]/tar.gz/[hash]' cannot be downloaded.

Suggested next steps: check network connectivity using ping.
'''

## Also bad (remediation drift — forbidden)

'''
## Go linting failures (unused code)

go/api/integrations/security/tools/integration.go:28:6: type securityDeps is unused (unused)
go/api/integrations/security/tools/integration.go:35:2: field deps is unused (unused)

Affected jobs: lint-go, lint-go-result (aggregator of: lint-go)

Result: Fix the code issues above and remove the unused type, then the lint checks will pass.
'''

The "Result:" sentence is exactly what to AVOID — just stop after the error lines and affected-jobs list.
"#;

/// Hardcoded fallback so dragonfly picks up lovable's `kit` even when
/// it isn't on PATH. PATH lookup is still tried first so the user can override.
const KIT_FALLBACK_PATH: &str = "/Users/arong/projects/lovable/lovable/bin/kit";

/// Call `kit llm` with a system prompt and a file containing the user
/// content. Tries `kit` on PATH first, then `KIT_FALLBACK_PATH`. Returns
/// `None` on spawn failure / nonzero exit / empty output.
///
/// Same-model benchmark showed kit's transport is 2-6x faster than the
/// claude CLI (claude --print has 15-35s of startup overhead even when
/// nothing else is happening); kit pays ~1s flat. Quality is identical
/// for `--print`-style runs that don't need tool use.
async fn call_kit_llm(
    model: &str,
    system_prompt: &str,
    content_file: &std::path::Path,
) -> Option<String> {
    let path_str = content_file.to_string_lossy();
    for bin in ["kit", KIT_FALLBACK_PATH] {
        let result = Command::new(bin)
            .args([
                "llm",
                "--model",
                model,
                "--system",
                system_prompt,
                "--file",
                &path_str,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        let Ok(out) = result else { continue };
        if !out.status.success() {
            continue;
        }
        let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if body.is_empty() {
            continue;
        }
        return Some(body);
    }
    None
}

/// Pipe a full failure-log dump through `model` (default Haiku 4.5) and
/// return a terse, deduped markdown summary. Skips the LLM entirely for
/// inputs under 5 KB. Returns `None` on invocation failure so callers
/// can fall back to the raw content.
///
/// The full untruncated log lives on disk at `full_log_path`; a footer
/// pointing back at it is appended below.
async fn distill_failure_logs(
    raw: &str,
    full_log_path: &std::path::Path,
    model: Model,
) -> Option<String> {
    let mut output = call_kit_llm(model.kit_id(), DISTILL_INSTRUCTIONS, full_log_path).await?;
    output.push_str(&format!(
        "\n\nFull untruncated log ({} bytes): {}\n",
        raw.len(),
        full_log_path.display()
    ));
    Some(output)
}

/// One failing check, sourced from `gh pr checks --json`. Covers every provider
/// surfaced as a commit status / check-run — GitHub Actions, Buildkite, Wiz,
/// Spacelift, custom statuses — not just GHA workflow runs.
#[derive(Deserialize, Debug, Clone)]
struct PrCheck {
    name: String,
    /// `pass`, `fail`, `pending`, `skipping`.
    bucket: String,
    /// Empty string when GitHub provides no link (rare but observed).
    #[serde(default)]
    link: String,
    /// Workflow filename (for GHA checks); empty for non-GHA providers.
    #[allow(dead_code)]
    #[serde(default)]
    workflow: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckProvider {
    GitHubActions,
    Buildkite,
    External,
}

fn classify_provider(link: &str) -> CheckProvider {
    if link.contains("github.com/") && link.contains("/actions/runs/") {
        CheckProvider::GitHubActions
    } else if link.contains("buildkite.com/") {
        CheckProvider::Buildkite
    } else {
        CheckProvider::External
    }
}

/// `link` like `https://github.com/<owner>/<repo>/actions/runs/<run_id>[/job/<job_id>]`.
/// Returns (run_id, optional job_id). Falls back to last numeric segment.
fn parse_gha_link(link: &str) -> (u64, Option<u64>) {
    let job = Regex::new(r"/job/(\d+)")
        .unwrap()
        .captures(link)
        .and_then(|c| c.get(1)?.as_str().parse().ok());
    let run = Regex::new(r"/actions/runs/(\d+)")
        .unwrap()
        .captures(link)
        .and_then(|c| c.get(1)?.as_str().parse().ok())
        .unwrap_or_else(|| run_id_from_url(link));
    (run, job)
}

/// `link` like `https://buildkite.com/<org>/<pipeline>/builds/<n>`. Returns
/// (org, pipeline, build_number) when parsable.
fn parse_buildkite_link(link: &str) -> Option<(String, String, u64)> {
    let re = Regex::new(r"buildkite\.com/([^/]+)/([^/]+)/builds/(\d+)").unwrap();
    let c = re.captures(link)?;
    Some((
        c.get(1)?.as_str().into(),
        c.get(2)?.as_str().into(),
        c.get(3)?.as_str().parse().ok()?,
    ))
}

async fn list_failed_checks(pr_number: &str) -> Vec<PrCheck> {
    let r = sh3(&format!(
        "gh pr checks {pr_number} --json name,bucket,link,workflow,description"
    ))
    .await;
    let mut all: Vec<PrCheck> = parse_json(&r.stdout).unwrap_or_default();
    all.retain(|c| c.bucket == "fail" && !is_ignored_check(&c.name, IGNORED_CHECKS));
    all
}

/// Fetch the failing job's log via `gh run view`. Prefer per-job mode when the
/// check link points at a specific job; that avoids dumping every failing job
/// in a workflow when only one is the target.
async fn fetch_gha_log(check: &PrCheck) -> String {
    let (run_id, job_id) = parse_gha_link(&check.link);
    let cmd = match job_id {
        Some(j) => format!("gh run view --job={j} --log"),
        None if run_id != 0 => format!("gh run view {run_id} --log"),
        _ => return String::new(),
    };
    let log = sh3(&cmd).await;
    let raw = if !log.stdout.is_empty() {
        log.stdout
    } else {
        log.stderr
    };
    denoise_gha_log(&raw)
}

/// Drop `git fetch`/checkout noise from a GHA job log.
///
/// `actions/checkout` with `fetch-depth: 0` prints one `[new branch] <name> ->
/// origin/<name>` line per remote ref plus a flood of `Updating files: N%`
/// progress lines. On a repo with thousands of branches this is the bulk of the
/// log and breaks failure surfacing two ways: it front-loads the log so a
/// byte-prefix truncation never reaches the failing step, and hundreds of
/// branch names contain substrings like "error"/"fail" that seed
/// [extract_failure_summary]'s keep window, inflating its output past 100 KB.
fn denoise_gha_log(log: &str) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(concat!(
            r"\[(new (branch|tag|ref)|tag update|deleted)\]",
            r"|-> origin/\S+( \(forced update\))?$",
            r"|\b(Updating files|Receiving objects|Resolving deltas|Compressing objects",
            r"|Counting objects|Enumerating objects|Filtering content|Checking out files): +\d",
        ))
        .unwrap()
    });
    log.lines()
        .filter(|line| !re.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Deserialize)]
struct GhJob {
    steps: Option<Vec<GhJobStep>>,
}
#[derive(Deserialize)]
struct GhJobStep {
    name: String,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
}

/// Compact per-step conclusions for a GHA job, e.g.
/// `Step conclusions: "Run ./test"=success, "Test Report"=failure`.
///
/// This is the signal that distinguishes "the test reran and passed; only the
/// junit report-publish step is red" (flaky, dorny/test-reporter with
/// fail-on-error) from a real test failure — a distinction invisible in the
/// flat log text the distiller otherwise sees. Cheap (a few lines) so it's
/// safe to prepend to the distiller input alongside the regex-trimmed log.
/// Empty when the check has no per-job link or the API returns no steps.
async fn fetch_gha_step_conclusions(check: &PrCheck) -> String {
    let (_run, job_id) = parse_gha_link(&check.link);
    let Some(job_id) = job_id else {
        return String::new();
    };
    let r = sh3(&format!(
        "gh api 'repos/{{owner}}/{{repo}}/actions/jobs/{job_id}'"
    ))
    .await;
    let Some(job) = parse_json::<GhJob>(&r.stdout) else {
        return String::new();
    };
    let steps = job.steps.unwrap_or_default();
    if steps.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = steps
        .iter()
        .map(|s| {
            let c = s
                .conclusion
                .as_deref()
                .filter(|c| !c.is_empty())
                .unwrap_or(if s.status.is_empty() { "?" } else { &s.status });
            format!("\"{}\"={}", s.name, c)
        })
        .collect();
    format!("Step conclusions: {}", parts.join(", "))
}

/// Buildkite logs require `BUILDKITE_API_TOKEN`. Without one, surface the URL +
/// any check-run output GitHub already stored, so the agent isn't left blind.
async fn fetch_buildkite_log(check: &PrCheck, head_sha: &str) -> String {
    let parsed = parse_buildkite_link(&check.link);
    let token = std::env::var("BUILDKITE_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    if let (Some((org, pipeline, number)), Some(tok)) = (parsed.clone(), token) {
        let api = format!(
            "https://api.buildkite.com/v2/organizations/{org}/pipelines/{pipeline}/builds/{number}?include_retried_jobs=true"
        );
        let r = sh3(&format!(
            "curl -sf -H 'Authorization: Bearer {tok}' {api:?}"
        ))
        .await;
        if r.code == 0 && !r.stdout.is_empty() {
            // Extract per-job logs for failing jobs.
            let logs = extract_buildkite_failed_logs(&r.stdout, &tok).await;
            if !logs.is_empty() {
                return logs;
            }
        }
    }

    // Fallback: GitHub's check-run output for this name, plus the link.
    let mut parts = Vec::new();
    if let Some((_, _, number)) = parsed {
        parts.push(format!("Buildkite build #{number}: {}", check.link));
    } else {
        parts.push(format!("Buildkite check: {}", check.link));
    }
    if !check.description.is_empty() {
        parts.push(check.description.clone());
    }
    let cr = fetch_check_run_output(head_sha, &check.name).await;
    if !cr.is_empty() {
        parts.push(cr);
    }
    if std::env::var("BUILDKITE_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .is_none()
    {
        parts.push(
            "(Set BUILDKITE_API_TOKEN to fetch full Buildkite logs. \
             Otherwise open the URL above.)"
                .into(),
        );
    }
    parts.join("\n")
}

#[derive(Deserialize)]
struct BkBuild {
    #[allow(dead_code)]
    number: u64,
    jobs: Option<Vec<BkJob>>,
}
#[derive(Deserialize)]
struct BkJob {
    id: Option<String>,
    name: Option<String>,
    state: Option<String>,
    exit_status: Option<i64>,
    raw_log_url: Option<String>,
}

async fn extract_buildkite_failed_logs(build_json: &str, token: &str) -> String {
    let build: BkBuild = match serde_json::from_str(build_json) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let mut out = Vec::new();
    for j in build.jobs.into_iter().flatten() {
        let failed =
            j.state.as_deref() == Some("failed") || j.exit_status.map(|e| e != 0).unwrap_or(false);
        if !failed {
            continue;
        }
        let name = j.name.clone().unwrap_or_else(|| "unnamed".into());
        let log = if let Some(url) = j.raw_log_url.as_ref() {
            let r = sh3(&format!(
                "curl -sf -H 'Authorization: Bearer {token}' {url:?}"
            ))
            .await;
            if r.code == 0 { r.stdout } else { String::new() }
        } else {
            String::new()
        };
        let summary = extract_failure_summary(&log);
        out.push(format!(
            "### Buildkite job: {name} (id={})\nExit: {}\n```\n{}\n```\n",
            j.id.unwrap_or_default(),
            j.exit_status
                .map(|e| e.to_string())
                .unwrap_or_else(|| "?".into()),
            if summary.is_empty() {
                truncate(&log, 4000).to_string()
            } else {
                summary
            },
        ));
    }
    out.join("\n")
}

/// `gh api repos/.../check-runs` returns `output.title` / `output.summary` /
/// `output.text` for many providers (Buildkite, Wiz, Spacelift). Use it as a
/// fallback so the agent always gets *something* even when we can't fetch the
/// provider's full log.
async fn fetch_check_run_output(head_sha: &str, name: &str) -> String {
    if head_sha.is_empty() {
        return String::new();
    }
    let r = sh3(&format!(
        "gh api 'repos/{{owner}}/{{repo}}/commits/{head_sha}/check-runs?per_page=100' \
         --jq '.check_runs[] | select(.name == \"{}\")'",
        name.replace('"', "\\\"")
    ))
    .await;
    if r.code != 0 || r.stdout.is_empty() {
        return String::new();
    }
    // Take the first check-run if there are multiple (retries).
    let first_obj = r.stdout.split("\n}\n{").next().unwrap_or(&r.stdout);
    let parsed: serde_json::Value = match serde_json::from_str(first_obj) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let mut parts = Vec::new();
    for k in ["title", "summary", "text"] {
        if let Some(s) = parsed
            .pointer(&format!("/output/{k}"))
            .and_then(|v| v.as_str())
        {
            if !s.is_empty() {
                parts.push(format!("**{k}**: {}", truncate(s, 2000)));
            }
        }
    }
    parts.join("\n")
}

async fn fetch_external_log(check: &PrCheck, head_sha: &str) -> String {
    let mut parts = vec![format!(
        "External check ({}): {}",
        classify_provider_label(&check.link),
        check.link
    )];
    if !check.description.is_empty() {
        parts.push(check.description.clone());
    }
    let cr = fetch_check_run_output(head_sha, &check.name).await;
    if !cr.is_empty() {
        parts.push(cr);
    }
    parts.join("\n")
}

fn classify_provider_label(link: &str) -> &'static str {
    if link.contains("buildkite.com/") {
        "buildkite"
    } else if link.contains("spacelift.io") {
        "spacelift"
    } else if link.contains("wiz.io") {
        "wiz"
    } else if link.contains("mintlify.com") {
        "mintlify"
    } else if link.contains("depthfirst.com") {
        "depthfirst"
    } else if link.contains("github.com") {
        "github"
    } else {
        "unknown"
    }
}

fn strip_ansi(s: &str) -> String {
    // GitHub Actions' log API returns ESC bytes rendered as literal "^[" pairs;
    // normalize back to real ESC so strip_ansi_escapes handles them.
    let normalized = s.replace("^[", "\x1b");
    let stripped = strip_ansi_escapes::strip(normalized.as_bytes());
    String::from_utf8(stripped).unwrap_or(normalized)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Slice at a UTF-8 boundary.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Keep the last `max` bytes of `s` at a UTF-8 boundary, marking the cut.
///
/// CI logs put the failure and the trailing `##[error]Process completed` line
/// at the end, so when a log overflows the cap the tail is the part worth
/// keeping; the head holds only setup/checkout noise a head-anchored
/// [truncate] would preserve instead.
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…[earlier log truncated]…\n{}", &s[start..])
}

/// Collect per-check failure logs across all providers. Replaces the old
/// GHA-only run-list path. Falls back to a synthetic note for any check we
/// can't fetch a real log for — never returns empty when there are failures.
async fn collect_failure_logs(pr_number: &str, head_sha: &str) -> FailureLogs {
    println!("   Collecting failure logs...");
    let failed = list_failed_checks(pr_number).await;
    let mut summaries = Vec::new();
    let mut full_logs = Vec::new();
    let mut names = Vec::new();

    for check in &failed {
        let provider = classify_provider(&check.link);
        let provider_label = classify_provider_label(&check.link);
        println!(
            "      Fetching log for {} ({})...",
            check.name, provider_label
        );
        let (raw, steps) = if matches!(provider, CheckProvider::GitHubActions) {
            tokio::join!(fetch_gha_log(check), fetch_gha_step_conclusions(check))
        } else {
            let raw = match provider {
                CheckProvider::Buildkite => fetch_buildkite_log(check, head_sha).await,
                _ => fetch_external_log(check, head_sha).await,
            };
            (raw, String::new())
        };
        let raw = strip_ansi(&raw);
        names.push(check.name.clone());
        let summary = if matches!(provider, CheckProvider::GitHubActions) {
            extract_failure_summary(&raw)
        } else {
            // For non-GHA, the "raw" text is already a curated summary.
            raw.clone()
        };
        let header = format!("### {} ({})", check.name, provider_label);
        let link_line = if check.link.is_empty() {
            String::new()
        } else {
            format!("\nLink: {}", check.link)
        };
        // Prepend per-step conclusions so flaky-reran-and-passed is
        // distinguishable from a real failure; see [fetch_gha_step_conclusions].
        let step_line = if steps.is_empty() {
            String::new()
        } else {
            format!("\n{steps}")
        };
        summaries.push(format!(
            "{header}{link_line}{step_line}\n```\n{}\n```",
            if summary.trim().is_empty() {
                "(no extracted error lines)"
            } else {
                summary.trim()
            }
        ));
        if matches!(provider, CheckProvider::GitHubActions) && !raw.is_empty() {
            let body = truncate_tail(&raw, 16000);
            full_logs.push(format!("## {} — full log\n```\n{}\n```", check.name, body));
        }
    }

    if failed.is_empty() {
        // Defensive: gh pr checks --json returned nothing failed but caller
        // believed there were failures. Don't leave the file empty — point at
        // the live `gh pr checks` output.
        summaries.push(
            "(no failing checks reported by `gh pr checks --json`; \
                        run `dragonfly ci status` to investigate)"
                .into(),
        );
    }

    let raw_content = format!(
        "# CI Failure Logs\n\n## Error Summary\n\n{}\n\n---\n\n# Full Logs\n\n{}\n",
        summaries.join("\n\n"),
        full_logs.join("\n\n"),
    );

    // For non-trivial logs, pipe through haiku and replace the bulk dump
    // with a distilled summary that points at the full log on disk.
    if raw_content.len() >= 1000 {
        let full_file = section("failures-full", &raw_content);
        println!(
            "   Distilling failure logs ({} bytes) via LLM...",
            raw_content.len()
        );
        let distill_start = std::time::Instant::now();
        match distill_failure_logs(&raw_content, &full_file.path, Model::Haiku).await {
            Some(distilled) => {
                println!(
                    "   Distilled in {:.1}s ({} bytes → {} bytes).",
                    distill_start.elapsed().as_secs_f64(),
                    raw_content.len(),
                    distilled.len(),
                );
                return FailureLogs {
                    content: distilled,
                    names,
                    full_file: Some(full_file),
                };
            }
            None => {
                println!("   Distill failed; falling back to raw failure logs.");
            }
        }
    }

    FailureLogs {
        content: raw_content,
        names,
        full_file: None,
    }
}

// ── CI subcommands ───────────────────────────────────────────────────────────

async fn resolve_pr_number(pr: Option<String>) -> Option<String> {
    if let Some(p) = pr {
        return Some(p);
    }
    let s = sh("gh pr view --json number --jq '.number'").await?;
    if s.is_empty() { None } else { Some(s) }
}

async fn ci_status_cmd(pr: Option<String>, all: bool, ignored: &[&str]) -> i32 {
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    let checks_cmd =
        format!("gh pr checks {pr_number} --json name,bucket,link,workflow,description");
    let (r, has_conflicts) = tokio::join!(sh3(&checks_cmd), check_origin_main_conflicts(),);
    let mut checks: Vec<PrCheck> = parse_json(&r.stdout).unwrap_or_default();
    checks.retain(|c| !is_ignored_check(&c.name, ignored));
    // Dedup by name, keeping the highest-priority bucket: fail > pending > pass > skipping.
    let priority = |b: &str| match b {
        "fail" => 3,
        "pending" => 2,
        "pass" => 1,
        _ => 0,
    };
    checks.sort_by(|a, b| priority(&b.bucket).cmp(&priority(&a.bucket)));
    let mut seen = std::collections::HashSet::new();
    checks.retain(|c| seen.insert(c.name.clone()));
    if !all {
        checks.retain(|c| c.bucket == "fail" || c.bucket == "pending");
    }
    checks.sort_by(|a, b| (priority(&b.bucket), &a.name).cmp(&(priority(&a.bucket), &b.name)));

    let mut failed = 0;
    let mut pending = 0;
    let mut passed_total = 0;
    let mut skipped_total = 0;
    let mut gates = 0;
    // Always count totals from full set.
    let all_checks: Vec<PrCheck> = parse_json(&r.stdout).unwrap_or_default();
    let mut by_name: HashMap<String, &PrCheck> = HashMap::new();
    for c in all_checks
        .iter()
        .filter(|c| !is_ignored_check(&c.name, ignored))
    {
        // Keep the highest-priority row per name.
        let keep = by_name
            .get(&c.name)
            .map(|p| priority(&c.bucket) > priority(&p.bucket))
            .unwrap_or(true);
        if keep {
            by_name.insert(c.name.clone(), c);
        }
    }
    for c in by_name.values() {
        // A failing gate is awaiting human approval, not a fixable CI failure;
        // count it separately so the exit code and `fail` tally stay clean.
        if c.bucket == "fail" && is_gate_check(&c.name) {
            gates += 1;
            continue;
        }
        match c.bucket.as_str() {
            "fail" => failed += 1,
            "pending" => pending += 1,
            "pass" => passed_total += 1,
            _ => skipped_total += 1,
        }
    }

    let gate_suffix = if gates > 0 {
        format!(", {gates} needs-approval")
    } else {
        String::new()
    };
    println!(
        "PR #{pr_number} — {} fail, {} pending, {} pass, {} skip{gate_suffix}",
        failed, pending, passed_total, skipped_total
    );
    if has_conflicts {
        println!("⚠️  Merge conflicts with origin/main — some checks did not run.");
    }
    for c in &checks {
        let is_gate = c.bucket == "fail" && is_gate_check(&c.name);
        let icon = if is_gate {
            "🔒"
        } else {
            match c.bucket.as_str() {
                "fail" => "❌",
                "pending" => "⏳",
                "pass" => "✅",
                _ => "⏭",
            }
        };
        let provider = if is_gate {
            "needs-approval"
        } else {
            classify_provider_label(&c.link)
        };
        // The gate's commit-status description (e.g. "awaiting @lovablelabs/ai
        // approval") is the actionable bit, not the link.
        let trailer = if is_gate && !c.description.is_empty() {
            format!("  — {}", c.description)
        } else if c.link.is_empty() {
            String::new()
        } else {
            format!("  {}", c.link)
        };
        println!("{icon} [{provider:>14}] {}{trailer}", c.name);
    }
    // Exit non-zero only on a real (fixable) failure — a pending approval gate
    // is not something the agent can or should "fix".
    if failed > 0 { 1 } else { 0 }
}

/// Failing gate-style checks (name + commit-status description), e.g. the
/// core-prompt review gate "awaiting @lovablelabs/ai approval". Surfaced
/// separately by `ci failures` so a gate-only red PR doesn't read as
/// "No failing checks" while `ci status` shows a red gate.
async fn failing_gate_checks(pr_number: &str) -> Vec<PrCheck> {
    let r = sh3(&format!(
        "gh pr checks {pr_number} --json name,bucket,link,workflow,description"
    ))
    .await;
    let all: Vec<PrCheck> = parse_json(&r.stdout).unwrap_or_default();
    all.into_iter()
        .filter(|c| c.bucket == "fail" && is_gate_check(&c.name))
        .collect()
}

/// Print failing gate checks with their commit-status description, so the
/// agent learns *why* a red check has no fixable log rather than burning
/// round-trips on `gh run view`. A gate needs a human/team action, not a fix.
fn print_gate_note(gates: &[PrCheck]) {
    println!(
        "\n🔒 {} approval gate(s) red (not fixable by CI — needs a human/team action):",
        gates.len()
    );
    for g in gates {
        let desc = if g.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", g.description)
        };
        let link = if g.link.is_empty() {
            String::new()
        } else {
            format!("  {}", g.link)
        };
        println!("  - {}{desc}{link}", g.name);
    }
}

async fn ci_failures_cmd(
    pr: Option<String>,
    max_bytes: usize,
    raw_only: bool,
    model: Option<Model>,
) -> i32 {
    let model = model.unwrap_or(Model::Haiku);
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    let head_sha = sh("git rev-parse HEAD").await.unwrap_or_default();
    let (failed, gates) = tokio::join!(
        list_failed_checks(&pr_number),
        failing_gate_checks(&pr_number)
    );
    if failed.is_empty() {
        // A gate-only red PR is not a fixable failure — say so explicitly so
        // this agrees with `ci status` instead of reporting a bare "none"
        // while `ci status` shows a red gate.
        if gates.is_empty() {
            println!("No failing checks for PR #{pr_number}.");
        } else {
            println!("No fixable CI failures for PR #{pr_number}.");
            print_gate_note(&gates);
        }
        return 0;
    }

    // `buf` is the compact distiller input (per-step conclusions + regex-
    // trimmed errors). `full_buf` keeps the lightly-truncated raw logs for the
    // on-disk `-full` file the distiller footer points at, so "read the full
    // log" actually yields the log rather than the trimmed summary.
    let mut buf = String::new();
    let mut full_buf = String::new();
    let header = format!(
        "# Failing checks for PR #{pr_number} ({} total)\n\n",
        failed.len()
    );
    buf.push_str(&header);
    full_buf.push_str(&header);
    for check in &failed {
        let provider = classify_provider(&check.link);
        let provider_label = classify_provider_label(&check.link);
        buf.push_str(&format!("## {} ({})\n", check.name, provider_label));
        if !check.link.is_empty() {
            buf.push_str(&format!("Link: {}\n", check.link));
        }
        let (raw, steps) = if matches!(provider, CheckProvider::GitHubActions) {
            tokio::join!(fetch_gha_log(check), fetch_gha_step_conclusions(check))
        } else {
            let raw = match provider {
                CheckProvider::Buildkite => fetch_buildkite_log(check, &head_sha).await,
                _ => fetch_external_log(check, &head_sha).await,
            };
            (raw, String::new())
        };
        let raw = strip_ansi(&raw);
        // Per-step conclusions go in ahead of the trimmed log so the distiller
        // can tell a reran-and-passed test (only the publish step red) from a
        // real failure without the full log.
        if !steps.is_empty() {
            buf.push_str(&format!("{steps}\n"));
        }
        let body = if matches!(provider, CheckProvider::GitHubActions) {
            let s = extract_failure_summary(&raw);
            if s.is_empty() {
                truncate_tail(&raw, max_bytes)
            } else {
                s
            }
        } else {
            truncate(&raw, max_bytes).to_string()
        };
        if body.trim().is_empty() {
            buf.push_str("(no extracted error lines)\n\n");
        } else {
            buf.push_str(&format!("```\n{}\n```\n\n", body.trim()));
        }
        full_buf.push_str(&format!("## {} ({})\n", check.name, provider_label));
        if !steps.is_empty() {
            full_buf.push_str(&format!("{steps}\n"));
        }
        let full_body = truncate_tail(&raw, 16000);
        if full_body.trim().is_empty() {
            full_buf.push_str("(no log captured)\n\n");
        } else {
            full_buf.push_str(&format!("```\n{}\n```\n\n", full_body.trim()));
        }
    }

    if raw_only || buf.len() < 1000 {
        print!("{buf}");
        if !gates.is_empty() {
            print_gate_note(&gates);
        }
        return 1;
    }

    let full_file = section("ci-failures-full", &full_buf);
    eprintln!(
        "   Distilling failure logs ({} bytes) via {}...",
        buf.len(),
        model.kit_id()
    );
    let start = std::time::Instant::now();
    match distill_failure_logs(&buf, &full_file.path, model).await {
        Some(distilled) => {
            eprintln!(
                "   Distilled in {:.1}s ({} bytes → {} bytes).",
                start.elapsed().as_secs_f64(),
                buf.len(),
                distilled.len(),
            );
            println!("{distilled}");
        }
        None => {
            eprintln!("   Distill failed; falling back to raw failure logs.");
            print!("{buf}");
        }
    }
    if !gates.is_empty() {
        print_gate_note(&gates);
    }
    1
}

/// Resolve the commit `ci watch` anchors on: an explicit PR's head, or where
/// the current branch was last pushed. `git push` updates the remote-tracking
/// ref locally, so right after a push `@{push}` / `origin/<branch>` is exactly
/// the pushed commit — no fetch or PR lookup needed. Returns (sha, branch).
async fn resolve_watch_anchor(pr: Option<String>) -> Option<(String, String)> {
    if let Some(pr) = pr {
        let out = sh(&format!(
            "gh pr view {pr} --json headRefOid,headRefName \
             --jq '.headRefOid + \" \" + .headRefName'"
        ))
        .await?;
        let mut parts = out.split_whitespace();
        let sha = parts.next()?.to_string();
        return Some((sha, parts.next().unwrap_or_default().to_string()));
    }
    let branch = sh("git branch --show-current")
        .await
        .filter(|s| !s.is_empty())?;
    // `@{push}` is the configured push destination; fall back to the
    // conventional origin/<branch> when no upstream is configured.
    let sha = match sh("git rev-parse @{push} 2>/dev/null").await {
        Some(s) => s,
        None => sh(&format!("git rev-parse origin/{branch} 2>/dev/null")).await?,
    };
    Some((sha, branch))
}

/// What `ci watch` re-resolves each poll to notice a fresh push. A new push
/// supersedes (cancel-superseded) the watched commit's runs, so re-anchoring
/// onto the new head beats fail-fasting on the old head's cancelled jobs.
enum WatchAnchor {
    /// Re-resolve the local push destination: `@{push}` (which `git push`
    /// updates locally), falling back to `origin/<branch>`.
    Branch(String),
    /// Re-resolve the PR head via `gh pr view`.
    Pr(String),
}

impl WatchAnchor {
    async fn current_head(&self) -> Option<String> {
        match self {
            WatchAnchor::Branch(branch) => match sh("git rev-parse @{push} 2>/dev/null").await {
                Some(s) if !s.is_empty() => Some(s),
                _ => sh(&format!("git rev-parse origin/{branch} 2>/dev/null")).await,
            },
            WatchAnchor::Pr(pr) => {
                sh(&format!(
                    "gh pr view {pr} --json headRefOid --jq '.headRefOid'"
                ))
                .await
            }
        }
    }
}

async fn ci_watch_cmd(pr: Option<String>) -> i32 {
    // The conflict probe's `git fetch origin main` is the slowest startup
    // call and depends on nothing below — start it before anchor resolution
    // so it overlaps the gh round-trips.
    let conflicts = tokio::spawn(check_origin_main_conflicts());
    let explicit_pr = pr.is_some();
    let Some((sha, branch)) = resolve_watch_anchor(pr.clone()).await else {
        eprintln!(
            "Could not resolve a pushed commit to watch — push the branch first (or pass --pr)."
        );
        return 2;
    };
    let anchor = match pr {
        Some(p) => WatchAnchor::Pr(p),
        None => WatchAnchor::Branch(branch.clone()),
    };
    // The anchor is what was pushed, not HEAD; flag the gap so unpushed local
    // commits aren't silently mistaken for being under test.
    if !explicit_pr {
        let head = sh("git rev-parse HEAD").await.unwrap_or_default();
        if !head.is_empty() && head != sha {
            println!(
                "⚠️  HEAD ({}) is not what was pushed — watching pushed commit {}.",
                &head[..7.min(head.len())],
                &sha[..7.min(sha.len())]
            );
        }
    }
    // The CI start epoch and first poll only feed display; fetch them
    // alongside the conflict probe instead of after it.
    let (conflicts, ci_start, first_checks) = tokio::join!(
        conflicts,
        get_ci_start_epoch(&branch, &sha),
        checks_for_sha(&sha)
    );
    if conflicts.unwrap_or(false) {
        println!("⚠️  Merge conflicts with origin/main — some checks did not run.");
    }
    ci_watch_sha(anchor, sha, ci_start, first_checks).await
}

// ── SHA-anchored CI watch ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RestCheckRuns {
    check_runs: Vec<RestCheckRun>,
}
#[derive(Deserialize)]
struct RestCheckRun {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}
#[derive(Deserialize)]
struct RestCombinedStatus {
    #[serde(default)]
    statuses: Vec<RestStatus>,
}
#[derive(Deserialize)]
struct RestStatus {
    context: String,
    state: String,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Check runs + commit statuses for one specific commit, mapped into the same
/// name/bucket shape `gh pr checks --json` produces. Querying by SHA instead
/// of by PR is what lets `push -w` watch exactly what it pushed.
async fn checks_for_sha(sha: &str) -> Vec<PrCheck> {
    let runs_cmd =
        format!("gh api 'repos/{{owner}}/{{repo}}/commits/{sha}/check-runs?per_page=100'");
    // The combined-status endpoint covers providers that report commit
    // statuses rather than check runs (Buildkite, Spacelift, Graphite, …).
    let status_cmd = format!("gh api 'repos/{{owner}}/{{repo}}/commits/{sha}/status'");
    let (runs, statuses) = tokio::join!(sh3(&runs_cmd), sh3(&status_cmd));
    parse_sha_checks(&runs.stdout, &statuses.stdout)
}

fn parse_sha_checks(runs_json: &str, statuses_json: &str) -> Vec<PrCheck> {
    // A re-triggered workflow creates a new check suite with same-named runs;
    // keep only the newest run per name (mirrors [parse_checks]'s run-id dedup).
    let mut latest: HashMap<String, (u64, PrCheck)> = HashMap::new();
    if let Some(r) = parse_json::<RestCheckRuns>(runs_json) {
        for run in r.check_runs {
            let bucket = if run.status != "completed" {
                "pending"
            } else {
                match run.conclusion.as_deref() {
                    Some("success") => "pass",
                    Some("skipped") | Some("neutral") => "skipping",
                    // A queued run the scheduler abandoned because a newer run
                    // for the same head superseded it — never a code failure.
                    Some("stale") => "stale",
                    // Usually cancel-superseded after a re-trigger. Held apart
                    // from `fail` so `ci watch` can debounce instead of
                    // fail-fasting on a phantom; see [ci_watch_sha].
                    Some("cancelled") => "cancelled",
                    // failure, timed_out, action_required, …
                    _ => "fail",
                }
            };
            let check = PrCheck {
                name: run.name.clone(),
                bucket: bucket.into(),
                link: run.html_url.unwrap_or_default(),
                workflow: String::new(),
                description: String::new(),
            };
            if latest.get(&run.name).is_none_or(|(id, _)| run.id > *id) {
                latest.insert(run.name, (run.id, check));
            }
        }
    }
    let mut checks: Vec<PrCheck> = latest.into_values().map(|(_, c)| c).collect();

    if let Some(s) = parse_json::<RestCombinedStatus>(statuses_json) {
        for st in s.statuses {
            let bucket = match st.state.as_str() {
                "success" => "pass",
                "failure" | "error" => "fail",
                _ => "pending",
            };
            checks.push(PrCheck {
                name: st.context,
                bucket: bucket.into(),
                link: st.target_url.unwrap_or_default(),
                workflow: String::new(),
                description: st.description.unwrap_or_default(),
            });
        }
    }
    checks
}

fn counts_from_checks(checks: &[PrCheck], ignored: &[&str]) -> CheckCounts {
    let mut counts = CheckCounts {
        passed: 0,
        failed: 0,
        pending: 0,
        skipping: 0,
        pending_names: Vec::new(),
        cancelled_names: Vec::new(),
        stale: 0,
    };
    for c in checks
        .iter()
        .filter(|c| !is_ignored_check(&c.name, ignored))
    {
        match c.bucket.as_str() {
            "pass" => counts.passed += 1,
            "fail" => counts.failed += 1,
            "skipping" => counts.skipping += 1,
            "stale" => counts.stale += 1,
            "cancelled" => counts.cancelled_names.push(c.name.clone()),
            _ => {
                counts.pending += 1;
                counts.pending_names.push(c.name.clone());
            }
        }
    }
    counts.pending_names.sort();
    counts.cancelled_names.sort();
    counts
}

/// Print pre-fetched checks in the `ci status` shape (tally header, then one
/// line per failing/pending check). Returns 1 when any non-ignored check
/// failed, else 0.
fn print_checks_summary(label: &str, checks: &[PrCheck], ignored: &[&str]) -> i32 {
    let counts = counts_from_checks(checks, ignored);
    let mut header = format!(
        "{label} — {} fail, {} pending, {} pass, {} skip",
        counts.failed, counts.pending, counts.passed, counts.skipping
    );
    if !counts.cancelled_names.is_empty() {
        header += &format!(", {} cancelled", counts.cancelled_names.len());
    }
    if counts.stale > 0 {
        header += &format!(", {} stale", counts.stale);
    }
    println!("{header}");
    let priority = |b: &str| match b {
        "fail" => 4,
        "cancelled" => 3,
        "pending" => 2,
        "stale" => 1,
        _ => 0,
    };
    let mut shown: Vec<&PrCheck> = checks
        .iter()
        .filter(|c| !is_ignored_check(&c.name, ignored))
        .filter(|c| {
            matches!(
                c.bucket.as_str(),
                "fail" | "pending" | "cancelled" | "stale"
            )
        })
        .collect();
    shown.sort_by(|a, b| (priority(&b.bucket), &a.name).cmp(&(priority(&a.bucket), &b.name)));
    for c in shown {
        let icon = match c.bucket.as_str() {
            "fail" => "❌",
            "pending" => "⏳",
            "cancelled" | "stale" => "🔁",
            _ => "⏭",
        };
        let provider = classify_provider_label(&c.link);
        let link = if c.link.is_empty() {
            String::new()
        } else {
            format!("  {}", c.link)
        };
        println!("{icon} [{provider:>10}] {}{link}", c.name);
    }
    if counts.failed > 0 { 1 } else { 0 }
}

/// Poll one commit's checks until they settle, then print a final summary.
///
/// Terminal rules:
/// - A real failure (conclusion `failure`/`timed_out`/…) fail-fasts instantly.
/// - `cancelled` is debounced one extra poll: a cancellation is usually
///   cancel-superseded after a re-trigger, whose replacement runs register a
///   beat later. It only counts as red if the SAME cancelled set survives a
///   second poll without a newer run for those names superseding it. This is
///   the fix for `ci watch` fail-fasting on phantom superseded-run failures.
/// - `stale`/`skipping`/`pass` are non-blocking; the watch settles when
///   nothing is `pending` (and any `cancelled` set has settled).
///
/// Each poll also re-resolves [WatchAnchor] — a fresh push moves the head and
/// cancel-supersedes the old runs, so the watch re-anchors onto the new commit
/// ("Push detected…") instead of fail-fasting on the abandoned one.
///
/// Polls REST by SHA rather than `gh pr checks --watch`: gh's watch blocks
/// until every check reaches a terminal state, so the perpetually-`pending`
/// [GRAPHITE_MERGEABILITY] check would hang it forever, and the PR's head
/// can move under a watch while a fixed SHA cannot.
async fn ci_watch_sha(
    anchor: WatchAnchor,
    initial_sha: String,
    initial_ci_start: f64,
    first_checks: Vec<PrCheck>,
) -> i32 {
    const POLL: std::time::Duration = std::time::Duration::from_secs(15);
    let mut sha = initial_sha;
    let mut ci_start = initial_ci_start;
    let start = std::time::Instant::now();
    let mut prev_line = String::new();
    let mut checks = first_checks;
    // The cancelled set observed last poll, once everything else is terminal.
    // Re-seen unchanged ⇒ genuine cancellation; changed/superseded ⇒ reset.
    let mut cancel_settle: Option<Vec<String>> = None;

    loop {
        let counts = counts_from_checks(&checks, WATCH_IGNORED_CHECKS);
        let total = counts.passed
            + counts.failed
            + counts.pending
            + counts.skipping
            + counts.stale
            + counts.cancelled_names.len();
        let short = format!("Commit {}", &sha[..7.min(sha.len())]);

        // Checks take a few seconds to register after a push; an empty list
        // means "not started yet", never "all done".
        let line = if total == 0 {
            format!(
                "  [{}m] ⏳ waiting for checks to appear...",
                start.elapsed().as_secs() / 60
            )
        } else {
            format_ci_status(&counts, ci_start, 0)
        };
        if line != prev_line {
            print!("\r{line}    ");
            std::io::stdout().flush().ok();
            prev_line = line;
        }

        // Real failures fail-fast immediately — no debounce.
        if counts.failed > 0 {
            println!();
            return print_checks_summary(&short, &checks, WATCH_IGNORED_CHECKS);
        }

        // Nothing pending ⇒ candidate terminal state.
        if total > 0 && counts.pending == 0 {
            if counts.cancelled_names.is_empty() {
                println!();
                return print_checks_summary(&short, &checks, WATCH_IGNORED_CHECKS);
            }
            // Only cancelled checks remain. Settle them across one more poll.
            if cancel_settle.as_ref() == Some(&counts.cancelled_names) {
                println!();
                print_checks_summary(&short, &checks, WATCH_IGNORED_CHECKS);
                return 1;
            }
            cancel_settle = Some(counts.cancelled_names.clone());
        } else {
            cancel_settle = None;
        }

        sleep(POLL).await;

        // A new push supersedes the watched commit; follow it instead of
        // fail-fasting on the old head's cancel-superseded jobs.
        if let Some(new_sha) = anchor.current_head().await {
            if !new_sha.is_empty() && new_sha != sha {
                println!(
                    "\n🔄 Push detected. Restarting watch at #{}",
                    &new_sha[..7.min(new_sha.len())]
                );
                sha = new_sha;
                ci_start = now_epoch();
                cancel_settle = None;
                prev_line.clear();
            }
        }

        checks = checks_for_sha(&sha).await;
    }
}

async fn ci_flaky_cmd(name: String, limit: usize) -> i32 {
    let shas = sh(&format!("git log origin/main --format='%H' -{limit}"))
        .await
        .unwrap_or_default();
    if shas.is_empty() {
        eprintln!("Could not list commits on origin/main.");
        return 2;
    }
    let mut pass = 0;
    let mut fail = 0;
    let mut skip = 0;
    let mut other = 0;
    let mut rows = Vec::new();
    for sha in shas.lines() {
        let r = sh(&format!(
            "gh api 'repos/{{owner}}/{{repo}}/commits/{sha}/check-runs?per_page=100' \
             --jq '.check_runs[] | select(.name == \"{}\") | \"\\(.conclusion // .status) \\(.html_url)\"' 2>/dev/null | head -1",
            name.replace('"', "\\\"")
        )).await.unwrap_or_default();
        let mut parts = r.splitn(2, ' ');
        let conclusion = parts.next().unwrap_or("").to_string();
        let url = parts.next().unwrap_or("").trim().to_string();
        let conclusion = if conclusion.is_empty() {
            "no-run".to_string()
        } else {
            conclusion
        };
        match conclusion.as_str() {
            "success" => pass += 1,
            "failure" | "cancelled" | "timed_out" => fail += 1,
            "skipped" | "neutral" => skip += 1,
            "no-run" => skip += 1,
            _ => other += 1,
        }
        rows.push(format!(
            "{} {}{}",
            &sha[..7.min(sha.len())],
            conclusion,
            if url.is_empty() {
                String::new()
            } else {
                format!("  {url}")
            }
        ));
    }
    println!("Check `{name}` on last {limit} commits of origin/main:");
    println!("  ✅ {pass} pass    ❌ {fail} fail    ⏭ {skip} skip/none    ? {other} other\n");
    for row in &rows {
        println!("{row}");
    }
    let verdict = if pass + fail == 0 {
        "No data — this check doesn't run on main commits. Compare against other PRs instead."
    } else if fail == 0 {
        "Consistently passing on main — failure is likely caused by this PR."
    } else if pass == 0 {
        "Consistently failing on main — pre-existing issue, do not fix in this PR without confirmation."
    } else {
        "Mixed pass/fail on main — likely flaky. Consider rerunning."
    };
    println!("\nVerdict: {verdict}");
    0
}

#[derive(Deserialize)]
struct GhRun {
    #[serde(rename = "databaseId")]
    database_id: u64,
    name: String,
    #[serde(rename = "headSha")]
    head_sha: String,
    conclusion: Option<String>,
    status: String,
    attempt: u64,
    #[serde(rename = "createdAt")]
    created_at: String,
}

async fn ci_retries_cmd(pr: Option<String>) -> i32 {
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    let head_sha = sh(&format!(
        "gh pr view {pr_number} --json headRefOid --jq '.headRefOid'"
    ))
    .await
    .unwrap_or_default();
    let branch = sh(&format!(
        "gh pr view {pr_number} --json headRefName --jq '.headRefName'"
    ))
    .await
    .unwrap_or_default();
    if branch.is_empty() {
        eprintln!("Could not determine PR head branch.");
        return 2;
    }
    let r = sh3(&format!(
        "gh run list --branch {branch} --limit 50 \
         --json databaseId,name,headSha,conclusion,status,attempt,createdAt"
    ))
    .await;
    let mut runs: Vec<GhRun> = parse_json(&r.stdout).unwrap_or_default();
    runs.retain(|r| r.head_sha == head_sha);

    println!(
        "# Workflow runs for PR #{pr_number} (head {})",
        &head_sha[..7.min(head_sha.len())]
    );
    if runs.is_empty() {
        println!("(no GitHub Actions runs found for this head SHA)");
        return 0;
    }

    fn short_time(iso: &str) -> String {
        // "2026-05-25T09:25:18Z" -> "05-25 09:25"
        let mut chars = iso.chars();
        let date: String = chars.by_ref().skip(5).take(5).collect(); // "MM-DD"
        let _ = chars.next(); // 'T'
        let time: String = chars.take(5).collect(); // "HH:MM"
        format!("{date} {time}")
    }
    fn result_str(r: &GhRun) -> String {
        r.conclusion
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| r.status.clone())
    }

    // Sort: retried runs (attempt > 1) first, then by name, then by createdAt desc.
    runs.sort_by(|a, b| {
        b.attempt
            .cmp(&a.attempt)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    let name_w = runs.iter().map(|r| r.name.len()).max().unwrap_or(4).min(50);
    let result_w = runs.iter().map(|r| result_str(r).len()).max().unwrap_or(7);

    println!(
        "{:<name_w$}  {:>3}  {:<result_w$}  {:<11}  {}",
        "NAME", "ATT", "RESULT", "TIME", "RUN_ID"
    );
    for r in &runs {
        let marker = if r.attempt > 1 { " ← retried" } else { "" };
        println!(
            "{:<name_w$}  {:>3}  {:<result_w$}  {:<11}  {}{}",
            r.name,
            r.attempt,
            result_str(r),
            short_time(&r.created_at),
            r.database_id,
            marker
        );
    }

    let retried = runs.iter().filter(|r| r.attempt > 1).count();
    if retried > 0 {
        println!(
            "\n{retried} run(s) retried (attempt > 1). Avoid rerunning these without asking the user."
        );
    }
    0
}

async fn ci_rerun_cmd(name: String, pr: Option<String>) -> i32 {
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    // Resolve the check name to a run via the check's own link — the same
    // source `ci status`/`ci failures` report from — so the short check name
    // (`test-go`, `test-realtime`) matches even when its workflow has a
    // different display name (`Test`, `Test Go realtime`). Name-matching
    // against `gh run list`'s workflow names misses that mapping.
    let failed = list_failed_checks(&pr_number).await;
    if failed.is_empty() {
        eprintln!("No failing checks for PR #{pr_number} to rerun.");
        return 2;
    }
    // Accept both the short check name and the workflow display name, in
    // either direction, so callers can paste whichever `ci status` showed.
    let matches: Vec<&PrCheck> = failed
        .iter()
        .filter(|c| {
            c.name == name
                || c.name.eq_ignore_ascii_case(&name)
                || c.name.contains(&name)
                || name.contains(&c.name)
        })
        .collect();
    let target = matches
        .iter()
        .copied()
        .find(|c| c.name == name)
        .or_else(|| matches.first().copied());
    let Some(check) = target else {
        eprintln!("No failing check named `{name}`. Failing checks:");
        for c in &failed {
            eprintln!("  - {} [{}]", c.name, classify_provider_label(&c.link));
        }
        return 2;
    };
    if classify_provider(&check.link) != CheckProvider::GitHubActions {
        eprintln!(
            "`{}` is a {} check — only GitHub Actions runs can be rerun via this command.",
            check.name,
            classify_provider_label(&check.link)
        );
        return 2;
    }
    let (run_id, _job) = parse_gha_link(&check.link);
    if run_id == 0 {
        eprintln!(
            "Could not resolve a GitHub Actions run id from `{}`'s link: {}",
            check.name, check.link
        );
        return 2;
    }
    println!(
        "Re-running failed jobs in `{}` (run {run_id})...",
        check.name
    );
    let r = sh3(&format!("gh run rerun {run_id} --failed")).await;
    if !r.stdout.is_empty() {
        println!("{}", r.stdout);
    }
    if r.code != 0 {
        eprintln!("{}", r.stderr);
        return r.code;
    }
    0
}

async fn ci_distill_cmd(file: PathBuf, model: Option<Model>) -> i32 {
    let model = model.unwrap_or(Model::Haiku);
    let raw = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to read {}: {e}", file.display());
            return 2;
        }
    };
    if raw.len() < 1000 {
        eprintln!(
            "Input is only {} bytes; distill skipped (raw printed as-is).",
            raw.len()
        );
        print!("{raw}");
        return 0;
    }
    eprintln!(
        "   Distilling failure logs ({} bytes) via {}...",
        raw.len(),
        model.kit_id()
    );
    let start = std::time::Instant::now();
    match distill_failure_logs(&raw, &file, model).await {
        Some(distilled) => {
            eprintln!(
                "   Distilled in {:.1}s ({} bytes → {} bytes).",
                start.elapsed().as_secs_f64(),
                raw.len(),
                distilled.len(),
            );
            println!("{distilled}");
            0
        }
        None => {
            eprintln!("Distill failed.");
            1
        }
    }
}

// ── Merge conflict check ─────────────────────────────────────────────────────

const MERGE_TREE_PROBE_CMD: &str = "git merge-tree --write-tree --name-only origin/main HEAD";

async fn merge_tree_probe() -> ShResult {
    sh3(MERGE_TREE_PROBE_CMD).await
}

fn merge_probe_has_conflicts(r: &ShResult) -> bool {
    r.code != 0
}

/// Fetch `origin/main` then probe HEAD for merge conflicts with it.
/// Best-effort: a fetch or probe failure is reported as "no conflicts" so
/// callers don't false-alarm on missing remotes or transient network errors.
async fn check_origin_main_conflicts() -> bool {
    let _ = sh3("git fetch origin main --quiet").await;
    merge_probe_has_conflicts(&merge_tree_probe().await)
}

async fn build_merge_content(r: ShResult) -> MergeResult {
    println!("   Checking for merge conflicts with origin/main...");
    let mut content = format!("# Merge Conflict Check\n\nExit code: {}\n", r.code);
    if !r.stdout.is_empty() {
        content += &format!("```\n{}\n```\n", r.stdout);
    }
    if !r.stderr.is_empty() {
        content += &format!("Stderr:\n```\n{}\n```\n", r.stderr);
    }

    let has_conflicts = merge_probe_has_conflicts(&r);
    if has_conflicts {
        println!("⚠️  Potential merge conflicts detected");
        if let Some(base) = sh("git merge-base HEAD origin/main").await {
            if let Some(commits) = sh(&format!("git log --oneline {base}..origin/main")).await {
                content +=
                    &format!("\n## Recent commits on main since merge-base\n```\n{commits}\n```\n");
            }
        }
    } else {
        println!("✅ No merge conflicts");
    }
    MergeResult {
        content,
        has_conflicts,
    }
}

// ── PR handling ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PrData {
    number: u64,
    url: String,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
    #[serde(default)]
    author: PrAuthor,
}

#[derive(Deserialize, Default)]
struct PrAuthor {
    #[serde(default)]
    login: String,
}

async fn lookup_existing_pr(bg_pr: Child) -> Option<PrInfo> {
    let pr_data: Option<PrData> = sh_wait(bg_pr).await.and_then(|s| parse_json(&s));
    pr_data.map(|pr| {
        println!("🔗 {}", pr.url);
        PrInfo {
            number: Some(pr.number.to_string()),
            url: Some(pr.url),
            is_draft: pr.is_draft,
            author_login: pr.author.login,
        }
    })
}

/// Block SIGCHLD on the current thread for the duration of `f`. Tokio's I/O
/// driver runs on a separate thread, so the kernel still gets to reap children
/// via that thread — we just stop SIGCHLD from interrupting our blocking
/// `read(2)` inside dialoguer (which surfaces as EINTR → user-cancelled).
fn with_sigchld_blocked<T>(f: impl FnOnce() -> T) -> T {
    let mut new_set: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut old_set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut new_set);
        libc::sigaddset(&mut new_set, libc::SIGCHLD);
        libc::pthread_sigmask(libc::SIG_BLOCK, &new_set, &mut old_set);
    }
    let out = f();
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &old_set, std::ptr::null_mut());
    }
    out
}

fn prompt_pr_title(branch_commits: &Option<String>) -> Option<String> {
    let commit_subjects: Vec<String> = branch_commits
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(|line| {
            line.split_once(' ')
                .map(|(_, t)| t)
                .unwrap_or(line)
                .to_string()
        })
        .collect();
    if !commit_subjects.is_empty() {
        println!("   Commits on this branch:");
        for title in &commit_subjects {
            println!("      • {title}");
        }
    }

    // Show the single-commit subject as a greyed default, but never auto-fill
    // the body from the commit message.
    let mut input = dialoguer::Input::<String>::new().with_prompt("Title");
    if commit_subjects.len() == 1 {
        input = input.default(commit_subjects[0].clone());
    }
    // Background tasks (dedup hints, PR areas, relevant-context) keep
    // printing while we block here; buffer their status lines so they
    // can't clobber the typed title.
    let read = {
        let _hold = status::hold();
        with_sigchld_blocked(|| input.interact_text())
    };
    let title = match read {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            println!("⚠️  Title prompt cancelled: {e}");
            return None;
        }
    };
    if title.is_empty() {
        println!("⚠️  Empty title — aborting PR creation.");
        return None;
    }
    Some(title)
}

/// Title resolution for --non-interactive PR creation: the --title flag,
/// else the newest commit subject. None aborts PR creation, matching the
/// cancelled-prompt path in [prompt_pr_title].
fn non_interactive_pr_title(flag: Option<&str>, branch_commits: &Option<String>) -> Option<String> {
    if let Some(t) = flag {
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let subject = branch_commits
        .as_deref()
        .unwrap_or("")
        .lines()
        .next()
        .map(|line| {
            line.split_once(' ')
                .map(|(_, t)| t)
                .unwrap_or(line)
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty());
    match &subject {
        Some(t) => println!("   PR title (from commit subject): {t}"),
        None => {
            println!("⚠️  No --title and no commits to derive a title from — skipping PR creation.")
        }
    }
    subject
}

async fn create_pr_with_title(title: &str) -> PrInfo {
    let rc = std::process::Command::new("gh")
        .args(["pr", "create", "--draft", "--title", title, "--body", ""])
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(1);

    if rc == 0 {
        if let Some(data) = sh("gh pr view --json number,url,isDraft,author")
            .await
            .and_then(|s| parse_json::<PrData>(&s))
        {
            return PrInfo {
                number: Some(data.number.to_string()),
                url: Some(data.url),
                is_draft: true,
                author_login: data.author.login,
            };
        }
    } else {
        println!("⚠️  PR creation failed");
    }
    PrInfo {
        number: None,
        url: None,
        is_draft: false,
        author_login: String::new(),
    }
}

// ── Reviews + CI collection ──────────────────────────────────────────────────

async fn collect_reviews_and_ci(
    pr_number: &str,
    pr_url: &str,
    branch: &str,
    has_conflicts: bool,
    base_ref: &str,
) -> CiResult {
    let mut files = Vec::new();
    let mut has_unresolved = false;
    let mut skip_ci: Option<String> = None;
    let mut failed_names = Vec::new();

    let url_parts: Vec<&str> = pr_url.split('/').collect();
    let bg_checks = if !has_conflicts {
        Some(sh_bg(&format!("gh pr checks {pr_number}")))
    } else {
        None
    };

    if url_parts.len() >= 5 {
        let (owner, repo) = (url_parts[3], url_parts[4]);
        let (review_files, unresolved) = collect_reviews(owner, repo, pr_number).await;
        files.extend(review_files);
        has_unresolved = unresolved;
        if has_unresolved {
            println!("⚠️  Unresolved review comments found");
        }
    }

    if has_conflicts {
        skip_ci = Some("merge conflicts".into());
    }

    if let Some(reason) = &skip_ci {
        println!("   Skipping CI wait — {reason} to investigate first.");
        if let Some(bg) = bg_checks {
            sh3_wait(bg).await;
        }
    } else {
        let first = if let Some(bg) = bg_checks {
            sh3_wait(bg).await
        } else {
            sh3(&format!("gh pr checks {pr_number}")).await
        };
        let mut counts = parse_checks(&first.stdout, IGNORED_CHECKS);
        let observed = counts.passed + counts.failed + counts.pending + counts.skipping;
        if first.code != 0 && counts.failed == 0 && observed == 0 {
            counts.pending = counts.pending.max(1);
        }

        if has_unresolved && counts.failed == 0 {
            skip_ci = Some("unresolved review comments".into());
            println!("   Skipping CI wait — unresolved review comments to investigate first.");
            let ci_content = format!(
                "# CI Checks\n\nPR: #{pr_number}\nExit code: {}\n\
                 Note: CI wait skipped due to unresolved review comments.\n```\n{}\n```\n",
                first.code,
                strip_skipping(&first.stdout)
            );
            files.push(section("ci", &ci_content));
        } else {
            let ci = wait_for_ci(
                pr_number,
                branch,
                Some((counts, first.code, first.stdout)),
                base_ref,
            )
            .await;
            files.push(section("ci", &ci.ci_content));
            if let Some(ref failures) = ci.failures_content {
                files.push(section("failures", failures));
            }
            if let Some(full) = ci.failures_full_file {
                files.push(full);
            }
            files.extend(ci.lint_files);
            failed_names = ci.failed_names;
        }
    }

    CiResult {
        files,
        has_unresolved,
        skip_ci,
        failed_names,
    }
}

// ── Context collection ───────────────────────────────────────────────────────

async fn collect_context_strings(
    branch_commits: &Option<String>,
    base_ref: &str,
) -> ContextStrings {
    let diff_cmd = format!("git diff --stat {base_ref}...HEAD");
    let (diff, main) = tokio::join!(
        sh(&diff_cmd),
        sh(
            "git log HEAD..origin/main --oneline --grep='build: automatic update of go-api' --invert-grep"
        ),
    );

    let changed_files = diff
        .filter(|s| !s.is_empty())
        .map(|s| format!("\nFiles changed in this PR:\n```\n{s}\n```\n"))
        .unwrap_or_default();

    let main_commits = main
        .filter(|s| !s.is_empty())
        .map(|s| {
            let lines: Vec<&str> = s.lines().collect();
            let display = if lines.len() > 30 {
                format!(
                    "{}\n[Truncated - use `git log HEAD..origin/main --oneline` to see all commits]",
                    lines[..30].join("\n")
                )
            } else {
                s
            };
            format!("\nRecent commits on main not on this branch:\n```\n{display}\n```\n")
        })
        .unwrap_or_default();

    let pr_commits = branch_commits
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(|s| format!("\nCommits in this PR:\n```\n{s}\n```\n"))
        .unwrap_or_default();

    ContextStrings {
        changed_files,
        main_commits,
        pr_commits,
    }
}

// ── Build files index ────────────────────────────────────────────────────────

fn build_files_index(
    files: &[TempFile],
    has_conflicts: bool,
    failed_names: &[String],
    dedup_funcs: Option<usize>,
) -> String {
    let failures_label = if failed_names.is_empty() {
        "CI failure logs (distilled summary; references full log below)".into()
    } else {
        format!(
            "CI failure logs (distilled summary; references full log below): {}",
            failed_names.join(", ")
        )
    };
    let merge_label = if has_conflicts {
        "would merging HEAD into origin/main conflict? (git merge-tree) — CONFLICTS DETECTED"
    } else {
        "would merging HEAD into origin/main conflict? (git merge-tree) — clean"
    };

    let labels: HashMap<&str, String> = HashMap::from([
        ("push", "push result + git status".into()),
        ("merge", merge_label.into()),
        (
            "review-threads",
            "review threads — inline review + bot comments)".into(),
        ),
        ("review-pr", "top-level PR reviews".into()),
        (
            "pr-meta",
            "PR title, body, review decision, requested reviewers".into(),
        ),
        ("ci", "CI check results".into()),
        ("failures", failures_label),
        (
            "failures-full",
            "CI failure logs (full, untruncated raw)".into(),
        ),
        ("lint", "local lint failures".into()),
        (
            "dedup",
            format!(
                "{} potential duplicated function{} (hints, not verdicts)",
                dedup_funcs.unwrap_or(0),
                if dedup_funcs == Some(1) { "" } else { "s" },
            ),
        ),
    ]);

    let mut index = String::new();
    for f in files {
        let name = f.path.file_name().unwrap_or_default().to_string_lossy();
        // Filename: psc-{prefix}-{random}.{ext} — extract prefix
        let parts: Vec<&str> = name.split('-').collect();
        let prefix = if parts.len() >= 3 {
            parts[1..parts.len() - 1].join("-")
        } else {
            name.to_string()
        };
        let label = labels
            .get(prefix.as_str())
            .map(|s| s.as_str())
            .unwrap_or(&prefix);
        index += &format!("- `{}` ({} lines) — {label}\n", f.path.display(), f.lines);
    }
    index
}

// ── Push content ─────────────────────────────────────────────────────────────

fn build_push_content(push: &PushResult, git_status: &str) -> String {
    let mut content = format!(
        "# Push Result\n\nBranch: `{}`\nStrategy: {}, exit code: {}\n",
        push.branch, push.strategy, push.code
    );
    if !push.stdout.is_empty() {
        content += &format!("```\n{}\n```\n", push.stdout);
    }
    if !push.stderr.is_empty() {
        content += &format!("Stderr:\n```\n{}\n```\n", push.stderr);
    }
    content += &format!("\n# Git Status (porcelain v2)\n```\n{git_status}\n```\n");
    content
}

// ── Diff files ───────────────────────────────────────────────────────────────

async fn write_diff_files(changed_files: &[&str], base_ref: &str) -> String {
    let mut result = String::new();
    for fname in changed_files {
        if let Some(diff) = sh(&format!("git diff {base_ref}...HEAD -- {fname}")).await {
            if !diff.is_empty() {
                let f = section("diff", &format!("# Diff: {fname}\n```diff\n{diff}\n```\n"));
                result += &format!(
                    "- `{}` ({} lines) — diff for {fname}\n",
                    f.path.display(),
                    f.lines
                );
            }
        }
    }
    result
}

async fn full_diffs<'a>(changed_files: &[&'a str], base_ref: &str) -> Vec<(&'a str, String)> {
    let mut result = Vec::<(&str, String)>::new();
    for &fname in changed_files {
        let res = if let Some(diff) = sh(&format!("git diff {base_ref}...HEAD -- {fname}")).await {
            if !diff.is_empty() {
                (fname, diff)
            } else {
                (fname, "<empty>".to_string())
            }
        } else {
            (fname, "<failed to get diff>".to_string())
        };
        result.push(res);
    }
    result
}

/// Parses the new-file start line from a unified-diff hunk header body (the
/// text after the leading `@@`). For `@@ -a,b +c,d @@ ...` it returns `c`.
fn parse_hunk_new_start(hunk_rest: &str) -> Option<usize> {
    hunk_rest
        .split_whitespace()
        .find(|t| t.starts_with('+'))
        .and_then(|t| t[1..].split(',').next())
        .and_then(|n| n.parse::<usize>().ok())
}

/// Prefixes each added or context line of a unified diff with its line number
/// in the new (post-change) file, e.g. `58: +// foo`. Deleted lines get no
/// number, since they are absent from the new file. Hunk and file headers
/// pass through unchanged; the counter is reseeded from each `@@` header's
/// `+c` start.
fn annotate_diff_new_line_numbers(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len() + diff.len() / 4);
    let mut new_line: usize = 0;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            new_line = parse_hunk_new_start(rest).unwrap_or(new_line);
            out.push_str(line);
        } else if line.is_empty()
            || line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with('\\')
        {
            // File/metadata headers and "\ No newline at end of file": no number.
            out.push_str(line);
        } else if line.starts_with('-') {
            // Deleted line: absent from the new file, so no number.
            out.push_str(line);
        } else {
            // Added (`+`) or context (leading space) line: number it.
            out.push_str(&format!("{new_line}: {line}"));
            new_line += 1;
        }
        out.push('\n');
    }
    out
}

// ── Guide file collection (CLAUDE.md / AGENTS.md) ───────────────────────────

/// Collects every `CLAUDE.md`, `AGENTS.md`, and `AGENT.md` guide that
/// applies to `paths`.
///
/// A guide applies when it sits in the directory of one of `paths` or in any
/// ancestor up to and including the repository root (`git rev-parse
/// --show-toplevel`). If the working directory isn't inside a git repo,
/// walking continues to the filesystem root — only directories that actually
/// hold a guide contribute anything anyway.
///
/// `~/.claude/CLAUDE.md` is also always seeded when it exists, even though
/// it sits outside the project root. The RAG scorer downstream is expected
/// to drop irrelevant chunks per PR.
///
/// `@`-references inside collected guides are followed transitively. A
/// reference may be absolute (`@/abs/path`), home-rooted (`@~/path`), or
/// relative to the file containing the reference (`@./sibling.md`,
/// `@subdir/file.md`, `@foo.md`). References that look path-like but fail to
/// resolve emit a warning to stderr; non-path-looking matches (e.g.
/// `@username` in prose) are ignored silently.
///
/// Returns absolute, canonicalized paths, deduplicated, in sorted order.
#[allow(dead_code)]
fn collect_relevant_guides<P: AsRef<std::path::Path>>(paths: &[P]) -> Vec<PathBuf> {
    use std::collections::HashSet;
    use std::path::Path;

    const GUIDE_NAMES: &[&str] = &["CLAUDE.md", "AGENTS.md", "AGENT.md"];

    let project_root = git_top_level();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let mut found: HashSet<PathBuf> = HashSet::new();
    let mut queue: Vec<PathBuf> = Vec::new();
    let mut walked_dirs: HashSet<PathBuf> = HashSet::new();

    // Phase 1: walk ancestor chains of each input.
    for p in paths {
        let abs = if p.as_ref().is_absolute() {
            p.as_ref().to_path_buf()
        } else {
            cwd.join(p.as_ref())
        };
        let mut dir = if abs.is_dir() {
            abs.clone()
        } else {
            abs.parent().map(Path::to_path_buf).unwrap_or(abs)
        };
        loop {
            // Same chain visited via another input — every ancestor was too.
            if !walked_dirs.insert(dir.clone()) {
                break;
            }
            for name in GUIDE_NAMES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    let canon = candidate.canonicalize().unwrap_or(candidate);
                    if found.insert(canon.clone()) {
                        queue.push(canon);
                    }
                }
            }
            if project_root.as_ref().is_some_and(|r| &dir == r) {
                break;
            }
            let Some(parent) = dir.parent() else { break };
            dir = parent.to_path_buf();
        }
    }

    // Phase 1b: seed with user-global ~/.claude/CLAUDE.md if present.
    // It's outside any project's git toplevel so the ancestor walk above
    // never reaches it, but the user's global instructions are often
    // relevant — leave it to the RAG scorer to drop irrelevant chunks.
    // @-references inside it are still followed transitively by phase 2.
    let user_claude_md = home_dir().join(".claude").join("CLAUDE.md");
    if user_claude_md.is_file() {
        let canon = user_claude_md.canonicalize().unwrap_or(user_claude_md);
        if found.insert(canon.clone()) {
            queue.push(canon);
        }
    }

    // Phase 2: follow @-references transitively. Cycle-safe via `found`.
    static AT_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `@` must follow start-of-line or a non-word, non-@, non-backtick char.
    // Excluding backtick keeps inline code spans like `` `@types/react` ``
    // (npm scope names mentioned in prose) from registering as references.
    // Capture is `\S+`; trailing punctuation is stripped in the consumer.
    let at_re = AT_RE.get_or_init(|| Regex::new(r"(?m)(?:^|[^\w@`])@(\S+)").unwrap());

    while let Some(file) = queue.pop() {
        let content = match std::fs::read_to_string(&file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: failed to read guide {}: {e}", file.display());
                continue;
            }
        };
        let file_dir = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        for cap in at_re.captures_iter(&content) {
            let raw = cap.get(1).unwrap().as_str().trim_end_matches(|c: char| {
                matches!(c, '.' | ',' | ';' | ':' | ')' | ']' | '}' | '"' | '\'')
            });
            if raw.is_empty() {
                continue;
            }
            match resolve_at_ref(raw, &file_dir) {
                Some(p) => {
                    let canon = p.canonicalize().unwrap_or(p);
                    if found.insert(canon.clone()) {
                        queue.push(canon);
                    }
                }
                None if looks_like_path_ref(raw) => {
                    eprintln!(
                        "warning: unresolved @ reference '{raw}' in {}",
                        file.display()
                    );
                }
                None => {} // not path-like — likely prose, not a reference
            }
        }
    }

    let mut result: Vec<PathBuf> = found.into_iter().collect();
    result.sort();
    result
}

#[allow(dead_code)]
fn git_top_level() -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Expands an `@`-reference body (the text after `@`) and returns it iff the
/// target exists. Relative references are resolved against `parent_dir`,
/// which should be the directory of the file the reference was found in.
#[allow(dead_code)]
fn resolve_at_ref(raw: &str, parent_dir: &std::path::Path) -> Option<PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        home_dir().join(rest)
    } else if raw == "~" {
        home_dir()
    } else if raw.starts_with('/') {
        PathBuf::from(raw)
    } else {
        parent_dir.join(raw)
    };
    if expanded.is_file() {
        Some(expanded)
    } else {
        None
    }
}

/// Heuristic: true if `raw` looks intentional enough to warn about when it
/// fails to resolve. Used to silence `@username`-style prose matches.
#[allow(dead_code)]
fn looks_like_path_ref(raw: &str) -> bool {
    raw.contains('/')
        || raw.starts_with('~')
        || raw.starts_with('.')
        || std::path::Path::new(raw).extension().is_some()
}

// ── Review log context ───────────────────────────────────────────────────────

fn get_review_log_context(pr_number: &Option<String>) -> (String, String) {
    let Some(pr) = pr_number else {
        return (String::new(), String::new());
    };
    let log_dir = home_dir().join(format!(".dragonfly/pr-logs/{pr}"));
    let mut existing: Vec<PathBuf> = std::fs::read_dir(&log_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("review-") && n.ends_with(".md"))
        })
        .collect();
    existing.sort();
    let next_n = existing.len();

    let mut prior = String::new();
    if !existing.is_empty() {
        prior = "\nPrior review logs:\n".into();
        for path in &existing {
            let line_count = std::fs::read_to_string(path)
                .map(|s| s.lines().count())
                .unwrap_or(0);
            prior += &format!("- `{}` ({line_count} lines)\n", path.display());
        }
    }

    let instruction = format!(
        "\nAfter fixing issues in 'Phase 6: Custom review', directly after pushing (but before waiting for CI), \
         save a brief summary to `{}/review-{next_n}.md`. \
         Include which issues the user did not want to fix.\n",
        log_dir.display()
    );
    (prior, instruction)
}

fn has_prior_review_logs(pr_number: &str) -> bool {
    let log_dir = home_dir().join(format!(".dragonfly/pr-logs/{pr_number}"));
    let Ok(entries) = std::fs::read_dir(&log_dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("review-") && n.ends_with(".md"))
    })
}

// ── Initial code review (gemini) ─────────────────────────────────────────────

/// Build the inline-diff content the initial-review LLM sees. Same shape as
/// what `analyze_pr_areas` consumes — kit has no tool use, so the commit
/// list, file-change summary, and diff blocks all have to be in the prompt
/// itself rather than reachable via Read.
async fn build_review_input(base_ref: &str) -> Option<String> {
    let changed_files = get_changed_files(base_ref).await;
    let relevant = filter_relevant_files(&changed_files);
    if relevant.is_empty() {
        return None;
    }
    let diffs = full_diffs(&relevant, base_ref).await;
    let diff_body = diffs
        .iter()
        .map(|(name, diff)| format!("<diff name=\"{name}\">\n{diff}\n</diff>"))
        .collect::<Vec<_>>()
        .join("\n");
    if diff_body.trim().is_empty() {
        return None;
    }
    let branch_commits = sh(&format!("git log {base_ref}..HEAD --oneline")).await;
    let ctx = collect_context_strings(&branch_commits, base_ref).await;
    Some(format!(
        "{}{}\nPer-file diffs:\n{}\n",
        ctx.pr_commits, ctx.changed_files, diff_body
    ))
}

/// True if the initial-review output contains a HIGH severity marker.
/// The prompt instructs the model to use `Severity: HIGH`, but we accept
/// any casing and surrounding whitespace; word-boundary on `high` keeps
/// us from matching `highest`, `highlight`, etc.
fn review_has_high_severity(body: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)severity\s*:\s*high\b").unwrap());
    re.is_match(body)
}

async fn run_initial_review(base_ref: &str, model: Model) -> Option<String> {
    let diff = build_review_input(base_ref).await?;
    let input = section("initial-review-input", &diff);
    call_kit_llm(model.kit_id(), INITIAL_REVIEW_PROMPT, &input.path).await
}

async fn print_initial_review_prompt() {
    let base_ref = pr_base_ref().await;
    if base_ref != "origin/main" {
        eprintln!("   Diffing against `{base_ref}` (stack parent).");
    }
    let diff = build_review_input(&base_ref).await.unwrap_or_default();
    println!("# System prompt\n\n{INITIAL_REVIEW_PROMPT}");
    if diff.is_empty() {
        println!("# User content\n\n(no diff against {base_ref})");
    } else {
        println!("# User content\n\n{diff}");
    }
}

/// Materialise relevant CLAUDE.md/AGENTS.md chunks for the current PR,
/// score them via `lov eval rag score`, and dump a TSV the user can sort by
/// score. Threshold tuning is a separate problem — this command is meant
/// for exploration, not for filtering inside the kit-llm pipeline.
async fn score_guides_cmd(
    output_path: Option<PathBuf>,
    base_override: Option<String>,
    threshold: Option<f64>,
) {
    let base_ref = match base_override {
        Some(b) => {
            eprintln!("   Diffing against `{b}` (--base override).");
            b
        }
        None => {
            let r = pr_base_ref().await;
            if r != "origin/main" {
                eprintln!("   Diffing against `{r}` (stack parent).");
            }
            r
        }
    };

    let Some(query) = build_review_input(&base_ref).await else {
        eprintln!("No diff against {base_ref}.");
        std::process::exit(1);
    };

    let changed = get_changed_files(&base_ref).await;
    let guide_paths = collect_relevant_guides(&changed);
    let chunks = guide_chunks::chunk_guides(&guide_paths);
    if chunks.is_empty() {
        eprintln!(
            "No guide chunks resolved for {} changed files.",
            changed.len()
        );
        std::process::exit(1);
    }
    eprintln!(
        "   Scoring {} chunks from {} guides via `lov eval rag score`...",
        chunks.len(),
        guide_paths.len(),
    );

    let start = std::time::Instant::now();
    let scores = match pr_score::score_chunks(&chunks, &query).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("score-guides failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("   Scored in {:.1}s.", start.elapsed().as_secs_f64());

    let (out_chunks, out_scores): (Vec<guide_chunks::GuideChunk>, Vec<f64>) = match threshold {
        Some(t) => {
            let above: Vec<usize> = scores
                .iter()
                .enumerate()
                .filter(|&(_, &s)| s >= t)
                .map(|(i, _)| i)
                .collect();
            let kept = guide_chunks::with_ancestors(&chunks, &above);
            eprintln!(
                "   Threshold ≥{:.1}: {} chunks scored, {} above, {} after ancestor pull.",
                t,
                chunks.len(),
                above.len(),
                kept.len(),
            );
            (
                kept.iter().map(|&i| chunks[i].clone()).collect(),
                kept.iter().map(|&i| scores[i]).collect(),
            )
        }
        None => (chunks, scores),
    };

    let writer: Box<dyn std::io::Write> = match &output_path {
        Some(p) => match std::fs::File::create(p) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("failed to create {}: {e}", p.display());
                std::process::exit(1);
            }
        },
        None => Box::new(std::io::stdout()),
    };
    let mut writer = std::io::BufWriter::new(writer);
    if let Err(e) = pr_score::write_scores_tsv(&mut writer, &out_chunks, &out_scores) {
        eprintln!("failed to write TSV: {e}");
        std::process::exit(1);
    }
    drop(writer);

    if let Some(p) = output_path {
        eprintln!("   TSV written to {}", p.display());
    }
}

/// Score relevant guide chunks against the PR diff via `lov eval rag score`,
/// keep those at or above [RELEVANT_CONTEXT_THRESHOLD] plus their heading
/// ancestors, and render a `<relevant-context>` block. Returns an empty
/// string on any failure (lov missing, auth error, empty diff) so the
/// prompt still builds without the RAG section.
const RELEVANT_CONTEXT_THRESHOLD: f64 = 5.0;

async fn build_relevant_context(base_ref: &str) -> String {
    let Some(query) = build_review_input(base_ref).await else {
        return String::new();
    };
    let changed = get_changed_files(base_ref).await;
    let guide_paths = collect_relevant_guides(&changed);
    let chunks = guide_chunks::chunk_guides(&guide_paths);
    if chunks.is_empty() {
        return String::new();
    }
    let scores = match pr_score::score_chunks(&chunks, &query).await {
        Ok(s) => s,
        Err(e) => {
            status::status_line!("   Skipping <relevant-context> ({e}).");
            return String::new();
        }
    };
    let above: Vec<usize> = scores
        .iter()
        .enumerate()
        .filter(|&(_, &s)| s >= RELEVANT_CONTEXT_THRESHOLD)
        .map(|(i, _)| i)
        .collect();
    let kept = guide_chunks::with_ancestors(&chunks, &above);
    if kept.is_empty() {
        return String::new();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    status::status_line!(
        "   Including {} relevant-context chunks (threshold ≥{:.1}, +{} ancestor pulls).",
        kept.len(),
        RELEVANT_CONTEXT_THRESHOLD,
        kept.len().saturating_sub(above.len()),
    );
    pr_score::render_relevant_context(&chunks, &kept, &cwd)
}

async fn pr_review_cmd(model: Option<Model>) -> i32 {
    let model = model.unwrap_or(Model::Gemini);
    let base_ref = pr_base_ref().await;
    if base_ref != "origin/main" {
        eprintln!("   Diffing against `{base_ref}` (stack parent).");
    }
    eprintln!("   Running initial code review via {}...", model.kit_id());
    let start = std::time::Instant::now();
    match run_initial_review(&base_ref, model).await {
        Some(body) => {
            eprintln!("   Reviewed in {:.1}s.", start.elapsed().as_secs_f64());
            println!("{body}");
            0
        }
        None => {
            eprintln!(
                "Initial review failed (kit/model unavailable, or no diff against {base_ref})."
            );
            1
        }
    }
}

// ── Review-agent context (SubagentStart hook target) ─────────────────────────

/// TTL for the cached review-agent context. The Phase 6 fan-out spawns
/// several review-agent subagents within a few seconds of each other,
/// so a few-minute window is enough to serve them all from cache while
/// still picking up new commits / diffs on the next review round.
const REVIEW_CTX_TTL_SEC: u64 = 240;

fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce4_84222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn review_ctx_paths(inline_diffs: bool) -> (PathBuf, PathBuf) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Key by mode too: the inline and reference forms differ, so they must
    // not share a cache entry.
    let key = fnv1a64(&format!(
        "{}\u{0}inline={inline_diffs}",
        cwd.to_string_lossy()
    ));
    let cache = PathBuf::from(format!("/tmp/dragonfly-review-ctx-{key:016x}.md"));
    let lock = PathBuf::from(format!("/tmp/dragonfly-review-ctx-{key:016x}.lock"));
    (cache, lock)
}

/// Acquires an exclusive POSIX advisory lock (flock(2), LOCK_EX) on
/// the given fd. Blocks until granted. The lock is released when the
/// file is closed (i.e. when the [File] is dropped).
///
/// Used to serialize concurrent review-agent context builds across
/// processes — when the parent agent spawns N review-agents in
/// parallel, the SubagentStart hook fires N times; only the first one
/// generates the cache, the others block here and then read it.
fn flock_exclusive(f: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Builds the `<dragonfly-context>` block that gets injected into
/// review-agent subagents. Includes commit list, files-changed summary,
/// paths to per-file diff files under /tmp, and a scored
/// `<relevant-context>` block for the changed files. Heavy: the
/// relevant-context step shells out to `lov eval rag score`, which is the
/// reason for the surrounding cache + lock.
async fn build_review_agent_context(inline_diffs: bool) -> String {
    let base_ref = pr_base_ref().await;
    let log_cmd = format!("git log {base_ref}..HEAD --oneline");
    let (branch, branch_commits) = tokio::join!(sh("git branch --show-current"), sh(&log_cmd),);

    let changed_files = get_changed_files(&base_ref).await;
    let relevant: Vec<String> = filter_relevant_files(&changed_files)
        .iter()
        .map(|s| s.to_string())
        .collect();

    let relevant_for_diffs = relevant.clone();
    let base_for_diffs = base_ref.clone();
    let diff_files_fut = async move {
        let r: Vec<&str> = relevant_for_diffs.iter().map(String::as_str).collect();
        write_diff_files(&r, &base_for_diffs).await
    };

    let relevant_for_full = relevant.clone();
    let base_for_full = base_ref.clone();
    let full_diffs_fut = async move {
        let r: Vec<&str> = relevant_for_full.iter().map(String::as_str).collect();
        // full_diffs returns borrows tied to `r`; convert to owned
        // strings so the result can outlive the async block.
        full_diffs(&r, &base_for_full)
            .await
            .into_iter()
            .map(|(name, diff)| (name.to_string(), diff))
            .collect::<Vec<(String, String)>>()
    };

    let base_for_rag = base_ref.clone();
    let rag_fut = async move { build_relevant_context(&base_for_rag).await };

    let ctx_fut = collect_context_strings(&branch_commits, &base_ref);

    let (diff_files_str, full_diffs_vec, relevant_context, ctx) =
        tokio::join!(diff_files_fut, full_diffs_fut, rag_fut, ctx_fut);

    // Reuse the SHA-keyed pr-areas cache populated by the main prompt
    // build. When this runs standalone (no preceding dragonfly),
    // it cold-builds once (~3-5 s) and writes the cache. analyze_pr_areas
    // needs the inline diff content because kit has no tool-use.
    let full_diff_str = full_diffs_vec
        .iter()
        .map(|(name, diff)| format!("<diff name=\"{name}\">\n{diff}\n</diff>"))
        .collect::<Vec<_>>()
        .join("\n");
    let pr_areas = if full_diff_str.trim().is_empty() {
        None
    } else {
        analyze_pr_areas(&full_diff_str, &ctx.changed_files, &ctx.pr_commits).await
    };

    let branch = branch.unwrap_or_default();
    let mut out = String::new();
    out.push_str("<dragonfly-context>\n");
    out.push_str(&format!(
        "Branch: `{}`  (base: `{}`)\n",
        if branch.is_empty() {
            "(detached)".to_string()
        } else {
            branch
        },
        base_ref,
    ));
    out.push_str(&ctx.pr_commits);
    out.push_str(&ctx.changed_files);
    if inline_diffs {
        // Self-contained context: inline each changed file's diff as a
        // <diff name=".."> block instead of a /tmp path reference. Every
        // added/context line is prefixed with its new-file line number so the
        // reviewer can cite file:line without counting from the hunk header.
        let inlined = full_diffs_vec
            .iter()
            .filter(|(_, diff)| diff != "<empty>")
            .map(|(name, diff)| {
                format!(
                    "<diff name=\"{name}\">\n{}\n</diff>",
                    annotate_diff_new_line_numbers(diff)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if inlined.trim().is_empty() {
            out.push_str("\n(no per-file diffs against base — review may be unnecessary)\n");
        } else {
            out.push_str("\n<diffs>\n");
            out.push_str(&inlined);
            out.push_str("\n</diffs>\n");
        }
    } else if !diff_files_str.is_empty() {
        out.push_str("\nPer-file diff files:\n");
        out.push_str(&diff_files_str);
    } else {
        out.push_str("\n(no per-file diffs against base — review may be unnecessary)\n");
    }
    if let Some(v) = pr_areas {
        if let Ok(pretty) = serde_json::to_string_pretty(&v) {
            out.push_str("\n<pr-areas>\nPer-file `potential_for_bugs` and `potential_for_simplification` scores (1-10). Use to prioritise the assigned concern.\n```json\n");
            out.push_str(&pretty);
            out.push_str("\n```\n</pr-areas>\n");
        }
    }
    if !relevant_context.is_empty() {
        out.push('\n');
        out.push_str(&relevant_context);
        if !relevant_context.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("</dragonfly-context>\n");
    out
}

/// Builds the `<dragonfly-context>` block for dedup-reviewer subagents and
/// prints it (implementation of `prompt dedup-reviewer`). Tailored for
/// duplication review: commit list, files-changed summary, per-file diff
/// paths, and the full duplicate-function hint list inlined — the subagent's
/// primary input, so no file indirection. Skips the RAG-scored
/// `<relevant-context>` (convention excerpts don't help duplication review
/// and are the slowest leg of the review-agent build), which also makes this
/// cheap enough to rebuild per invocation: only one dedup-reviewer spawns
/// per review round, so the review-agent cache+flock machinery buys nothing.
async fn dedup_reviewer_prompt_cmd() -> i32 {
    let base_ref = pr_base_ref().await;
    let log_cmd = format!("git log {base_ref}..HEAD --oneline");
    let (branch, branch_commits) = tokio::join!(sh("git branch --show-current"), sh(&log_cmd));

    let changed_files = get_changed_files(&base_ref).await;
    let relevant: Vec<String> = filter_relevant_files(&changed_files)
        .iter()
        .map(|s| s.to_string())
        .collect();

    let base_for_diffs = base_ref.clone();
    let diff_files_fut = async move {
        let r: Vec<&str> = relevant.iter().map(String::as_str).collect();
        write_diff_files(&r, &base_for_diffs).await
    };
    let hints_fut = dedup::build_inline_block(&base_ref);
    let ctx_fut = collect_context_strings(&branch_commits, &base_ref);

    let (diff_files_str, hints_block, ctx) = tokio::join!(diff_files_fut, hints_fut, ctx_fut);

    let branch = branch.unwrap_or_default();
    let mut out = String::from("<dragonfly-context>\n");
    out.push_str(&format!(
        "Branch: `{}`  (base: `{}`)\n",
        if branch.is_empty() {
            "(detached)".to_string()
        } else {
            branch
        },
        base_ref,
    ));
    out.push_str(&ctx.pr_commits);
    out.push_str(&ctx.changed_files);
    if !diff_files_str.is_empty() {
        out.push_str("\nPer-file diff files:\n");
        out.push_str(&diff_files_str);
    } else {
        out.push_str("\n(no per-file diffs against base — review may be unnecessary)\n");
    }
    match hints_block {
        Some(block) => {
            out.push('\n');
            out.push_str(&block);
        }
        None => out.push_str(
            "\n(no duplicate-function hints for this PR — focus on the broader duplication hunt)\n",
        ),
    }
    out.push_str("</dragonfly-context>\n");
    print!("{out}");
    0
}

/// Implementation of `prompt review-agent`. Serializes parallel callers
/// via flock on a per-cwd lockfile; returns the cached body when it's
/// less than [REVIEW_CTX_TTL_SEC] seconds old, otherwise rebuilds.
async fn review_agent_prompt_cmd(inline_diffs: bool) -> i32 {
    let (cache_path, lock_path) = review_ctx_paths(inline_diffs);

    // Open the lock file ahead of any work. Drop happens at end of fn,
    // which releases the flock — the cache write must finish first.
    let lock_file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "review-agent: failed to open lock {}: {e}",
                lock_path.display()
            );
            return 1;
        }
    };
    if let Err(e) = flock_exclusive(&lock_file) {
        eprintln!("review-agent: flock failed: {e}");
        return 1;
    }

    // Cache hit? Read mtime; if within TTL, dump it.
    if let Ok(meta) = std::fs::metadata(&cache_path) {
        if let Ok(modified) = meta.modified() {
            if let Ok(age) = SystemTime::now().duration_since(modified) {
                if age.as_secs() < REVIEW_CTX_TTL_SEC {
                    match std::fs::read_to_string(&cache_path) {
                        Ok(body) => {
                            eprintln!(
                                "review-agent: cache hit ({}, {}s old)",
                                cache_path.display(),
                                age.as_secs(),
                            );
                            print!("{body}");
                            return 0;
                        }
                        Err(e) => {
                            eprintln!("review-agent: cache file unreadable ({e}); regenerating",);
                        }
                    }
                }
            }
        }
    }

    // Cache miss / stale — rebuild.
    eprintln!("review-agent: building context (cache miss/stale)...");
    let start = std::time::Instant::now();
    let body = build_review_agent_context(inline_diffs).await;
    eprintln!(
        "review-agent: built in {:.1}s ({} bytes)",
        start.elapsed().as_secs_f64(),
        body.len(),
    );

    // Atomic write: tmp file + rename, so a concurrent reader (if a
    // future change downgrades to LOCK_SH for the hot path) never sees
    // a torn write.
    let tmp_path = cache_path.with_extension(format!("md.tmp.{}", std::process::id()));
    if let Err(e) = std::fs::write(&tmp_path, &body) {
        eprintln!("review-agent: failed to write {}: {e}", tmp_path.display());
        return 1;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &cache_path) {
        eprintln!(
            "review-agent: failed to rename {} -> {}: {e}",
            tmp_path.display(),
            cache_path.display(),
        );
        return 1;
    }
    print!("{body}");
    0
}

// ── PR area analysis ─────────────────────────────────────────────────────────

fn pr_areas_cache_path(sha: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"));
    base.join("dragonfly")
        .join("pr-areas")
        .join(format!("{sha}.json"))
}

const PR_AREAS_INSTRUCTIONS: &str = r#"You are given the diff of a pull request. Make a list of the high-level areas that the PR covers.

If the PR is small, and there's only one area covered, then output only one area.
For each area, list a name, a description (a few sentances), and list the files or directories that are the most relevant.

The PR should be split into areas such that a single reviewer will have full context on everything in that area and can review it mostly independently of the other areas. If two changes need to be understood together to be reviewed correctly, they belong in the same area; if a reviewer of one change wouldn't need to look at the other to judge it, they belong in different areas. For medium and large PRs (more than ~5 files or ~200 changed lines), expect multiple distinct areas — splitting them is the default, not the exception.

# Output format

Output ONLY raw JSON. Do NOT wrap your output in markdown code fences (no ```json, no ```). The first character of your output must be `{` and the last character must be `}`. Output nothing — no prose, no commentary, no summary — before or after the JSON.

Every area object MUST contain ALL six required keys: `name`, `description`, `simplification_motivation`, `files`, `potential_for_bugs`, `potential_for_simplification`. Do not omit any key on any area, even if a value is short.

Include a potential_for_bugs estimate according to the following scale examples:

1. No code changed.
2. Minor changes that preserve existing semantics perfectly.
3. Changes to cli tools that aren't actually used in production.
4. Changes to non-critical services.
6. Non-trivial, but well encapsulated, changes to core services.
8. Non-trivial changes to core services affecting many parts of the codebase, making it very likely some part is missed.
10. Large scale changes or many subtle edge cases.

Focus on the potential for bugs causing issues in production.

Also include a simplification_motivation.
Specify if there's lots of duplicate code, large functions that should be broken down into smaller functions, or repetitive patterns that could be restructured to simplify the code.
You should base this on how much the code/changes can be simplified further, not how much this PR simplified things.
Afterwards, add a potential_for_simplification estimate from 1 to 10, that summarizes the reasoning.

## Example

{
    "areas": [
        {
            "name": "Frontend SSE streaming",
            "description": "Refactored the streaming logic of agent and user messages from a long-polling http endpoint to use websockets...",
            "simplification_motivation": "The functions fetchHistory and loadOlderEvents could be refactored to reduce duplication and improve readability.",
            "files": ["app/src/lib/trajectory", "app/proto/generated_types.ts"],
            "potential_for_bugs": 8,
            "potential_for_simplification": 5
        },
        {
            "name": "Fixed off-by-one error in backend trajectory endpoint",
            "description": "The Limit parameter had an off-by-one error which resulted in too many results being returned...",
            "simplification_motivation": "Very little code is touched, so there's minimal opportunity for simplification.",
            "files": ["go/api/endpoints.go"],
            "potential_for_bugs": 3,
            "potential_for_simplification": 1
        },
        {
            "name": "Updated all test mocks to behave like the websocket stream",
            "description": "Several test mocks...",
            "simplification_motivation": "The mocks use the same pattern, and could be broken down into reusable components.",
            "files": ["go/pkg/trajectory/message_test.go", "go/pkg/trajectory/streaming_test.go", "go/pkg/trajectory/hitl_test.go"],
            "potential_for_bugs": 5,
            "potential_for_simplification": 9
        }
    ]
}
"#;

/// Analyze the PR diff via Haiku 4.5. Caller provides `full_diff_str`
/// containing inlined per-file `<diff name="…">…</diff>` blocks (no tool
/// use is available, so the diff has to be in the prompt itself). Cached
/// on disk by HEAD SHA so repeated runs on the same commit are free.
async fn analyze_pr_areas(
    full_diff_str: &str,
    changed_files_str: &str,
    pr_commits_str: &str,
) -> Option<serde_json::Value> {
    let head_sha = sh("git rev-parse HEAD").await.unwrap_or_default();
    let cache_path = if !head_sha.is_empty() {
        Some(pr_areas_cache_path(&head_sha))
    } else {
        None
    };
    if let Some(path) = &cache_path {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                status::status_line!(
                    "   PR areas: cache hit ({}).",
                    &head_sha[..head_sha.len().min(7)]
                );
                return Some(v);
            }
        }
    }

    let content =
        format!("{pr_commits_str}\n{changed_files_str}\n\nPer-file diffs:\n{full_diff_str}\n");
    let content_file = section("pr-areas-input", &content);
    let raw = call_kit_llm(
        "anthropic/claude-haiku-4-5",
        PR_AREAS_INSTRUCTIONS,
        &content_file.path,
    )
    .await?;
    let parsed = extract_json_from_end(&raw)?;

    if let Some(path) = &cache_path {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(serialized) = serde_json::to_string_pretty(&parsed) {
            let _ = std::fs::write(path, serialized);
        }
    }

    Some(parsed)
}

fn extract_json_from_end(output: &str) -> Option<serde_json::Value> {
    let json_end = output.rfind('}')?;
    let bytes = output.as_bytes();
    let mut depth: i32 = 0;
    for i in (0..=json_end).rev() {
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return serde_json::from_str(&output[i..=json_end]).ok();
        }
    }
    None
}

// ── Graphite detection ───────────────────────────────────────────────────────

struct GraphiteInfo {
    stack_viz: String,
    stack_ci_status: String,
}

async fn get_graphite_trunk() -> String {
    // Use --git-common-dir so this works inside worktrees (where .git is a file).
    let Some(git_common_dir) = sh("git rev-parse --git-common-dir").await else {
        return "main".into();
    };
    let config_candidates = [
        PathBuf::from(&git_common_dir).join(".graphite_repo_config"),
        PathBuf::from(&git_common_dir).join("graphite_repo_config"),
    ];
    config_candidates
        .iter()
        .find(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| v.get("trunk").and_then(|s| s.as_str().map(String::from)))
        .unwrap_or_else(|| "main".to_string())
}

/// Parse branch names from `gt log short --stack` output. Since `--stack`
/// already restricts output to ancestors + descendants of the current branch,
/// we only need to strip the bullet chars and any trailing "(needs restack)" /
/// "(current, ...)" annotation.
fn parse_stack_branches(output: &str, trunk: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let line_before_paren = line.split('(').next().unwrap_or(line);
            let start = line_before_paren
                .char_indices()
                .find(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')?
                .0;
            let name = line_before_paren[start..].trim().to_string();
            if name.is_empty() || name == trunk {
                None
            } else {
                Some(name)
            }
        })
        .collect()
}

/// True if the current branch is tracked by Graphite (single branch or stack).
/// `gt log short --stack` exits non-zero on an untracked branch, so a clean exit
/// with at least one non-trunk branch means Graphite owns this branch's parent.
async fn is_graphite_branch() -> bool {
    let r = sh3("gt log short --stack 2>/dev/null").await;
    if r.code != 0 || r.stdout.is_empty() {
        return false;
    }
    let trunk = get_graphite_trunk().await;
    !parse_stack_branches(&r.stdout, &trunk).is_empty()
}

/// Returns (stack_viz, branches) if the current branch is part of a multi-branch
/// Graphite stack. Uses `gt log short --stack`, which limits output to the
/// current linear stack — no sibling-stack filtering needed.
async fn detect_graphite_stack(trunk: &str) -> Option<(String, Vec<String>)> {
    let r = sh3("gt log short --stack 2>/dev/null").await;
    if r.code != 0 || r.stdout.is_empty() {
        return None;
    }
    let branches = parse_stack_branches(&r.stdout, trunk);
    // A stack needs at least 2 non-trunk branches (current + ancestor/descendant).
    if branches.len() < 2 {
        return None;
    }
    Some((r.stdout, branches))
}

async fn branch_ci_status(branch: String, is_current: bool) -> String {
    let view_cmd = format!("gh pr view {branch} --json number 2>/dev/null");
    let checks_cmd = format!("gh pr checks {branch} 2>/dev/null");
    let (view_r, checks_r) = tokio::join!(sh3(&view_cmd), sh3(&checks_cmd));

    let marker = if is_current {
        " **(current — CI wait blocks here)**"
    } else {
        ""
    };

    let pr_num: Option<u64> = serde_json::from_str::<serde_json::Value>(&view_r.stdout)
        .ok()
        .and_then(|v| v.get("number").and_then(|n| n.as_u64()));

    let Some(pr) = pr_num else {
        return format!("- `{branch}`{marker} — no PR");
    };

    let counts = parse_checks(&checks_r.stdout, IGNORED_CHECKS);
    let mut parts = Vec::new();
    if counts.passed > 0 {
        parts.push(format!("{} passed", counts.passed));
    }
    if counts.failed > 0 {
        parts.push(format!("{} failing", counts.failed));
    }
    if counts.pending > 0 {
        parts.push(format!("{} pending", counts.pending));
    }
    let summary = if parts.is_empty() {
        "no checks yet".into()
    } else {
        parts.join(", ")
    };
    format!("- `{branch}`{marker} — PR #{pr} — {summary}")
}

async fn collect_stack_ci_status(branches: &[String], current: &str) -> String {
    let handles: Vec<_> = branches
        .iter()
        .map(|b| {
            let is_current = b == current;
            tokio::spawn(branch_ci_status(b.clone(), is_current))
        })
        .collect();

    let mut lines = Vec::new();
    for h in handles {
        if let Ok(line) = h.await {
            lines.push(line);
        }
    }
    lines.join("\n")
}

async fn build_graphite_info() -> Option<GraphiteInfo> {
    let trunk = get_graphite_trunk().await;
    let (stack_viz, branches) = detect_graphite_stack(&trunk).await?;
    let current = sh("git branch --show-current").await.unwrap_or_default();
    let stack_ci_status = collect_stack_ci_status(&branches, &current).await;
    Some(GraphiteInfo {
        stack_viz,
        stack_ci_status,
    })
}

/// Returns the git ref to compare HEAD against for "what's in this PR" diffs.
/// In a Graphite stack, returns `origin/<parent_branch>` (or the local branch
/// if origin/<parent_branch> is missing). Otherwise returns `origin/<trunk>`.
/// Merge-conflict checks deliberately stay against `origin/main` and use raw
/// strings rather than this helper.
async fn pr_base_ref() -> String {
    let trunk = get_graphite_trunk().await;
    let default = format!("origin/{trunk}");
    let Some((_, branches)) = detect_graphite_stack(&trunk).await else {
        return default;
    };
    let current = sh("git branch --show-current").await.unwrap_or_default();
    if current.is_empty() {
        return default;
    }
    let Some(idx) = branches.iter().position(|b| b == &current) else {
        return default;
    };
    let Some(parent) = branches.get(idx + 1) else {
        return default;
    };
    // Sanity: parent must be an ancestor of HEAD.
    if sh3(&format!("git merge-base --is-ancestor {parent} HEAD"))
        .await
        .code
        != 0
    {
        return default;
    }
    let remote = format!("origin/{parent}");
    if sh(&format!("git rev-parse --verify {remote}"))
        .await
        .is_some()
    {
        remote
    } else if sh(&format!("git rev-parse --verify {parent}"))
        .await
        .is_some()
    {
        parent.clone()
    } else {
        default
    }
}

fn graphite_section(info: &GraphiteInfo) -> String {
    format!(
        "\n## Graphite Stack\n\n\
         This branch is in a Graphite stack. Prefer `gt` over raw git so stack metadata stays in sync:\n\n\
         - `gt submit --no-edit --stack` — push/update the whole stack. `gt absorb` and `gt restack` rewrite ancestor/descendant commits, so pushing only the current branch would leave those PRs pointing at orphaned commits on GitHub. `gt submit` ignores `--title`/`--body`; use `gh pr edit` for those.\n\
         - `gt absorb --dry-run` → `gt absorb` — route a staged fix into the ancestor branch whose lines it touches, instead of piling a commit on the current branch.\n\
         - `gt restack` — rebase dependents after amending an ancestor or when trunk has moved. Don't use `gt get --force` here; it force-updates siblings from remote.\n\n\
         Current stack:\n\
         ```\n{}\n```\n\n\
         ### Stack PR CI status\n\n\
         **Only the current branch's CI blocks this run.** Ancestor-PR failures are informational — mention them in the final summary, but don't block on or fix them unless the user asks.\n\n\
         {}\n",
        info.stack_viz, info.stack_ci_status
    )
}

// ── Prompt building ──────────────────────────────────────────────────────────

// Settings JSON + Bash PreToolUse hooks are bundled with the binary. The
// template's hook commands use `__DRAGONFLY_HOOKS__` as a placeholder we
// substitute with the absolute hooks dir at runtime, so the file passed to
// `claude --settings ...` always points at the hooks shipped alongside this
// build of dragonfly.
const DRAGONFLY_SETTINGS_TEMPLATE: &str = include_str!("../settings/dragonfly-settings.json");
const DRAGONFLY_HOOKS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/hooks");

// Bundled like the settings template above: the guide references sibling
// files (pr-descriptions/, grafana_dashboards.md) via `__DRAGONFLY_ROOT__`,
// which we expand to the checkout dir at runtime so the file the agent reads
// contains resolvable absolute paths on any machine.
const PR_DESCRIPTION_GUIDE_TEMPLATE: &str = include_str!("../pr-description-guide.md");

fn pr_description_guide_expanded() -> String {
    let body =
        PR_DESCRIPTION_GUIDE_TEMPLATE.replace("__DRAGONFLY_ROOT__", env!("CARGO_MANIFEST_DIR"));
    let f = tempfile::Builder::new()
        .prefix("dragonfly-pr-description-guide-")
        .suffix(".md")
        .tempfile_in("/tmp")
        .expect("failed to create pr description guide tempfile");
    let (mut file, path) = f
        .keep()
        .expect("failed to persist pr description guide tempfile");
    file.write_all(body.as_bytes())
        .expect("failed to write pr description guide tempfile");
    path.to_string_lossy().into_owned()
}
const DOTENV_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.env");

fn dragonfly_settings_expanded() -> String {
    let body = DRAGONFLY_SETTINGS_TEMPLATE.replace("__DRAGONFLY_HOOKS__", DRAGONFLY_HOOKS_DIR);
    let f = tempfile::Builder::new()
        .prefix("dragonfly-settings-")
        .suffix(".json")
        .tempfile_in("/tmp")
        .expect("failed to create settings tempfile");
    let (mut file, path) = f.keep().expect("failed to persist settings tempfile");
    file.write_all(body.as_bytes())
        .expect("failed to write settings tempfile");
    path.to_string_lossy().into_owned()
}

// ── Custom subagent definitions ─────────────────────────────────────────────

// settings.json doesn't support an inline `agents` field — subagents have to
// come from `.claude/agents/`, `~/.claude/agents/`, a plugin's agents dir, or
// the `--agents` CLI flag. We use the CLI flag so a fresh checkout of any
// repo gets the dragonfly review subagents without writing files into the
// user's project. Agent bodies are bundled with the binary.
const REVIEW_AGENT_MD: &str = include_str!("../agents/review-agent.md");
const COMMENT_REVIEWER_MD: &str = include_str!("../agents/comment-reviewer.md");
const DEDUP_REVIEWER_MD: &str = include_str!("../agents/dedup-reviewer.md");
const TEST_REVIEWER_MD: &str = include_str!("../agents/test-reviewer.md");
// Bundled so the `@../code-comments.md` reference in agent bodies can be
// inlined at registration time. See [expand_bundled_refs].
const CODE_COMMENTS_MD: &str = include_str!("../code-comments.md");

/// Subagent definitions registered via `claude --agents`. Each is keyed in
/// the resulting object by its frontmatter `name`, which is the
/// `subagent_type` the parent agent passes to the Agent tool.
const BUNDLED_AGENTS: &[&str] = &[
    REVIEW_AGENT_MD,
    COMMENT_REVIEWER_MD,
    DEDUP_REVIEWER_MD,
    TEST_REVIEWER_MD,
];

/// Minimal YAML-frontmatter splitter for agent markdown files. Only handles
/// the subset our agent files actually use: a leading `---\n...\n---\n`
/// block of `key: value` lines (no nesting, no multi-line strings),
/// followed by the body. Panics on malformed input — the input is a
/// repo-bundled constant compiled in via [include_str!], so a parse failure
/// would mean a programming error, not a runtime data issue.
fn split_agent_frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let rest = text
        .strip_prefix("---\n")
        .expect("agent file must start with '---\\n'");
    let end = rest
        .find("\n---\n")
        .expect("agent file frontmatter must close with '\\n---\\n'");
    let yaml = &rest[..end];
    let body = rest[end + "\n---\n".len()..].trim_start_matches('\n');
    let mut fields = HashMap::new();
    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    (fields, body)
}

/// Expands `@<path>` references in `body`, replacing each with the content
/// `resolve(path)` returns, inlined between `--- begin/end <path> ---` markers.
/// A token whose `resolve` returns None is left verbatim (e.g. `@username` in
/// prose, or a path that points at no readable file).
///
/// Single source of truth for @-expansion. The bundled `--agents` registration
/// resolves against compiled-in [include_str!] content via
/// [expand_bundled_refs]; the `expand-agent` CLI resolves against on-disk files
/// via [expand_agent_file] for scripts/compare-comment-reviewers.sh.
fn expand_at_refs(body: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '/' | '-'))
            })
            .unwrap_or(after.len());
        let path = &after[..end];
        match resolve(path) {
            Some(content) if !path.is_empty() => out.push_str(&format!(
                "{path} (inlined below)\n\n--- begin {path} ---\n{}\n--- end {path} ---",
                content.trim_end(),
            )),
            // Bare `@`, or a token that resolves to nothing: keep verbatim.
            _ => {
                out.push('@');
                out.push_str(path);
            }
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Inlines the `@`-imports in a bundled agent body against compiled-in content.
///
/// Claude Code resolves a relative `@`-path against the subagent's cwd (the PR
/// repo being reviewed), not this repo's agents dir, so without this the
/// comment-reviewer's `Read: @../code-comments.md` misses and it burns turns
/// hunting for its guidelines. Bodies without the reference (review-agent) pass
/// through unchanged.
fn expand_bundled_refs(body: &str) -> String {
    expand_at_refs(body, |path| match path {
        "../code-comments.md" => Some(CODE_COMMENTS_MD.to_string()),
        _ => None,
    })
}

/// Resolves an `@`-reference path against `base`, honoring `~/` and absolute
/// paths; relative paths join `base` (the referencing file's directory).
fn resolve_ref_path(p: &str, base: &std::path::Path) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if p.starts_with('/') {
        return PathBuf::from(p);
    }
    base.join(p)
}

/// Reads an agent markdown file, strips its frontmatter, and inlines every
/// `@`-import (resolved against the file's own directory). Backs the hidden
/// `expand-agent` CLI so the comparison harness reuses [expand_at_refs] rather
/// than reimplementing it.
fn expand_agent_file(path: &std::path::Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    let (_, body) = split_agent_frontmatter(&text);
    let base = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    Ok(expand_at_refs(body, |ref_path| {
        std::fs::read_to_string(resolve_ref_path(ref_path, &base)).ok()
    }))
}

/// Parses one bundled agent markdown into its `(name, entry)` pair for the
/// `--agents` object. The entry follows the same schema as the frontmatter
/// (description / prompt / model / color), with `prompt` carrying the body
/// (after [expand_bundled_refs] inlines any `@`-import).
fn agent_json_entry(md: &str) -> (String, serde_json::Value) {
    let (meta, body) = split_agent_frontmatter(md);
    let name = meta.get("name").expect("agent missing `name`").clone();
    let mut entry = serde_json::Map::new();
    entry.insert(
        "description".into(),
        serde_json::Value::String(
            meta.get("description")
                .expect("agent missing `description`")
                .clone(),
        ),
    );
    entry.insert(
        "prompt".into(),
        serde_json::Value::String(expand_bundled_refs(body)),
    );
    if let Some(model) = meta.get("model") {
        entry.insert("model".into(), serde_json::Value::String(model.clone()));
    }
    if let Some(color) = meta.get("color") {
        entry.insert("color".into(), serde_json::Value::String(color.clone()));
    }
    (name, serde_json::Value::Object(entry))
}

/// Builds the JSON value passed via `claude --agents '<json>'`. The outer
/// object is keyed by agent name; each [BUNDLED_AGENTS] entry follows the
/// markdown-frontmatter schema, with `prompt` carrying the body. See
/// https://code.claude.com/docs/en/sub-agents (CLI-defined subagents).
fn build_agents_json() -> String {
    let mut outer = serde_json::Map::new();
    for md in BUNDLED_AGENTS {
        let (name, entry) = agent_json_entry(md);
        outer.insert(name, entry);
    }
    serde_json::to_string(&serde_json::Value::Object(outer)).expect("agent JSON must serialize")
}

fn load_dotenv() -> Vec<(String, String)> {
    let content = match std::fs::read_to_string(DOTENV_PATH) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
                || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
            {
                &v[1..v.len() - 1]
            } else {
                v
            };
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

const INITIAL_REVIEW_PROMPT: &str = r#"You are reviewing a pull request. The diff is inlined below as `<diff name="…">…</diff>` blocks.

Read the diff carefully and surface concrete issues a maintainer would want to know about before merging. Focus on correctness first — bugs, broken invariants, missing error/nil handling, subtle logic mistakes, resource leaks, race conditions, security holes. Then consider whether the change introduces unsafe edge cases, misuses an API, or breaks an assumption made elsewhere in the file. Note significant clarity or design problems if they're likely to cause real bugs, but don't pad the list with style nits.

Use your judgement: if a line in the diff looks suspicious but you can't confirm it's wrong from the diff alone, say so and explain what would need to be true for it to be a bug. Don't invent issues just to fill space. If you find no real problems, say so plainly.

For blocking issues (likely to make CI or testing fail or unambigiously need to be fixed), use severity "HIGH".

# Output format

A numbered markdown list. For each issue:

1. `path/to/file.ext:LINE` — one-sentence description of the issue.
   Why it matters: <one or two sentences>.
   Severity: LOW | MED | HIGH,
   Suggested fix: <one-line suggestion, or "needs investigation" if unclear>.

Use the line numbers as they appear in the diff hunks. If you find no issues worth flagging, output exactly: `No issues found.` and nothing else.
"#;

const REVIEW_AGGRESSIVE: &str = "\
This PR has high potential for bugs. Be thorough:
Trace through ALL code paths touched by this PR. Follow the call chains — don't just read the diff in isolation.
Spawn multiple `review-agent` subagents in parallel (one Agent tool call per concern) so different areas are reviewed simultaneously. Each subagent gets the <dragonfly-context> block via the SubagentStart hook — do NOT re-inline the diff or file index, just tell each subagent which area/concern to focus on.
Look for subtle issues: race conditions, missing error handling, incorrect assumptions about state, edge cases in new logic.
Leave no stone unturned — the goal is to be confident nothing was missed.

Use the potential_for_bugs field in the area breakdown as a guide for what to focus on in particular.

One of the subagents should be dedicated to finding code that can be simplified. Guide it using the potential_for_simplification score in the PR areas breakdown — focus it on the areas with the highest simplification potential.
";

const REVIEW_SIMPLIFICATION: &str = "\
This PR has areas with high simplification potential.
Spawn a dedicated `review-agent` subagent (subagent_type: review-agent) to review the code for simplification opportunities — duplicate code, large functions that should be broken down, or repetitive patterns that could be restructured.
Guide it using the potential_for_simplification score in the PR areas breakdown — focus it on the areas with the highest simplification potential. The <dragonfly-context> hook delivers the diff files automatically; you only need to specify the focus.
";

fn build_prompt(
    pr_status: &str,
    files_index: &str,
    skip_ci: &Option<String>,
    files: &[TempFile],
    ctx: &ContextStrings,
    diff_files_str: &str,
    prior_reviews_str: &str,
    review_instruction: &str,
    initial_review_str: &str,
    pr_areas_str: &str,
    pr_areas: &Option<serde_json::Value>,
    graphite_str: &str,
    agent_sessions_str: &str,
    relevant_context_str: &str,
    review_only_note: Option<&str>,
    push_code: i32,
) -> String {
    let notes = if let Some(reason) = skip_ci {
        format!(" CI was skipped due to {reason} — investigate those first.")
    } else if files
        .iter()
        .any(|f| f.path.to_string_lossy().contains("failures"))
    {
        " CI stopped at first failure; other checks may still be running.".into()
    } else {
        String::new()
    };

    let max_area_score = |field: &str| -> u64 {
        pr_areas
            .as_ref()
            .and_then(|v| v.get("areas"))
            .and_then(|a| a.as_array())
            .map(|areas| {
                areas
                    .iter()
                    .filter_map(|a| a.get(field).and_then(|v| v.as_u64()))
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    };

    let max_bug_potential = max_area_score("potential_for_bugs");
    let max_simplification = max_area_score("potential_for_simplification");

    let review_instructions = if max_bug_potential >= 6 {
        REVIEW_AGGRESSIVE
    } else if max_simplification >= 6 {
        REVIEW_SIMPLIFICATION
    } else {
        ""
    };
    let skill_text = skill::DRAGONFLY_SKILL
        .replace("CUSTOM_REVIEW_PLACEHOLDER", review_instructions)
        .replace(
            "PR_DESCRIPTION_GUIDE_PATH",
            &pr_description_guide_expanded(),
        )
        .replace("CODE_COMMENTS_PLACEHOLDER", skill::CODE_COMMENTS_GUIDE);

    let now = chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S %:z (%Z)")
        .to_string();
    let review_only_prefix = review_only_note.unwrap_or("");

    // The push already ran; surface a non-zero exit instead of claiming "done",
    // since a bare push against a misconfigured upstream (origin/main) 128s and
    // the fix would otherwise be left unpushed. See the Push Result section.
    let phase1 = if push_code == 0 {
        "Already done.".to_string()
    } else {
        format!(
            "⚠️ Push exited {push_code} — it likely FAILED (see the Push Result section below). \
             Re-push the branch (e.g. `git push -u origin HEAD:<branch>`) and confirm it succeeded \
             before continuing."
        )
    };

    format!(
        "{review_only_prefix}{skill_text}{graphite_str}\n\n\
         # Instructions\n\n\
         Current time: {now}\n\
         PR status: {pr_status}\n\
         {}{}{}\n\
         Per-file diffs:\n\
         {diff_files_str}{pr_areas_str}\n\
         {relevant_context_str}\n\
         Phase 1 (push):\n\
         {phase1}\n\n\
         Phase 2/3:\n\
         {notes}\n\n\
         Pre-collected data:\n\
         {files_index}\n\
         {agent_sessions_str}\
         {prior_reviews_str}{review_instruction}{initial_review_str}\n\
         Read only the files you need. Start with the smallest/most relevant ones.\n\n\
         Continue with the next relevant phase, and read the instructions carefully.\n",
        ctx.main_commits, ctx.pr_commits, ctx.changed_files,
    )
}

fn filter_relevant_files(paths: &[String]) -> Vec<&str> {
    paths
        .iter()
        .filter(|p| !p.ends_with("_gen.go") && !p.ends_with("_pb.ts"))
        .map(|s| s.as_str())
        .collect()
}

// ── Main ─────────────────────────────────────────────────────────────────────

async fn run_areas_only() {
    let start = std::time::Instant::now();
    let base_ref = pr_base_ref().await;
    let changed_files = get_changed_files(&base_ref).await;
    let relevant_changed_files = filter_relevant_files(&changed_files);
    // let diff_files_str = write_diff_files(&relevant_changed_files, &base_ref).await;
    let full_diff = full_diffs(&relevant_changed_files, &base_ref).await;
    let branch_commits = sh(&format!("git log {base_ref}..HEAD --oneline")).await;
    let ctx = collect_context_strings(&branch_commits, &base_ref).await;

    let full_diff_str = full_diff
        .iter()
        .map(|(name, diff)| format!("<diff name=\"{name}\">\n{diff}\n</diff>"))
        .collect::<Vec<_>>()
        .join("\n");

    println!("Analyzing PR areas...");
    let pr_areas = analyze_pr_areas(&full_diff_str, &ctx.changed_files, &ctx.pr_commits).await;
    match pr_areas {
        Some(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        None => println!("No areas found."),
    }
    println!("\nCompleted in {:.1}s", start.elapsed().as_secs_f64());
}

/// Prepended to the agent prompt in --non-interactive mode. Overrides the
/// skill's ask-the-user choreography; phase numbers refer to skill.rs.
const NON_INTERACTIVE_NOTE: &str = "\
# ⚠️ NON-INTERACTIVE MODE\n\n\
No user is watching this session. Never ask the user anything or wait for approval — every \
instruction below that says to ask, offer, or wait for the user is overridden by these rules:\n\
- Ambiguous push state (Phase 1): stop and report instead of asking. Never force-push unless the listed conditions are unambiguously met.\n\
- CI failures (Phase 4): fix failures caused by this PR. Leave unrelated/pre-existing failures alone and note them in the final summary.\n\
- Review findings (Phases 5/6): fix only findings you are highly confident are real and in scope. Post the remaining uncertain findings as ONE top-level PR comment via `dragonfly pr comment --body -` (markdown on stdin), grouped in red/orange/green sections — do not wait for the user to pick.\n\
- After fixing, commit and push immediately; re-run the review-agent at most once.\n\
- PR description (Phase 7): write or update it as instructed, without asking.\n\
- Phase 9 (ready for review): SKIP ENTIRELY. Never run `gh pr ready` and never add reviewers — the PR must remain a draft for manual review.\n\
- No AI-attribution footers: no `Co-Authored-By: Claude...` in commit messages, no \"Generated with Claude Code\" in PR bodies or comments.\n\
- Finish with the Phase 8 summary as your final message.\n";

struct ClaudeInvocation {
    prompt: String,
    settings: String,
    /// JSON value passed to `claude --agents`. Contains the bundled
    /// review subagent definitions so the parent agent can spawn them in
    /// Phase 6 without the user having to install anything.
    agents: String,
    path: String,
}

/// Abort the full dragonfly flow when the working tree state would make the
/// pre-collected push/diff/CI data describe the wrong commits: a detached HEAD
/// or an in-progress rebase. Both leave HEAD off the PR branch, so pushing and
/// diffing against it silently mis-report (e.g. "1 commit" for a 12-commit PR
/// paused mid-fixup). Exits the process; run before [push].
async fn assert_head_runnable() {
    // `--git-path` resolves correctly inside linked worktrees too.
    for dir in ["rebase-merge", "rebase-apply"] {
        let Some(p) = sh(&format!("git rev-parse --git-path {dir}")).await else {
            continue;
        };
        if !p.trim().is_empty() && std::path::Path::new(p.trim()).exists() {
            eprintln!(
                "❌ A rebase is in progress ({dir}). Finish it (`git rebase --continue`) or \
                 abort it (`git rebase --abort`) before running dragonfly — data collected \
                 mid-rebase describes the wrong commits."
            );
            std::process::exit(1);
        }
    }
    // Detached HEAD: `git symbolic-ref -q HEAD` exits non-zero off a branch.
    if sh3("git symbolic-ref -q HEAD").await.code != 0 {
        eprintln!(
            "❌ HEAD is detached. Check out the PR branch before running dragonfly — \
             a detached HEAD isn't the branch that gets pushed or reviewed."
        );
        std::process::exit(1);
    }
}

async fn build_claude_invocation(
    force: bool,
    non_interactive: bool,
    title_flag: Option<String>,
    user_message: Option<String>,
) -> ClaudeInvocation {
    assert_head_runnable().await;
    // Resolve PR ownership before push() so the rebase prompt inside
    // maybe_rebase_on_main can be suppressed when the PR belongs to
    // someone else (review-only mode).
    let bg_pr = sh_bg("gh pr view --json number,url,isDraft,author 2>/dev/null");
    let bg_me = sh_bg("gh api user --jq .login 2>/dev/null");
    let early_pr_info = lookup_existing_pr(bg_pr).await;
    let viewer_login = sh_wait(bg_me).await.unwrap_or_default();
    let review_only = early_pr_info.as_ref().is_some_and(|p| {
        !p.author_login.is_empty() && !viewer_login.is_empty() && p.author_login != viewer_login
    });
    if review_only {
        let pr = early_pr_info.as_ref().unwrap();
        println!(
            "👀 PR owned by @{} (you are @{}) — review-only mode (no auto-rebase, no fixes).",
            pr.author_login, viewer_login,
        );
    }

    let (push_result, merge_probe) = push(force, review_only, non_interactive).await;
    if push_result.code != 0 {
        println!("⚠️  Push had issues: {}", push_result.stderr);
    }

    let mut graphite_handle = Some(tokio::spawn(build_graphite_info()));

    let base_ref = pr_base_ref().await;
    if base_ref != "origin/main" {
        println!("   Diffing against `{base_ref}` (stack parent).");
    }
    let branch_commits = sh(&format!("git log {base_ref}..HEAD --oneline")).await;

    // Score relevant CLAUDE.md/AGENTS.md chunks for the PR diff in
    // parallel — `lov eval rag score` is the slowest leg (~10 s), worth
    // overlapping with CI watch + area analysis.
    let base_ref_for_rag = base_ref.clone();
    let relevant_context_handle =
        tokio::spawn(async move { build_relevant_context(&base_ref_for_rag).await });

    // Dedup hints overlap with CI watch too — a cold summary cache can take
    // many minutes, but the CI wait dominates and a warm cache is seconds.
    let base_ref_for_dedup = base_ref.clone();
    let dedup_handle =
        tokio::spawn(async move { dedup::build_hints_file(&base_ref_for_dedup).await });

    // Run area analysis in parallel with PR/CI checks. The PR-areas LLM
    // call needs the inline diff content (kit has no tool-use, so we
    // can't point it at file paths and ask it to Read). Meanwhile the
    // main agent prompt only needs the file-paths index so it can choose
    // which diffs to load — both are computed here.
    let branch_commits_clone = branch_commits.clone();
    let base_ref_clone = base_ref.clone();
    let mut areas_handle = Some(tokio::spawn(async move {
        let changed_files = get_changed_files(&base_ref_clone).await;
        let relevant_changed_files = filter_relevant_files(&changed_files);
        let diff_files_str = write_diff_files(&relevant_changed_files, &base_ref_clone).await;
        let full_diffs = full_diffs(&relevant_changed_files, &base_ref_clone).await;
        let full_diff_str = full_diffs
            .iter()
            .map(|(name, diff)| format!("<diff name=\"{name}\">\n{diff}\n</diff>"))
            .collect::<Vec<_>>()
            .join("\n");
        let ctx = collect_context_strings(&branch_commits_clone, &base_ref_clone).await;

        let pr_areas = analyze_pr_areas(&full_diff_str, &ctx.changed_files, &ctx.pr_commits).await;
        (pr_areas, diff_files_str, ctx)
    }));

    // Launch independent checks in parallel. The merge-tree probe was
    // already run inside push() (it drives the rebase decision); reuse it.
    let bg_status = sh_bg("git status -b --porcelain=v2");

    let git_status = sh_wait(bg_status).await.unwrap_or_default();
    let push_content = build_push_content(&push_result, &git_status);
    let mut files = vec![section("push", &push_content)];

    let merge = build_merge_content(merge_probe).await;
    files.push(section("merge", &merge.content));

    let mut pre_areas: Option<(Option<serde_json::Value>, String, ContextStrings)> = None;
    let mut pre_graphite: Option<Option<GraphiteInfo>> = None;
    let pr_info = if let Some(pr) = early_pr_info {
        pr
    } else {
        println!("   No PR found — creating draft PR...");
        // Prompt for the title first so the user isn't blocked by the
        // subagent. `prompt_pr_title` masks SIGCHLD on the prompt thread so
        // a background child exiting won't interrupt dialoguer's read(2).
        let title = if non_interactive {
            non_interactive_pr_title(title_flag.as_deref(), &branch_commits)
        } else {
            prompt_pr_title(&branch_commits)
        };
        // Drain the subagent and graphite handles after the prompt; the
        // results are needed by the rest of build_claude_invocation either
        // way, and waiting now keeps the later `pre_*` paths tidy.
        if let Some(h) = areas_handle.take() {
            pre_areas = h.await.ok();
        }
        if let Some(h) = graphite_handle.take() {
            pre_graphite = Some(h.await.ok().flatten());
        }
        match title {
            Some(t) => create_pr_with_title(&t).await,
            None => PrInfo {
                number: None,
                url: None,
                is_draft: false,
                author_login: String::new(),
            },
        }
    };

    // Flip the agent into review-only mode when the PR belongs to someone
    // else. Detected up front (see early_pr_info / viewer_login); the
    // rebase prompt was already suppressed in push() when [review_only].
    let review_only_note = if review_only {
        Some(format!(
            "⚠️ REVIEW-ONLY MODE\n\
             This PR is owned by @{} — not you (@{}). Your job is to REVIEW it, not push fixes.\n\
             - Do NOT implement fixes unless the user explicitly asks for them.\n\
             - Focus on Phase 5 (review bot comments) and Phase 6 (custom review).\n\
             - Surface findings as a numbered list; let the user pick which to post as a PR comment.\n\
             - After user approves, post a single top-level PR comment with the review findings via `dragonfly pr comment --body -` (pipe the markdown on stdin to avoid shell-quoting the multi-line body). Group findings in red/orange/green sections (use colored dots).\n\
             - Skip Phase 7 (PR description) and Phase 9 (ready for review).\n\
             - When CI fails, report it; do not start fixing it.\n\n",
            pr_info.author_login, viewer_login,
        ))
    } else {
        None
    };

    // If this PR has no prior review log, kick off an initial code review
    // via Gemini in parallel with collect_reviews_and_ci. Gemini is weaker
    // than Claude, so its output is framed as a first-pass hint for the
    // main agent — see the heading in build_prompt.
    let initial_review_handle = match &pr_info.number {
        Some(pr_num) if !has_prior_review_logs(pr_num) => {
            println!("   No prior review log — running initial code review via gemini.");
            let base_ref_clone = base_ref.clone();
            Some(tokio::spawn(async move {
                run_initial_review(&base_ref_clone, Model::Gemini).await
            }))
        }
        _ => None,
    };

    // Spawn the CI+review-thread collection as a separate task so the
    // initial review can race it. If the review reports a HIGH severity
    // issue while CI is still in flight, we abort the local watch (remote
    // CI keeps running) so the agent goes straight to the blocker.
    let ci_handle: Option<tokio::task::JoinHandle<CiResult>> = match (&pr_info.number, &pr_info.url)
    {
        (Some(pr_num), Some(pr_url)) => {
            let pr_num = pr_num.clone();
            let pr_url = pr_url.clone();
            let branch = push_result.branch.clone();
            let base_ref_clone = base_ref.clone();
            let has_conflicts = merge.has_conflicts;
            Some(tokio::spawn(async move {
                collect_reviews_and_ci(&pr_num, &pr_url, &branch, has_conflicts, &base_ref_clone)
                    .await
            }))
        }
        _ => None,
    };

    let mut skip_ci: Option<String> = None;
    let mut failed_names: Vec<String> = Vec::new();
    let mut initial_review_body: Option<String> = None;
    let mut ci_aborted_by_review = false;

    match (initial_review_handle, ci_handle) {
        (Some(mut review_h), Some(mut ci_h)) => {
            tokio::select! {
                review = &mut review_h => {
                    let body = review.ok().flatten();
                    if body.as_deref().is_some_and(review_has_high_severity) {
                        println!("⚠️  Initial review uncovered blocking issue - skipping waiting for CI");
                        ci_h.abort();
                        ci_aborted_by_review = true;
                        skip_ci = Some("initial review flagged a HIGH severity issue".into());
                        files.push(section(
                            "ci",
                            "# CI\n\n⚠️ Local CI watch was aborted because the initial code review \
                             flagged a HIGH severity issue. Address that issue first.\n\nRemote CI is \
                             still running — the next push will collect fresh CI state.\n",
                        ));
                    } else if let Ok(ci) = ci_h.await {
                        files.extend(ci.files);
                        skip_ci = ci.skip_ci;
                        failed_names = ci.failed_names;
                    }
                    initial_review_body = body;
                }
                ci = &mut ci_h => {
                    if let Ok(ci) = ci {
                        files.extend(ci.files);
                        skip_ci = ci.skip_ci;
                        failed_names = ci.failed_names;
                    }
                    initial_review_body = review_h.await.ok().flatten();
                }
            }
        }
        (Some(review_h), None) => {
            initial_review_body = review_h.await.ok().flatten();
        }
        (None, Some(ci_h)) => {
            if let Ok(ci) = ci_h.await {
                files.extend(ci.files);
                skip_ci = ci.skip_ci;
                failed_names = ci.failed_names;
            }
        }
        (None, None) => {}
    }

    // Spawned right after push; by now the CI wait has usually absorbed the
    // dedup latency, so this await is close to free.
    let dedup_hints = dedup_handle.await.ok().flatten();
    let dedup_funcs = dedup_hints.as_ref().map(|h| h.funcs);
    if let Some(h) = dedup_hints {
        files.push(TempFile {
            path: h.path,
            lines: h.lines,
        });
    }

    let files_index = build_files_index(&files, merge.has_conflicts, &failed_names, dedup_funcs);

    let (prior_reviews, review_instruction) = get_review_log_context(&pr_info.number);
    // The review prompt instructs the model to output exactly "No issues found."
    // when it has nothing to flag — drop the heading and skip writing the file
    // entirely in that case so the agent isn't distracted by a no-op section.
    let initial_review_file = initial_review_body
        .filter(|body| !body.contains("No issues found"))
        .map(|body| section("initial-review", &body));
    let abort_note = if ci_aborted_by_review {
        "\n⚠️ This review flagged a HIGH severity issue and the local CI watch was \
         aborted as a result. Investigate and fix the HIGH severity issue first — \
         remote CI is still running and will be re-checked on the next push.\n"
    } else {
        ""
    };
    let initial_review_str = initial_review_file
        .as_ref()
        .map(|tf| format!(
            "\n# Initial code review\n{abort_note}\n\
             An initial code review has been done focusing on easy to surface bugs. Read this after you have understood the PR, but before running any other review agents.\n\
             This will likely contain several 'obvious' bugs that are good to fix so that the review agents can focus on the deeper issues. Validate all bugs before trying to fix them. Handle like other bot feedback.\n\
             Obviously incorrect code can be fixed immediately, but surface other issues to the user as a numbered list so that they can decide which to fix and which to leave.\n\
             As this initial review is more surface level, skip surfacing false-positive review findings to the user. If all are false-positives, continue immediately to the next phase.\n\n\
             - `{}` ({} lines) — initial code review\n",
            tf.path.display(),
            tf.lines,
        ))
        .unwrap_or_default();
    let pr_status = if pr_info.is_draft {
        "draft"
    } else if pr_info.number.is_some() {
        "ready for review"
    } else {
        "none"
    };

    println!("   Analyzing PR areas...");
    let (pr_areas, diff_files_str, ctx) = match pre_areas {
        Some(v) => v,
        None => areas_handle.take().unwrap().await.unwrap(),
    };
    let pr_areas_str = pr_areas
        .as_ref()
        .map(|v| {
            format!(
                "\nPR area analysis:\n```json\n{}\n```\n",
                serde_json::to_string_pretty(v).unwrap()
            )
        })
        .unwrap_or_default();

    let graphite_info = match pre_graphite {
        Some(v) => v,
        None => graphite_handle.take().unwrap().await.ok().flatten(),
    };
    if graphite_info.is_some() {
        println!("   Graphite stack detected — including stack workflow + per-PR CI status.");
    }
    let graphite_str = graphite_info
        .as_ref()
        .map(graphite_section)
        .unwrap_or_default();

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let agent_sessions = sessions::find_recent_sessions(&cwd, &push_result.branch);
    if !agent_sessions.is_empty() {
        println!(
            "   Including {} recent agent session{} for branch `{}`.",
            agent_sessions.len(),
            if agent_sessions.len() == 1 { "" } else { "s" },
            push_result.branch,
        );
    }
    let agent_sessions_str = sessions::render_section(&agent_sessions, &push_result.branch);

    let relevant_context_str = relevant_context_handle.await.unwrap_or_default();

    let prompt = build_prompt(
        pr_status,
        &files_index,
        &skip_ci,
        &files,
        &ctx,
        &diff_files_str,
        &prior_reviews,
        &review_instruction,
        &initial_review_str,
        &pr_areas_str,
        &pr_areas,
        &graphite_str,
        &agent_sessions_str,
        &relevant_context_str,
        review_only_note.as_deref(),
        push_result.code,
    );
    let prompt = if non_interactive {
        format!("{NON_INTERACTIVE_NOTE}\n{prompt}")
    } else {
        prompt
    };
    // Last so it wins recency: guidance like "skip Phase 6" must not be
    // buried under the pre-collected data sections above.
    let prompt = match &user_message {
        Some(msg) => format!(
            "{prompt}\n\
             The user has provided additional guidance which may be relevant for the review:\n\
             <user-guidance>\n{msg}\n</user-guidance>\n"
        ),
        None => prompt,
    };

    // Put our own binary on PATH so the agent can call dragonfly subcommands
    let path = {
        let current = std::env::var("PATH").unwrap_or_default();
        match std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        {
            Some(dir) => format!("{}:{current}", dir.display()),
            None => current,
        }
    };

    let settings = dragonfly_settings_expanded();
    let agents = build_agents_json();
    ClaudeInvocation {
        prompt,
        settings,
        agents,
        path,
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Some(msg) = cli.feedback {
        submit_feedback(&msg);
        return;
    }

    if let Some(command) = cli.command {
        match command {
            CliCommand::Pr {
                command: PrCommand::Thread { command },
            } => match command {
                ThreadCommand::Comment { thread_id, body } => {
                    pr_thread_comment(&thread_id, &body).await;
                }
                ThreadCommand::Resolve { thread_id } => {
                    pr_thread_resolve(&thread_id).await;
                }
            },
            CliCommand::Pr {
                command: PrCommand::Description { body, pr },
            } => {
                pr_set_description(pr, &body).await;
            }
            CliCommand::Pr {
                command: PrCommand::Comment { pr, body },
            } => {
                pr_comment(pr, &body).await;
            }
            CliCommand::Pr {
                command: PrCommand::Comments { pr },
            } => {
                pr_comments(pr).await;
            }
            CliCommand::Pr {
                command: PrCommand::Review { model },
            } => {
                let code = pr_review_cmd(model).await;
                std::process::exit(code);
            }
            CliCommand::Ci { command } => {
                let code = match command {
                    CiCommand::Status { all, pr } => {
                        ci_status_cmd(pr, all, &[QA_TECH_REVIEW, CLAUDE_CODE_REVIEW]).await
                    }
                    CiCommand::Failures {
                        pr,
                        max_bytes,
                        raw,
                        model,
                    } => ci_failures_cmd(pr, max_bytes, raw, model).await,
                    CiCommand::Watch { pr } => ci_watch_cmd(pr).await,
                    CiCommand::Flaky { name, limit } => ci_flaky_cmd(name, limit).await,
                    CiCommand::Retries { pr } => ci_retries_cmd(pr).await,
                    CiCommand::Rerun { name, pr } => ci_rerun_cmd(name, pr).await,
                    CiCommand::Distill { file, model } => ci_distill_cmd(file, model).await,
                };
                std::process::exit(code);
            }
            CliCommand::Dedup {
                command,
                threshold,
                limit,
                base,
                json,
            } => {
                let code = match command {
                    None => dedup::cmd_list(base, threshold, limit, json).await,
                    Some(DedupCommand::Dismiss { func, matches }) => {
                        dedup::cmd_dismiss(func, matches, base, threshold, limit).await
                    }
                    Some(DedupCommand::Exclusions { json }) => dedup::cmd_exclusions(json).await,
                    Some(DedupCommand::Sync { full }) => dedup::cmd_sync(full).await,
                };
                std::process::exit(code);
            }
            CliCommand::Prompt { target: None } => {
                let invocation =
                    build_claude_invocation(cli.force, cli.non_interactive, cli.title, cli.message)
                        .await;
                println!("{}", invocation.prompt);
            }
            CliCommand::Prompt {
                target: Some(PromptTarget::InitialReview),
            } => {
                print_initial_review_prompt().await;
            }
            CliCommand::Prompt {
                target: Some(PromptTarget::ReviewAgent { inline_diffs }),
            } => {
                let code = review_agent_prompt_cmd(inline_diffs).await;
                std::process::exit(code);
            }
            CliCommand::Prompt {
                target: Some(PromptTarget::DedupReviewer),
            } => {
                let code = dedup_reviewer_prompt_cmd().await;
                std::process::exit(code);
            }
            CliCommand::Guides { paths } => {
                let paths = if paths.is_empty() {
                    let mut buf = String::new();
                    use std::io::Read as _;
                    let _ = std::io::stdin().read_to_string(&mut buf);
                    buf.lines()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(PathBuf::from)
                        .collect::<Vec<_>>()
                } else {
                    paths
                };
                for g in collect_relevant_guides(&paths) {
                    println!("{}", g.display());
                }
            }
            CliCommand::ScoreGuides {
                output,
                base,
                threshold,
            } => {
                score_guides_cmd(output, base, threshold).await;
            }
            CliCommand::ExpandAgent { file } => match expand_agent_file(&file) {
                Ok(s) => print!("{s}"),
                Err(e) => {
                    eprintln!("expand-agent: {}: {e}", file.display());
                    std::process::exit(1);
                }
            },
            CliCommand::WatchMcp { pr, interval, demo } => {
                let code = watch_mcp::watch_mcp_cmd(pr, interval, demo).await;
                std::process::exit(code);
            }
        }
        return;
    }

    if cli.areas {
        run_areas_only().await;
        return;
    }

    let non_interactive = cli.non_interactive;
    let invocation =
        build_claude_invocation(cli.force, non_interactive, cli.title, cli.message).await;

    if non_interactive {
        println!("   Running Claude Code (print mode, non-interactive)...\n");
        let mut cmd = std::process::Command::new("claude");
        cmd.args([
            "-p",
            "--dangerously-skip-permissions",
            "--settings",
            &invocation.settings,
            "--agents",
            &invocation.agents,
        ])
        .arg(&invocation.prompt)
        .env("PATH", &invocation.path);
        for (k, v) in load_dotenv() {
            cmd.env(k, v);
        }
        // Exit code is the contract with orchestrators driving this mode:
        // claude's own failure (or a missing binary) must not read as success.
        let code = match cmd.status() {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => {
                eprintln!("Failed to run claude: {e}");
                1
            }
        };
        std::process::exit(code);
    }

    println!("   Launching Claude Code...\n");
    let mut cmd = std::process::Command::new("claude");
    cmd.args([
        "--dangerously-skip-permissions",
        "--settings",
        &invocation.settings,
        "--agents",
        &invocation.agents,
    ])
    .arg(&invocation.prompt)
    .env("PATH", &invocation.path);
    for (k, v) in load_dotenv() {
        cmd.env(k, v);
    }
    let err = cmd.exec();
    eprintln!("Failed to exec claude: {err}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sha_checks_buckets_and_dedup() {
        let runs = r#"{"check_runs": [
            {"id": 1, "name": "test-go", "status": "completed", "conclusion": "failure", "html_url": "https://github.com/x/y/actions/runs/1/job/1"},
            {"id": 2, "name": "test-go", "status": "in_progress", "conclusion": null, "html_url": "https://github.com/x/y/actions/runs/2/job/2"},
            {"id": 3, "name": "lint", "status": "completed", "conclusion": "success"},
            {"id": 4, "name": "docs", "status": "completed", "conclusion": "skipped"},
            {"id": 5, "name": "e2e", "status": "completed", "conclusion": "cancelled"},
            {"id": 6, "name": "queued-job", "status": "queued", "conclusion": null},
            {"id": 7, "name": "superseded", "status": "completed", "conclusion": "stale"}
        ]}"#;
        let statuses = r#"{"state": "pending", "statuses": [
            {"context": "buildkite/app", "state": "pending", "target_url": "https://buildkite.com/o/p/builds/1"},
            {"context": "wiz", "state": "success"},
            {"context": "spacelift/stack", "state": "error"}
        ]}"#;
        let checks = parse_sha_checks(runs, statuses);
        let bucket = |name: &str| {
            checks
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.bucket.as_str())
                .unwrap_or("missing")
        };
        // Newest run (id 2, in_progress) wins over the older failed attempt.
        assert_eq!(bucket("test-go"), "pending");
        assert_eq!(bucket("lint"), "pass");
        assert_eq!(bucket("docs"), "skipping");
        // cancelled/stale are held apart from `fail` so `ci watch` debounces a
        // superseded run instead of fail-fasting on a phantom.
        assert_eq!(bucket("e2e"), "cancelled");
        assert_eq!(bucket("superseded"), "stale");
        assert_eq!(bucket("queued-job"), "pending");
        assert_eq!(bucket("buildkite/app"), "pending");
        assert_eq!(bucket("wiz"), "pass");
        assert_eq!(bucket("spacelift/stack"), "fail");
        assert_eq!(checks.len(), 9);
    }

    #[test]
    fn annotate_diff_numbers_new_lines_only() {
        // Two hunks; deleted lines unnumbered, added/context lines carry their
        // new-file number, and the counter reseeds from the second @@ header.
        let diff = "\
diff --git a/f.rs b/f.rs
index 111..222 100644
--- a/f.rs
+++ b/f.rs
@@ -10,3 +10,3 @@ fn f() {
 ctx a
-old line
+new line
@@ -40,2 +50,3 @@ fn g() {
 ctx b
+added tail";
        let got = annotate_diff_new_line_numbers(diff);
        let expected = "\
diff --git a/f.rs b/f.rs
index 111..222 100644
--- a/f.rs
+++ b/f.rs
@@ -10,3 +10,3 @@ fn f() {
10:  ctx a
-old line
11: +new line
@@ -40,2 +50,3 @@ fn g() {
50:  ctx b
51: +added tail
";
        assert_eq!(got, expected);
    }

    #[test]
    fn bundled_comment_reviewer_inlines_code_comments() {
        let v: serde_json::Value = serde_json::from_str(&build_agents_json()).unwrap();
        // Regression: the relative @-import must be inlined, not left dangling
        // (Claude Code would resolve it against the PR repo's cwd and miss).
        for agent in ["comment-reviewer", "test-reviewer"] {
            let prompt = v[agent]["prompt"].as_str().unwrap();
            assert!(
                !prompt.contains("@../code-comments.md"),
                "dangling @-ref left in {agent} prompt"
            );
            assert!(
                prompt.contains("Explain why, never what"),
                "code-comments.md not inlined into {agent} prompt"
            );
        }
        // review-agent carries no such ref and is registered unchanged.
        assert!(
            v["review-agent"]["prompt"]
                .as_str()
                .unwrap()
                .contains("review")
        );
    }

    #[test]
    fn parse_hunk_new_start_reads_plus_side() {
        assert_eq!(parse_hunk_new_start(" -10,3 +50,3 @@ fn g() {"), Some(50));
        assert_eq!(parse_hunk_new_start(" -1 +1 @@"), Some(1));
        assert_eq!(parse_hunk_new_start(" garbage"), None);
    }

    #[test]
    fn parse_sha_checks_tolerates_api_errors() {
        assert!(parse_sha_checks("", "").is_empty());
        assert!(parse_sha_checks(r#"{"message": "Not Found"}"#, "gh: error").is_empty());
    }

    #[test]
    fn counts_from_checks_respects_ignore_list() {
        let mk = |name: &str, bucket: &str| PrCheck {
            name: name.into(),
            bucket: bucket.into(),
            link: String::new(),
            workflow: String::new(),
            description: String::new(),
        };
        let checks = vec![
            mk("test-go", "pass"),
            mk("test-spanner", "pending"),
            mk(GRAPHITE_MERGEABILITY, "pending"),
            mk("deploy", "fail"),
        ];
        let counts = counts_from_checks(&checks, WATCH_IGNORED_CHECKS);
        assert_eq!(counts.passed, 1);
        assert_eq!(counts.failed, 0);
        assert_eq!(counts.pending, 1);
        assert_eq!(counts.pending_names, vec!["test-spanner".to_string()]);
    }

    #[test]
    fn eval_and_approval_checks_do_not_block_the_watch() {
        let mk = |name: &str, bucket: &str| PrCheck {
            name: name.into(),
            bucket: bucket.into(),
            link: String::new(),
            workflow: String::new(),
            description: String::new(),
        };
        let checks = vec![
            mk("test-go", "pass"),
            // Matrix-generated eval names vary per PR; only the prefix is stable.
            mk(
                "Run relevant eval (kimi-reasoning-budget, go/api/pkg/providers/evals/kimi-reasoning-budget.yaml) / Run kimi-reasoning-budget eval",
                "pending",
            ),
            mk(EVAL_SELECT, "pending"),
            mk(EVAL_STATUS, "pending"),
            mk(HIGH_RISK_APPROVAL, "pending"),
        ];
        let counts = counts_from_checks(&checks, WATCH_IGNORED_CHECKS);
        assert_eq!(counts.passed, 1);
        assert_eq!(counts.pending, 0);
        assert!(counts.pending_names.is_empty());
    }

    #[test]
    fn is_ignored_check_matches_prefix_entries() {
        assert!(is_ignored_check(
            "Run relevant eval (foo, bar.yaml) / Run foo eval",
            &[EVAL_RUN_PREFIX]
        ));
        assert!(is_ignored_check("deploy", &["deploy"]));
        assert!(!is_ignored_check("deploy-extra", &["deploy"]));
    }

    #[test]
    fn counts_from_checks_separates_cancelled_and_stale() {
        let mk = |name: &str, bucket: &str| PrCheck {
            name: name.into(),
            bucket: bucket.into(),
            link: String::new(),
            workflow: String::new(),
            description: String::new(),
        };
        let checks = vec![
            mk("test-go", "pass"),
            mk("lint-go-result", "cancelled"),
            mk("test-result", "cancelled"),
            mk("e2e", "stale"),
        ];
        let counts = counts_from_checks(&checks, WATCH_IGNORED_CHECKS);
        // Cancelled/stale do not inflate `failed` (the phantom-failure bug);
        // cancelled names are sorted for the cross-poll settle comparison.
        assert_eq!(counts.failed, 0);
        assert_eq!(counts.stale, 1);
        assert_eq!(
            counts.cancelled_names,
            vec!["lint-go-result".to_string(), "test-result".to_string()]
        );
    }

    #[test]
    fn classify_issue_comment_tags_boilerplate_and_keeps_signal() {
        // Lovmesh plan preview is a CI-equivalent signal — never collapsed.
        assert_eq!(
            classify_issue_comment(
                "github-actions[bot]",
                "## Lovmesh Plan Preview\n❌ apply failed"
            ),
            ("bot-status", false)
        );
        // Codecov / pr-classification boilerplate collapses.
        assert!(classify_issue_comment("codecov[bot]", "coverage 80%").1);
        assert!(classify_issue_comment("lovable-ci-bot", "PR Classification: trivial").1);
        // Human comments are kept in full.
        assert_eq!(
            classify_issue_comment("aron", "this needs a second look"),
            ("comment", false)
        );
    }

    #[test]
    fn denoise_drops_checkout_noise_and_keeps_the_failure() {
        // `actions/checkout` ref enumeration + working-tree progress, then the
        // real failing step. Format mirrors `gh run view --log`: job\tstep\tts.
        let log = [
            "job\tCheckout PR head\t2026Z  * [new branch]            fix-error-handling -> origin/fix-error-handling",
            "job\tCheckout PR head\t2026Z  * [new branch]            saml-delete-orphan-user-on-join-fail -> origin/saml-delete-orphan-user-on-join-fail",
            "job\tCheckout PR head\t2026Z  + abc1234...def5678 b -> origin/b (forced update)",
            "job\tCheckout PR head\t2026Z  - [deleted]               (none) -> origin/gone",
            "job\tCheckout PR head\t2026Z  * [new tag]               v1.2.3 -> v1.2.3",
            "job\tCheckout PR head\t2026Z Updating files:  53% (1234/2345)",
            "job\tCheckout PR head\t2026Z Updating files: 100% (2345/2345), done.",
            "job\tRun codeowners PR check\t2026Z [fail] 1 added file(s) have NO matching CODEOWNERS rule:",
            "job\tRun codeowners PR check\t2026Z   - go/api/pkg/reviewpublish/strip_test.go",
            "job\tRun codeowners PR check\t2026Z ##[error]Process completed with exit code 1.",
        ]
        .join("\n");
        let out = denoise_gha_log(&log);
        // Ref enumeration and checkout progress are stripped...
        assert!(!out.contains("[new branch]"));
        assert!(!out.contains("fix-error-handling"));
        assert!(!out.contains("forced update"));
        assert!(!out.contains("[deleted]"));
        assert!(!out.contains("[new tag]"));
        assert!(!out.contains("Updating files"));
        // ...but the actual failure survives verbatim.
        assert!(out.contains("NO matching CODEOWNERS rule"));
        assert!(out.contains("go/api/pkg/reviewpublish/strip_test.go"));
        assert!(out.contains("Process completed with exit code 1"));

        // Regression: branch names containing "error"/"fail" no longer seed
        // extract_failure_summary's keep window, so the summary stays tight and
        // surfaces the real error instead of a wall of `-> origin/<branch>`.
        let summary = extract_failure_summary(&out);
        assert!(summary.contains("strip_test.go"));
        assert!(!summary.contains("origin/"));
        assert!(
            summary.len() < 600,
            "summary ballooned to {} bytes",
            summary.len()
        );
    }

    #[test]
    fn truncate_tail_keeps_the_end_where_failures_live() {
        // Under the cap: returned unchanged, no marker.
        assert_eq!(truncate_tail("short", 100), "short");
        // Over the cap: the tail (where `##[error]` lands) is kept; the
        // setup-noise head is dropped behind a marker.
        let long = format!("{}##[error]boom", "x".repeat(500));
        let out = truncate_tail(&long, 20);
        assert!(out.starts_with("…[earlier log truncated]…\n"));
        assert!(out.ends_with("##[error]boom"));
        assert!(!out.contains(&"x".repeat(22)));
    }
}
