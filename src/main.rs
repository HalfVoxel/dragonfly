use chrono::DateTime;
use clap::{Parser, Subcommand};
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

mod sessions;
mod skill;

#[derive(Parser)]
#[command(name = "push-and-check")]
struct Cli {
    /// Force push (e.g. after rebase)
    #[arg(long)]
    force: bool,

    /// Only run PR area analysis and print the result
    #[arg(long)]
    areas: bool,

    /// Submit feedback about push-and-check itself (appended to ~/.dragonfly/feedback)
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
    /// Run the full flow (push, CI wait, data collection) but print the prompt
    /// instead of invoking Claude Code. Useful for debugging / iterating on the
    /// prompt itself.
    Prompt,
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
    },
    /// Print PR review threads, top-level reviews, and metadata in the same
    /// cleaned format used by the pre-collected data (review-threads,
    /// review-pr, pr-meta). Defaults to the current branch's PR.
    Comments {
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
    },
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
    /// a per-check section with extracted error lines, then the link. Full logs are
    /// saved to /tmp/ when available.
    Failures {
        /// Explicit PR number. Defaults to the current branch's PR.
        #[arg(long)]
        pr: Option<String>,
        /// Maximum bytes of full log per check (default 8000).
        #[arg(long, default_value = "8000")]
        max_bytes: usize,
    },
    /// Wrap `gh pr checks --watch --fail-fast` with auto-reconnect; print a single
    /// final summary (same shape as `ci status`).
    Watch {
        /// Explicit PR number. Defaults to the current branch's PR.
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
}

struct PrInfo {
    number: Option<String>,
    url: Option<String>,
    is_draft: bool,
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
    failed_names: Vec<String>,
    lint_files: Vec<TempFile>,
}

struct FailureLogs {
    content: String,
    names: Vec<String>,
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
    let lines = content.lines().count() + usize::from(!content.ends_with('\n') && !content.is_empty());
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
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string();
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

    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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
            "--app-name=push-and-check",
            "--urgency=critical",
            &format!("--icon={icon}"),
            "push-and-check feedback",
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
async fn maybe_rebase_on_main(has_upstream: bool, merge_probe: &ShResult) -> bool {
    let behind = sh("git rev-list --count HEAD..origin/main")
        .await
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if behind == 0 {
        return false;
    }

    let dirty = sh("git status --porcelain --untracked-files=no").await.unwrap_or_default();
    if !dirty.is_empty() {
        println!("   Branch is {behind} behind origin/main; skipping auto-rebase (working tree dirty).");
        return false;
    }

    if merge_probe.code != 0 {
        println!("   Branch is {behind} behind origin/main; skipping auto-rebase (would conflict).");
        return false;
    }

    let proceed = if !has_upstream {
        println!("   Branch is {behind} behind origin/main — rebasing (new branch)...");
        true
    } else {
        let prompt = format!("Branch is {behind} behind origin/main and rebase is clean. Rebase now?");
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

async fn push(force: bool) -> (PushResult, ShResult) {
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
    let bg_merge = sh_bg("git merge-tree --write-tree --name-only origin/main HEAD");
    let upstream = sh("git rev-parse --abbrev-ref @{upstream} 2>/dev/null").await;
    let merge_probe = sh3_wait(bg_merge).await;

    let rebased = maybe_rebase_on_main(upstream.is_some(), &merge_probe).await;
    let force = force || rebased;
    // After a successful rebase HEAD sits on top of origin/main, so the
    // pre-rebase merge probe is stale. The new state is trivially clean.
    let merge_probe = if rebased {
        ShResult { code: 0, stdout: String::new(), stderr: String::new() }
    } else {
        merge_probe
    };

    if upstream.is_none() {
        println!("   No upstream — pushing with -u...");
        let r = sh3("git push -u origin HEAD").await;
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
            Some((parts.next()?.parse::<i64>().ok()?, parts.next()?.parse::<i64>().ok()?))
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
    let cmd = if needs_force { "git push --force-with-lease" } else { "git push" };
    println!("   {kind} ({label})...");
    let r = sh3(cmd).await;
    (
        PushResult {
            branch,
            strategy: if needs_force { "force-with-lease" } else { "fast-forward" },
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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
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
        let author = r.author.as_ref().map(|a| a.login.as_str()).unwrap_or("unknown");
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
                let author = c.author.as_ref().map(|a| a.login.as_str()).unwrap_or("unknown");
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
    meta: Option<String>,
    has_unresolved: bool,
}

async fn fetch_pr_comments(owner: &str, repo: &str, pr_number: &str) -> PrCommentsBundle {
    let query_escaped = REVIEW_THREADS_QUERY.replace('\'', "'\\''");
    let bg_threads = sh_bg(&format!(
        "gh api graphql -f query='{query_escaped}' -f owner={owner} -f repo={repo} -F pr={pr_number}"
    ));
    let bg_pr_view = sh_bg(&format!(
        "gh pr view {pr_number} --json title,body,reviewDecision,reviews,reviewRequests"
    ));

    let mut bundle = PrCommentsBundle {
        threads_xml: None,
        threads_raw_json: None,
        reviews_xml: None,
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
        sections.push((
            "review-threads (raw JSON — XML parse failed)",
            raw,
        ));
    }

    if sections.is_empty() {
        eprintln!("No review threads, reviews, or metadata found for PR #{pr_number}.");
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
    let decision = meta.review_decision.as_deref().filter(|s| !s.is_empty()).unwrap_or("none");
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

async fn pr_thread_comment(thread_id: &str, body: &str) {
    let signed = format!("{body}\n\n<sup>via Dragonfly (Claude)</sup>");
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

async fn pr_set_description(body_arg: &str) {
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

    let pr_number = match sh("gh pr view --json number --jq '.number'").await {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            eprintln!("Failed to find a PR for the current branch.");
            std::process::exit(1);
        }
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
        eprintln!("gh pr edit failed: {}", String::from_utf8_lossy(&out.stderr).trim());
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

// Checks that are slow, flaky, or non-blocking — exclude from the wait so they
// don't keep `pending` above zero forever. Failures here are surfaced to the
// user but not auto-fixed as part of push-and-check.
const IGNORED_CHECKS: &[&str] = &["Cursor Bugbot", "test-e2e", "doc-review", "deploy", "Graphite / mergeability_check"];

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

fn parse_checks(out: &str) -> CheckCounts {
    let mut checks: HashMap<&str, (u64, &str)> = HashMap::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            let name = parts[0].trim();
            if IGNORED_CHECKS.contains(&name) {
                continue;
            }
            let status = parts[1].trim();
            let run_id = if parts.len() >= 4 { run_id_from_url(parts[3]) } else { 0 };
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
        linters.push(("lint-web".into(), sh_bg("cd app && pnpm install --silent && lint-web")));
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
                    .map(|o| (
                        String::from_utf8_lossy(&o.stdout).trim().to_string(),
                        String::from_utf8_lossy(&o.stderr).trim().to_string(),
                    ))
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
        let mut c = parse_checks(&r.stdout);
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
        let changed_dirs: std::collections::HashSet<&str> = changed
            .iter()
            .filter_map(|f| f.split('/').next())
            .collect();
        linters = start_local_lints(&changed_dirs);
        if !linters.is_empty() {
            let names: Vec<_> = linters.iter().map(|(n, _)| n.as_str()).collect();
            println!("   Running locally: {}", names.join(", "));
        }
    }

    let rc;
    if counts.failed > 0 {
        println!("   ❌ {} failed, ✅ {} passed", counts.failed, counts.passed);
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
            counts = parse_checks(&r.stdout);
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
            failed_names: vec![],
            lint_files,
        };
    }

    if lint_results.iter().any(|lr| lr.code != 0) {
        println!("❌ Local lint failures detected");
        return CiWaitResult {
            ci_content,
            failures_content: None,
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
        failed_names: logs.names,
        lint_files,
    }
}

// ── Failure logs ─────────────────────────────────────────────────────────────

fn extract_failure_summary(log: &str) -> String {
    let re = Regex::new(
        r"(?i)FAIL|--- FAIL|panic:|Error:|error:|ERROR|fatal:|undefined:|cannot |could not |timed out|exit status"
    ).unwrap();

    log.lines()
        .filter_map(|line| {
            let text = line.splitn(4, '\t').last().unwrap_or(line);
            if re.is_match(text) { Some(text.trim()) } else { None }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Deserialize)]
struct RunInfo {
    #[serde(rename = "databaseId")]
    database_id: u64,
    name: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "headSha")]
    head_sha: Option<String>,
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
    let job = Regex::new(r"/job/(\d+)").unwrap()
        .captures(link)
        .and_then(|c| c.get(1)?.as_str().parse().ok());
    let run = Regex::new(r"/actions/runs/(\d+)").unwrap()
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
    Some((c.get(1)?.as_str().into(), c.get(2)?.as_str().into(), c.get(3)?.as_str().parse().ok()?))
}

async fn list_failed_checks(pr_number: &str) -> Vec<PrCheck> {
    let r = sh3(&format!(
        "gh pr checks {pr_number} --json name,bucket,link,workflow,description"
    )).await;
    let mut all: Vec<PrCheck> = parse_json(&r.stdout).unwrap_or_default();
    all.retain(|c| c.bucket == "fail" && !IGNORED_CHECKS.contains(&c.name.as_str()));
    all
}

/// Fetch the failing job's log via `gh run view`. Prefer per-job mode when the
/// check link points at a specific job; that avoids dumping every failing job
/// in a workflow when only one is the target.
async fn fetch_gha_log(check: &PrCheck) -> String {
    let (run_id, job_id) = parse_gha_link(&check.link);
    let cmd = match job_id {
        Some(j) => format!("gh run view --job={j} --log-failed"),
        None if run_id != 0 => format!("gh run view {run_id} --log-failed"),
        _ => return String::new(),
    };
    let log = sh3(&cmd).await;
    if !log.stdout.is_empty() { log.stdout } else { log.stderr }
}

/// Buildkite logs require `BUILDKITE_API_TOKEN`. Without one, surface the URL +
/// any check-run output GitHub already stored, so the agent isn't left blind.
async fn fetch_buildkite_log(check: &PrCheck, head_sha: &str) -> String {
    let parsed = parse_buildkite_link(&check.link);
    let token = std::env::var("BUILDKITE_API_TOKEN").ok().filter(|t| !t.is_empty());

    if let (Some((org, pipeline, number)), Some(tok)) = (parsed.clone(), token) {
        let api = format!(
            "https://api.buildkite.com/v2/organizations/{org}/pipelines/{pipeline}/builds/{number}?include_retried_jobs=true"
        );
        let r = sh3(&format!(
            "curl -sf -H 'Authorization: Bearer {tok}' {api:?}"
        )).await;
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
    if std::env::var("BUILDKITE_API_TOKEN").ok().filter(|t| !t.is_empty()).is_none() {
        parts.push(
            "(Set BUILDKITE_API_TOKEN to fetch full Buildkite logs. \
             Otherwise open the URL above.)".into(),
        );
    }
    parts.join("\n")
}

#[derive(Deserialize)]
struct BkBuild {
    #[allow(dead_code)] number: u64,
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
        let failed = j.state.as_deref() == Some("failed")
            || j.exit_status.map(|e| e != 0).unwrap_or(false);
        if !failed { continue; }
        let name = j.name.clone().unwrap_or_else(|| "unnamed".into());
        let log = if let Some(url) = j.raw_log_url.as_ref() {
            let r = sh3(&format!("curl -sf -H 'Authorization: Bearer {token}' {url:?}")).await;
            if r.code == 0 { r.stdout } else { String::new() }
        } else {
            String::new()
        };
        let summary = extract_failure_summary(&log);
        out.push(format!(
            "### Buildkite job: {name} (id={})\nExit: {}\n```\n{}\n```\n",
            j.id.unwrap_or_default(),
            j.exit_status.map(|e| e.to_string()).unwrap_or_else(|| "?".into()),
            if summary.is_empty() { truncate(&log, 4000).to_string() } else { summary },
        ));
    }
    out.join("\n")
}

/// `gh api repos/.../check-runs` returns `output.title` / `output.summary` /
/// `output.text` for many providers (Buildkite, Wiz, Spacelift). Use it as a
/// fallback so the agent always gets *something* even when we can't fetch the
/// provider's full log.
async fn fetch_check_run_output(head_sha: &str, name: &str) -> String {
    if head_sha.is_empty() { return String::new(); }
    let r = sh3(&format!(
        "gh api 'repos/{{owner}}/{{repo}}/commits/{head_sha}/check-runs?per_page=100' \
         --jq '.check_runs[] | select(.name == \"{}\")'",
        name.replace('"', "\\\"")
    )).await;
    if r.code != 0 || r.stdout.is_empty() { return String::new(); }
    // Take the first check-run if there are multiple (retries).
    let first_obj = r.stdout.split("\n}\n{").next().unwrap_or(&r.stdout);
    let parsed: serde_json::Value = match serde_json::from_str(first_obj) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let mut parts = Vec::new();
    for k in ["title", "summary", "text"] {
        if let Some(s) = parsed.pointer(&format!("/output/{k}")).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                parts.push(format!("**{k}**: {}", truncate(s, 2000)));
            }
        }
    }
    parts.join("\n")
}

async fn fetch_external_log(check: &PrCheck, head_sha: &str) -> String {
    let mut parts = vec![format!("External check ({}): {}", classify_provider_label(&check.link), check.link)];
    if !check.description.is_empty() {
        parts.push(check.description.clone());
    }
    let cr = fetch_check_run_output(head_sha, &check.name).await;
    if !cr.is_empty() { parts.push(cr); }
    parts.join("\n")
}

fn classify_provider_label(link: &str) -> &'static str {
    if link.contains("buildkite.com/") { "buildkite" }
    else if link.contains("spacelift.io") { "spacelift" }
    else if link.contains("wiz.io") { "wiz" }
    else if link.contains("mintlify.com") { "mintlify" }
    else if link.contains("depthfirst.com") { "depthfirst" }
    else if link.contains("github.com") { "github" }
    else { "unknown" }
}

fn strip_ansi(s: &str) -> String {
    // GitHub Actions' log API returns ESC bytes rendered as literal "^[" pairs;
    // normalize back to real ESC so strip_ansi_escapes handles them.
    let normalized = s.replace("^[", "\x1b");
    let stripped = strip_ansi_escapes::strip(normalized.as_bytes());
    String::from_utf8(stripped).unwrap_or(normalized)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s }
    else {
        // Slice at a UTF-8 boundary.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) { end -= 1; }
        &s[..end]
    }
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
        println!("      Fetching log for {} ({})...", check.name, provider_label);
        let raw = match provider {
            CheckProvider::GitHubActions => fetch_gha_log(check).await,
            CheckProvider::Buildkite => fetch_buildkite_log(check, head_sha).await,
            CheckProvider::External => fetch_external_log(check, head_sha).await,
        };
        names.push(check.name.clone());
        let summary = if matches!(provider, CheckProvider::GitHubActions) {
            extract_failure_summary(&raw)
        } else {
            // For non-GHA, the "raw" text is already a curated summary.
            raw.clone()
        };
        let header = format!("### {} ({})", check.name, provider_label);
        let link_line = if check.link.is_empty() { String::new() } else { format!("\nLink: {}", check.link) };
        summaries.push(format!("{header}{link_line}\n```\n{}\n```",
            if summary.trim().is_empty() { "(no extracted error lines)" } else { summary.trim() }));
        if matches!(provider, CheckProvider::GitHubActions) && !raw.is_empty() {
            let body = truncate(&raw, 16000);
            full_logs.push(format!("## {} — full log\n```\n{}\n```", check.name, body));
        }
    }

    if failed.is_empty() {
        // Defensive: gh pr checks --json returned nothing failed but caller
        // believed there were failures. Don't leave the file empty — point at
        // the live `gh pr checks` output.
        summaries.push("(no failing checks reported by `gh pr checks --json`; \
                        run `push-and-check ci status` to investigate)".into());
    }

    FailureLogs {
        content: format!(
            "# CI Failure Logs\n\n## Error Summary\n\n{}\n\n---\n\n# Full Logs\n\n{}\n",
            summaries.join("\n\n"),
            full_logs.join("\n\n"),
        ),
        names,
    }
}

// ── CI subcommands ───────────────────────────────────────────────────────────

async fn resolve_pr_number(pr: Option<String>) -> Option<String> {
    if let Some(p) = pr { return Some(p); }
    let s = sh("gh pr view --json number --jq '.number'").await?;
    if s.is_empty() { None } else { Some(s) }
}

async fn ci_status_cmd(pr: Option<String>, all: bool) -> i32 {
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    let r = sh3(&format!(
        "gh pr checks {pr_number} --json name,bucket,link,workflow,description"
    )).await;
    let mut checks: Vec<PrCheck> = parse_json(&r.stdout).unwrap_or_default();
    // Dedup by name, keeping the highest-priority bucket: fail > pending > pass > skipping.
    let priority = |b: &str| match b { "fail" => 3, "pending" => 2, "pass" => 1, _ => 0 };
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
    // Always count totals from full set.
    let all_checks: Vec<PrCheck> = parse_json(&r.stdout).unwrap_or_default();
    let mut by_name: HashMap<String, &PrCheck> = HashMap::new();
    for c in &all_checks {
        // Keep the highest-priority row per name.
        let keep = by_name.get(&c.name).map(|p| priority(&c.bucket) > priority(&p.bucket)).unwrap_or(true);
        if keep { by_name.insert(c.name.clone(), c); }
    }
    for c in by_name.values() {
        match c.bucket.as_str() {
            "fail" => failed += 1,
            "pending" => pending += 1,
            "pass" => passed_total += 1,
            _ => skipped_total += 1,
        }
    }

    println!("PR #{pr_number} — {} fail, {} pending, {} pass, {} skip",
        failed, pending, passed_total, skipped_total);
    for c in &checks {
        let icon = match c.bucket.as_str() {
            "fail" => "❌",
            "pending" => "⏳",
            "pass" => "✅",
            _ => "⏭",
        };
        let provider = classify_provider_label(&c.link);
        let link = if c.link.is_empty() { String::new() } else { format!("  {}", c.link) };
        println!("{icon} [{provider:>10}] {}{link}", c.name);
    }
    if failed > 0 { 1 } else { 0 }
}

async fn ci_failures_cmd(pr: Option<String>, max_bytes: usize) -> i32 {
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    let head_sha = sh("git rev-parse HEAD").await.unwrap_or_default();
    let failed = list_failed_checks(&pr_number).await;
    if failed.is_empty() {
        println!("No failing checks for PR #{pr_number}.");
        return 0;
    }
    println!("# Failing checks for PR #{pr_number} ({} total)\n", failed.len());
    for check in &failed {
        let provider = classify_provider(&check.link);
        let provider_label = classify_provider_label(&check.link);
        println!("## {} ({})", check.name, provider_label);
        if !check.link.is_empty() { println!("Link: {}", check.link); }
        let raw = match provider {
            CheckProvider::GitHubActions => fetch_gha_log(check).await,
            CheckProvider::Buildkite => fetch_buildkite_log(check, &head_sha).await,
            CheckProvider::External => fetch_external_log(check, &head_sha).await,
        };
        let raw = strip_ansi(&raw);
        let body = if matches!(provider, CheckProvider::GitHubActions) {
            let s = extract_failure_summary(&raw);
            if s.is_empty() {
                truncate(&raw, max_bytes).to_string()
            } else {
                s
            }
        } else {
            truncate(&raw, max_bytes).to_string()
        };
        if body.trim().is_empty() {
            println!("(no extracted error lines)\n");
        } else {
            println!("```\n{}\n```\n", body.trim());
        }
    }
    1
}

async fn ci_watch_cmd(pr: Option<String>) -> i32 {
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    // Use --watch --fail-fast and retry on dropped connection up to 3 times.
    let mut attempts = 0;
    loop {
        let r = sh3(&format!("gh pr checks {pr_number} --watch --fail-fast")).await;
        attempts += 1;
        // gh exits 0 when all pass, 8 when some fail (and shows final state).
        // Treat any other exit (1, broken pipe, etc.) as a dropped connection.
        if r.code == 0 || r.code == 8 || attempts >= 4 {
            // Print final summary in the ci-status shape, regardless of exit code.
            let _ = ci_status_cmd(Some(pr_number.clone()), false).await;
            return if r.code == 0 { 0 } else { 1 };
        }
        eprintln!("gh watch exited {} (attempt {attempts}/4); reconnecting in 5s...", r.code);
        sleep(std::time::Duration::from_secs(5)).await;
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
        let conclusion = if conclusion.is_empty() { "no-run".to_string() } else { conclusion };
        match conclusion.as_str() {
            "success" => pass += 1,
            "failure" | "cancelled" | "timed_out" => fail += 1,
            "skipped" | "neutral" => skip += 1,
            "no-run" => skip += 1,
            _ => other += 1,
        }
        rows.push(format!("{} {}{}", &sha[..7.min(sha.len())], conclusion, if url.is_empty() { String::new() } else { format!("  {url}") }));
    }
    println!("Check `{name}` on last {limit} commits of origin/main:");
    println!("  ✅ {pass} pass    ❌ {fail} fail    ⏭ {skip} skip/none    ? {other} other\n");
    for row in &rows { println!("{row}"); }
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
    )).await.unwrap_or_default();
    let branch = sh(&format!(
        "gh pr view {pr_number} --json headRefName --jq '.headRefName'"
    )).await.unwrap_or_default();
    if branch.is_empty() {
        eprintln!("Could not determine PR head branch.");
        return 2;
    }
    let r = sh3(&format!(
        "gh run list --branch {branch} --limit 50 \
         --json databaseId,name,headSha,conclusion,status,attempt,createdAt"
    )).await;
    let mut runs: Vec<GhRun> = parse_json(&r.stdout).unwrap_or_default();
    runs.retain(|r| r.head_sha == head_sha);

    println!("# Workflow runs for PR #{pr_number} (head {})",
        &head_sha[..7.min(head_sha.len())]);
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
        r.conclusion.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| r.status.clone())
    }

    // Sort: retried runs (attempt > 1) first, then by name, then by createdAt desc.
    runs.sort_by(|a, b| {
        b.attempt.cmp(&a.attempt)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    let name_w = runs.iter().map(|r| r.name.len()).max().unwrap_or(4).min(50);
    let result_w = runs.iter().map(|r| result_str(r).len()).max().unwrap_or(7);

    println!("{:<name_w$}  {:>3}  {:<result_w$}  {:<11}  {}",
        "NAME", "ATT", "RESULT", "TIME", "RUN_ID");
    for r in &runs {
        let marker = if r.attempt > 1 { " ← retried" } else { "" };
        println!("{:<name_w$}  {:>3}  {:<result_w$}  {:<11}  {}{}",
            r.name, r.attempt, result_str(r), short_time(&r.created_at),
            r.database_id, marker);
    }

    let retried = runs.iter().filter(|r| r.attempt > 1).count();
    if retried > 0 {
        println!("\n{retried} run(s) retried (attempt > 1). Avoid rerunning these without asking the user.");
    }
    0
}

async fn ci_rerun_cmd(name: String, pr: Option<String>) -> i32 {
    let Some(pr_number) = resolve_pr_number(pr).await else {
        eprintln!("No PR for current branch and --pr not supplied.");
        return 2;
    };
    let head_sha = sh(&format!(
        "gh pr view {pr_number} --json headRefOid --jq '.headRefOid'"
    )).await.unwrap_or_default();
    let branch = sh(&format!(
        "gh pr view {pr_number} --json headRefName --jq '.headRefName'"
    )).await.unwrap_or_default();
    // Match by check name → GHA run ID. We need to look up the workflow file
    // for the failing job, not the job ID, since rerun --failed acts on a run.
    let runs_json = sh3(&format!(
        "gh run list --branch {branch} --limit 50 \
         --json databaseId,name,headSha,conclusion \
         --jq '[.[] | select(.headSha == \"{head_sha}\" and .conclusion == \"failure\")]'"
    )).await;
    let runs: Vec<RunInfo> = parse_json(&runs_json.stdout).unwrap_or_default();
    if runs.is_empty() {
        eprintln!("No failing GHA runs for {name} on head {head_sha}. \
                   (Non-GHA checks like Buildkite cannot be rerun via this command.)");
        return 2;
    }
    // Try exact match first, then prefix.
    let target = runs.iter().find(|r| r.name.as_deref() == Some(name.as_str()))
        .or_else(|| runs.iter().find(|r| r.name.as_deref().map(|n| n.contains(&name)).unwrap_or(false)));
    let Some(run) = target else {
        eprintln!("No failing run named `{name}`. Failing runs:");
        for r in &runs { eprintln!("  - {}", r.name.as_deref().unwrap_or("?")); }
        return 2;
    };
    println!("Re-running failed jobs in {} (run {})...", run.name.as_deref().unwrap_or("?"), run.database_id);
    let r = sh3(&format!("gh run rerun {} --failed", run.database_id)).await;
    if !r.stdout.is_empty() { println!("{}", r.stdout); }
    if r.code != 0 {
        eprintln!("{}", r.stderr);
        return r.code;
    }
    0
}

// ── Merge conflict check ─────────────────────────────────────────────────────

async fn build_merge_content(r: ShResult) -> MergeResult {
    println!("   Checking for merge conflicts with origin/main...");
    let mut content = format!("# Merge Conflict Check\n\nExit code: {}\n", r.code);
    if !r.stdout.is_empty() {
        content += &format!("```\n{}\n```\n", r.stdout);
    }
    if !r.stderr.is_empty() {
        content += &format!("Stderr:\n```\n{}\n```\n", r.stderr);
    }

    let has_conflicts = r.code != 0;
    if has_conflicts {
        println!("⚠️  Potential merge conflicts detected");
        if let Some(base) = sh("git merge-base HEAD origin/main").await {
            if let Some(commits) = sh(&format!("git log --oneline {base}..origin/main")).await {
                content += &format!(
                    "\n## Recent commits on main since merge-base\n```\n{commits}\n```\n"
                );
            }
        }
    } else {
        println!("✅ No merge conflicts");
    }
    MergeResult { content, has_conflicts }
}

// ── PR handling ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PrData {
    number: u64,
    url: String,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
}

async fn lookup_existing_pr(bg_pr: Child) -> Option<PrInfo> {
    let pr_data: Option<PrData> = sh_wait(bg_pr).await.and_then(|s| parse_json(&s));
    pr_data.map(|pr| {
        println!("🔗 {}", pr.url);
        PrInfo {
            number: Some(pr.number.to_string()),
            url: Some(pr.url),
            is_draft: pr.is_draft,
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
        .map(|line| line.split_once(' ').map(|(_, t)| t).unwrap_or(line).to_string())
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
    let title = match with_sigchld_blocked(|| input.interact_text()) {
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

async fn create_pr_with_title(title: &str) -> PrInfo {
    let rc = std::process::Command::new("gh")
        .args(["pr", "create", "--draft", "--title", title, "--body", ""])
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(1);

    if rc == 0 {
        if let Some(data) = sh("gh pr view --json number,url,isDraft")
            .await
            .and_then(|s| parse_json::<PrData>(&s))
        {
            return PrInfo {
                number: Some(data.number.to_string()),
                url: Some(data.url),
                is_draft: true,
            };
        }
    } else {
        println!("⚠️  PR creation failed");
    }
    PrInfo { number: None, url: None, is_draft: false }
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
        let mut counts = parse_checks(&first.stdout);
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
                first.code, strip_skipping(&first.stdout)
            );
            files.push(section("ci", &ci_content));
        } else {
            let ci = wait_for_ci(pr_number, branch, Some((counts, first.code, first.stdout)), base_ref).await;
            files.push(section("ci", &ci.ci_content));
            if let Some(ref failures) = ci.failures_content {
                files.push(section("failures", failures));
            }
            files.extend(ci.lint_files);
            failed_names = ci.failed_names;
        }
    }

    CiResult { files, has_unresolved, skip_ci, failed_names }
}

// ── Context collection ───────────────────────────────────────────────────────

async fn collect_context_strings(branch_commits: &Option<String>, base_ref: &str) -> ContextStrings {
    let diff_cmd = format!("git diff --stat {base_ref}...HEAD");
    let (diff, main) = tokio::join!(
        sh(&diff_cmd),
        sh("git log HEAD..origin/main --oneline --grep='build: automatic update of go-api' --invert-grep"),
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

    ContextStrings { changed_files, main_commits, pr_commits }
}

// ── Build files index ────────────────────────────────────────────────────────

fn build_files_index(files: &[TempFile], has_conflicts: bool, failed_names: &[String]) -> String {
    let failures_label = if failed_names.is_empty() {
        "CI failure logs (error summary at top, full logs below)".into()
    } else {
        format!(
            "CI failure logs (error summary at top, full logs below): {}",
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
        ("review-threads", "review threads — inline review + bot comments)".into()),
        ("review-pr", "top-level PR reviews".into()),
        ("pr-meta", "PR title, body, review decision, requested reviewers".into()),
        ("ci", "CI check results".into()),
        ("failures", failures_label),
        ("lint", "local lint failures".into()),
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
        let label = labels.get(prefix.as_str()).map(|s| s.as_str()).unwrap_or(&prefix);
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
    let mut result  = Vec::<(&str,String)>::new();
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

// ── PR area analysis ─────────────────────────────────────────────────────────

fn pr_areas_cache_path(sha: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"));
    base.join("dragonfly").join("pr-areas").join(format!("{sha}.json"))
}

async fn analyze_pr_areas(
    diff_files_str: &str,
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
                println!("   PR areas: cache hit ({}).", &head_sha[..head_sha.len().min(7)]);
                return Some(v);
            }
        }
    }

    let prompt = format!(
        r#"
{pr_commits_str}
{changed_files_str}

Per-file diffs:
{diff_files_str}

# Instructions

Explore the changes done in this PR and make a list of the high-level areas that are covered.
Read the per-file diff files to understand the changes.
If the PR is small, and there's only one area covered, then output only one area.
For each area, list a name, a description (a few sentances), and list the files or directories that are the most relevant.
Format the output as json. Include only json and nothing else.

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

```
{{
    "areas": [
        {{
            "name": "Frontend SSE streaming",
            "description": "Refactored the streaming logic of agent and user messages from a long-polling http endpoint to use websockets...",
            "simplification_motivation": "The functions fetchHistory and loadOlderEvents could be refactored to reduce duplication and improve readability.",
            "files": ["app/src/lib/trajectory", "app/proto/generated_types.ts"],
            "potential_for_bugs": 8,
            "potential_for_simplification": 5
        }},
        {{
            "name": "Fixed off-by-one error in backend trajectory endpoint",
            "description": "The Limit parameter had an off-by-one error which resulted in too many results being returned...",
            "simplification_motivation": "Very little code is touched, so there's minimal opportunity for simplification.",
            "files": ["go/api/endpoints.go"],
            "potential_for_bugs": 3,
            "potential_for_simplification": 1
        }},
        {{
            "name": "Updated all test mocks to behave like the websocket stream",
            "description": "Several test mocks...",
            "simplification_motivation": "The mocks use the same pattern, and could be broken down into reusable components.",
            "files": ["go/pkg/trajectory/message_test.go", "go/pkg/trajectory/streaming_test.go", "go/pkg/trajectory/hitl_test.go"],
            "potential_for_bugs": 5,
            "potential_for_simplification": 9
        }}
    ]
}}
```
"#
    );

    let settings = push_and_fix_settings_expanded();
    let result = Command::new("claude")
        .args([
            "--print",
            "--dangerously-skip-permissions",
            "--model",
            "haiku",
            "--tools",
            "Bash,Edit,Glob,Grep,Read,Write",
            "--settings",
            &settings,
            "--system-prompt",
            &prompt,
            "Analyze the PR",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;

    let output = String::from_utf8_lossy(&result.stdout).trim().to_string();
    let parsed = extract_json_from_end(&output)?;

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

    let counts = parse_checks(&checks_r.stdout);
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
    Some(GraphiteInfo { stack_viz, stack_ci_status })
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
    if sh3(&format!("git merge-base --is-ancestor {parent} HEAD")).await.code != 0 {
        return default;
    }
    let remote = format!("origin/{parent}");
    if sh(&format!("git rev-parse --verify {remote}")).await.is_some() {
        remote
    } else if sh(&format!("git rev-parse --verify {parent}")).await.is_some() {
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
// build of push-and-check.
const PUSH_AND_FIX_SETTINGS_TEMPLATE: &str =
    include_str!("../settings/push-and-check-settings.json");
const DRAGONFLY_HOOKS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/hooks");

const GRAFANA_DASHBOARDS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/grafana_dashboards.md");
const DOTENV_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.env");

fn push_and_fix_settings_expanded() -> String {
    let body = PUSH_AND_FIX_SETTINGS_TEMPLATE.replace("__DRAGONFLY_HOOKS__", DRAGONFLY_HOOKS_DIR);
    let f = tempfile::Builder::new()
        .prefix("push-and-check-settings-")
        .suffix(".json")
        .tempfile_in("/tmp")
        .expect("failed to create settings tempfile");
    let (mut file, path) = f.keep().expect("failed to persist settings tempfile");
    file.write_all(body.as_bytes())
        .expect("failed to write settings tempfile");
    path.to_string_lossy().into_owned()
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

const REVIEW_AGGRESSIVE: &str = "\
This PR has high potential for bugs. Be thorough:
Trace through ALL code paths touched by this PR. Follow the call chains — don't just read the diff in isolation.
Use multiple sub-agents in parallel to review different areas simultaneously.
Look for subtle issues: race conditions, missing error handling, incorrect assumptions about state, edge cases in new logic.
Leave no stone unturned — the goal is to be confident nothing was missed.

Use the potential_for_bugs field in the area breakdown as a guide for what to focus on in particular.
Include the paths to the precalculated diff files for files that are relevant for each subagent.

One of the subagents should be dedicated to finding code that can be simplified. Guide it using the potential_for_simplification score in the PR areas breakdown — focus it on the areas with the highest simplification potential.
";

const REVIEW_SIMPLIFICATION: &str = "\
This PR has areas with high simplification potential.
Spin up a dedicated sub-agent to review the code for simplification opportunities — duplicate code, large functions that should be broken down, or repetitive patterns that could be restructured.
Guide it using the potential_for_simplification score in the PR areas breakdown — focus it on the areas with the highest simplification potential.
Include the paths to the precalculated diff files for files that are relevant for that subagent.
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
    pr_areas_str: &str,
    pr_areas: &Option<serde_json::Value>,
    graphite_str: &str,
    agent_sessions_str: &str,
) -> String {
    let notes = if let Some(reason) = skip_ci {
        format!(" CI was skipped due to {reason} — investigate those first.")
    } else if files.iter().any(|f| f.path.to_string_lossy().contains("failures")) {
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
    let skill_text = skill::PUSH_AND_FIX_SKILL
        .replace("CUSTOM_REVIEW_PLACEHOLDER", review_instructions)
        .replace("GRAFANA_DASHBOARDS_PATH", GRAFANA_DASHBOARDS_PATH);

    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z (%Z)").to_string();

    format!(
        "{skill_text}{graphite_str}\n\n\
         # Instructions\n\n\
         Current time: {now}\n\
         PR status: {pr_status}\n\
         {}{}{}\n\
         Per-file diffs:\n\
         {diff_files_str}{pr_areas_str}\n\
         Phase 1 (push):\n\
         Already done.\n\n\
         Phase 2/3:\n\
         {notes}\n\n\
         Pre-collected data:\n\
         {files_index}\n\
         {agent_sessions_str}\
         {prior_reviews_str}{review_instruction}\n\
         Read only the files you need. Start with the smallest/most relevant ones.\n\n\
         Continue with the next relevant phase, and read the instructions carefully.\n",
        ctx.main_commits, ctx.pr_commits, ctx.changed_files,
    )
}

fn filter_relevant_files(paths: &[String]) -> Vec<&str> {
    paths.iter().filter(|p| !p.ends_with("_gen.go") && !p.ends_with("_pb.ts")).map(|s| s.as_str()).collect()
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

    let full_diff_str = full_diff.iter().map(|(name, diff)| format!("<diff name=\"{name}\">\n{diff}\n</diff>")).collect::<Vec<_>>().join("\n");

    println!("Analyzing PR areas...");
    let pr_areas = analyze_pr_areas(&full_diff_str, &ctx.changed_files, &ctx.pr_commits).await;
    match pr_areas {
        Some(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        None => println!("No areas found."),
    }
    println!("\nCompleted in {:.1}s", start.elapsed().as_secs_f64());
}

struct ClaudeInvocation {
    prompt: String,
    settings: String,
    path: String,
}

async fn build_claude_invocation(force: bool) -> ClaudeInvocation {
    let (push_result, merge_probe) = push(force).await;
    if push_result.code != 0 {
        println!("⚠️  Push had issues: {}", push_result.stderr);
    }

    let mut graphite_handle = Some(tokio::spawn(build_graphite_info()));

    let base_ref = pr_base_ref().await;
    if base_ref != "origin/main" {
        println!("   Diffing against `{base_ref}` (stack parent).");
    }
    let branch_commits = sh(&format!("git log {base_ref}..HEAD --oneline")).await;

    // Run area analysis in parallel with PR/CI checks
    let branch_commits_clone = branch_commits.clone();
    let base_ref_clone = base_ref.clone();
    let mut areas_handle = Some(tokio::spawn(async move {
        let changed_files = get_changed_files(&base_ref_clone).await;
        let relevant_changed_files = filter_relevant_files(&changed_files);
        let diff_files_str = write_diff_files(&relevant_changed_files, &base_ref_clone).await;
        let ctx = collect_context_strings(&branch_commits_clone, &base_ref_clone).await;

        let pr_areas = analyze_pr_areas(&diff_files_str, &ctx.changed_files, &ctx.pr_commits).await;
        (pr_areas, diff_files_str, ctx)
    }));

    // Launch independent checks in parallel. The merge-tree probe was
    // already run inside push() (it drives the rebase decision); reuse it.
    let bg_status = sh_bg("git status -b --porcelain=v2");
    let bg_pr = sh_bg("gh pr view --json number,url,isDraft 2>/dev/null");

    let git_status = sh_wait(bg_status).await.unwrap_or_default();
    let push_content = build_push_content(&push_result, &git_status);
    let mut files = vec![section("push", &push_content)];

    let merge = build_merge_content(merge_probe).await;
    files.push(section("merge", &merge.content));

    let mut pre_areas: Option<(Option<serde_json::Value>, String, ContextStrings)> = None;
    let mut pre_graphite: Option<Option<GraphiteInfo>> = None;
    let pr_info = if let Some(pr) = lookup_existing_pr(bg_pr).await {
        pr
    } else {
        println!("   No PR found — creating draft PR...");
        // Prompt for the title first so the user isn't blocked by the
        // subagent. `prompt_pr_title` masks SIGCHLD on the prompt thread so
        // a background child exiting won't interrupt dialoguer's read(2).
        let title = prompt_pr_title(&branch_commits);
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
            None => PrInfo { number: None, url: None, is_draft: false },
        }
    };

    let mut skip_ci = None;
    let mut failed_names = Vec::new();
    if let Some(ref pr_num) = pr_info.number {
        if let Some(ref pr_url) = pr_info.url {
            let ci = collect_reviews_and_ci(pr_num, pr_url, &push_result.branch, merge.has_conflicts, &base_ref).await;
            files.extend(ci.files);
            skip_ci = ci.skip_ci;
            failed_names = ci.failed_names;
        }
    }

    let files_index = build_files_index(&files, merge.has_conflicts, &failed_names);

    let (prior_reviews, review_instruction) = get_review_log_context(&pr_info.number);
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

    let prompt = build_prompt(
        pr_status,
        &files_index,
        &skip_ci,
        &files,
        &ctx,
        &diff_files_str,
        &prior_reviews,
        &review_instruction,
        &pr_areas_str,
        &pr_areas,
        &graphite_str,
        &agent_sessions_str,
    );

    // Put our own binary on PATH so the agent can call push-and-check subcommands
    let path = {
        let current = std::env::var("PATH").unwrap_or_default();
        match std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            Some(dir) => format!("{}:{current}", dir.display()),
            None => current,
        }
    };

    let settings = push_and_fix_settings_expanded();
    ClaudeInvocation { prompt, settings, path }
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
            CliCommand::Pr { command: PrCommand::Thread { command } } => match command {
                ThreadCommand::Comment { thread_id, body } => {
                    pr_thread_comment(&thread_id, &body).await;
                }
                ThreadCommand::Resolve { thread_id } => {
                    pr_thread_resolve(&thread_id).await;
                }
            },
            CliCommand::Pr { command: PrCommand::Description { body } } => {
                pr_set_description(&body).await;
            }
            CliCommand::Pr { command: PrCommand::Comments { pr } } => {
                pr_comments(pr).await;
            }
            CliCommand::Ci { command } => {
                let code = match command {
                    CiCommand::Status { all, pr } => ci_status_cmd(pr, all).await,
                    CiCommand::Failures { pr, max_bytes } => ci_failures_cmd(pr, max_bytes).await,
                    CiCommand::Watch { pr } => ci_watch_cmd(pr).await,
                    CiCommand::Flaky { name, limit } => ci_flaky_cmd(name, limit).await,
                    CiCommand::Retries { pr } => ci_retries_cmd(pr).await,
                    CiCommand::Rerun { name, pr } => ci_rerun_cmd(name, pr).await,
                };
                std::process::exit(code);
            }
            CliCommand::Prompt => {
                let invocation = build_claude_invocation(cli.force).await;
                println!("{}", invocation.prompt);
            }
        }
        return;
    }

    if cli.areas {
        run_areas_only().await;
        return;
    }

    let invocation = build_claude_invocation(cli.force).await;
    println!("   Launching Claude Code...\n");
    let mut cmd = std::process::Command::new("claude");
    cmd.args(["--dangerously-skip-permissions", "--settings", &invocation.settings])
        .arg(&invocation.prompt)
        .env("PATH", &invocation.path);
    for (k, v) in load_dotenv() {
        cmd.env(k, v);
    }
    let err = cmd.exec();
    eprintln!("Failed to exec claude: {err}");
    std::process::exit(1);
}
