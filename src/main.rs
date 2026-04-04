use chrono::DateTime;
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

mod skill;

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

// ── Push ─────────────────────────────────────────────────────────────────────

async fn push(force: bool) -> PushResult {
    println!("   Fetching remote...");
    let bg_fetch = sh_bg("git fetch");

    let branch = sh("git branch --show-current").await.unwrap_or_default();
    if branch.is_empty() {
        eprintln!("❌ Not on a branch. Aborting.");
        std::process::exit(1);
    }
    println!("   Branch: {branch}");

    sh_wait(bg_fetch).await;

    let upstream = sh("git rev-parse --abbrev-ref @{upstream} 2>/dev/null").await;
    if upstream.is_none() {
        println!("   No upstream — pushing with -u...");
        let r = sh3("git push -u origin HEAD").await;
        return PushResult {
            branch,
            strategy: "new",
            code: r.code,
            stdout: r.stdout,
            stderr: r.stderr,
        };
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
        return PushResult {
            branch,
            strategy: "up-to-date",
            code: 0,
            stdout: "Already up to date".into(),
            stderr: String::new(),
        };
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
    PushResult {
        branch,
        strategy: if needs_force { "force-with-lease" } else { "fast-forward" },
        code: r.code,
        stdout: r.stdout,
        stderr: r.stderr,
    }
}

// ── Reviews ──────────────────────────────────────────────────────────────────

const REVIEW_THREADS_QUERY: &str = r#"query($owner: String!, $repo: String!, $pr: Int!) {
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
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "isOutdated")]
    is_outdated: bool,
}

async fn collect_reviews(owner: &str, repo: &str, pr_number: &str) -> (Vec<TempFile>, bool) {
    let query_escaped = REVIEW_THREADS_QUERY.replace('\'', "'\\''");
    let bg_threads = sh_bg(&format!(
        "gh api graphql -f query='{query_escaped}' -f owner={owner} -f repo={repo} -F pr={pr_number}"
    ));
    let bg_bot = sh_bg(&format!(
        "gh api repos/{owner}/{repo}/pulls/{pr_number}/comments \
         --jq '.[] | select(.user.type == \"Bot\" or (.user.login | test(\"bot|amp|review\"; \"i\"))) \
         | {{user: .user.login, path: .path, line: .line, body: .body}}'"
    ));
    let bg_reviews = sh_bg(&format!("gh pr view {pr_number} --json reviews"));

    let mut files = Vec::new();
    let mut has_unresolved = false;

    let threads = sh3_wait(bg_threads).await;
    if !threads.stdout.is_empty() {
        files.push(section_json("review-threads", &threads.stdout));
        if let Some(resp) = parse_json::<GqlThreadsResponse>(&threads.stdout) {
            let nodes = resp
                .data
                .and_then(|d| d.repository)
                .and_then(|r| r.pull_request)
                .and_then(|p| p.review_threads)
                .map(|t| t.nodes)
                .unwrap_or_default();
            has_unresolved = nodes.iter().any(|t| !t.is_resolved && !t.is_outdated);
        }
    }

    let bot = sh3_wait(bg_bot).await;
    if !bot.stdout.is_empty() {
        files.push(section_json("review-bot", &bot.stdout));
    }

    let reviews = sh3_wait(bg_reviews).await;
    if !reviews.stdout.is_empty() {
        files.push(section_json("review-pr", &reviews.stdout));
    }

    (files, has_unresolved)
}

// ── CI ───────────────────────────────────────────────────────────────────────

fn run_id_from_url(url: &str) -> u64 {
    let re = Regex::new(r"/(\d+)").unwrap();
    re.find_iter(url)
        .last()
        .and_then(|m| m.as_str().trim_start_matches('/').parse().ok())
        .unwrap_or(0)
}

fn parse_checks(out: &str) -> CheckCounts {
    let mut checks: HashMap<&str, (u64, &str)> = HashMap::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            let name = parts[0].trim();
            let status = parts[1].trim();
            let run_id = if parts.len() >= 4 { run_id_from_url(parts[3]) } else { 0 };
            if checks.get(name).is_none_or(|prev| run_id > prev.0) {
                checks.insert(name, (run_id, status));
            }
        }
    }

    let mut counts = CheckCounts { passed: 0, failed: 0, pending: 0, skipping: 0 };
    for &(_, status) in checks.values() {
        match status {
            "pass" => counts.passed += 1,
            "fail" => counts.failed += 1,
            "skipping" => counts.skipping += 1,
            _ => counts.pending += 1,
        }
    }
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

async fn get_changed_files() -> Vec<String> {
    sh("git diff --name-only origin/main...HEAD")
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
) -> CiWaitResult {
    println!("   Waiting for CI checks...");
    let head_sha = sh("git rev-parse HEAD").await.unwrap_or_default();

    let (mut counts, mut _check_rc, mut out) = if let Some((c, rc, o)) = first_check {
        (c, rc, o)
    } else {
        let r = sh3(&format!("gh pr checks {pr_number}")).await;
        let mut c = parse_checks(&r.stdout);
        if r.code != 0 && c.failed == 0 {
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
        let changed = get_changed_files().await;
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
            if _check_rc != 0 && counts.failed == 0 {
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
    ci_content += &format!("```\n{out}\n```\n");

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
    let logs = collect_failure_logs(branch, &head_sha).await;
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
    #[serde(rename = "headSha")]
    head_sha: Option<String>,
}

async fn collect_failure_logs(branch: &str, head_sha: &str) -> FailureLogs {
    println!("   Collecting failure logs...");
    let r = sh3(&format!(
        "gh run list --branch {branch} --status failure --limit 10 \
         --json databaseId,name,headSha"
    ))
    .await;

    let runs: Vec<RunInfo> = parse_json(&r.stdout).unwrap_or_default();
    let mut summaries = Vec::new();
    let mut full_logs = Vec::new();
    let mut names = Vec::new();

    for run in runs.iter().filter(|r| r.head_sha.as_deref() == Some(head_sha)) {
        let name = run.name.as_deref().unwrap_or("unknown");
        let run_id = run.database_id;
        names.push(name.to_string());
        println!("      Fetching logs for run {run_id} ({name})...");
        let log = sh3(&format!("gh run view {run_id} --log-failed")).await;
        let log_text = if !log.stdout.is_empty() { &log.stdout } else { &log.stderr };
        let summary = extract_failure_summary(log_text);
        summaries.push(format!("### {name}\n```\n{summary}\n```"));
        full_logs.push(format!("## Run {run_id} — {name}\n```\n{log_text}\n```"));
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

async fn find_or_create_pr(bg_pr: Child, branch_commits: &Option<String>) -> PrInfo {
    let pr_data: Option<PrData> = sh_wait(bg_pr).await.and_then(|s| parse_json(&s));

    if let Some(pr) = pr_data {
        println!("🔗 {}", pr.url);
        return PrInfo {
            number: Some(pr.number.to_string()),
            url: Some(pr.url),
            is_draft: pr.is_draft,
        };
    }

    println!("   No PR found — creating draft PR...");
    if let Some(commits) = branch_commits {
        println!("   Commits on this branch:");
        for line in commits.lines() {
            let title = line.split_once(' ').map(|(_, t)| t).unwrap_or(line);
            println!("      • {title}");
        }
    }

    // Interactive — needs inherited stdio
    let rc = std::process::Command::new("sh")
        .args(["-c", "gh pr create --draft"])
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
        if first.code != 0 && counts.failed == 0 {
            counts.pending = counts.pending.max(1);
        }

        if has_unresolved && counts.failed == 0 {
            skip_ci = Some("unresolved review comments".into());
            println!("   Skipping CI wait — unresolved review comments to investigate first.");
            let ci_content = format!(
                "# CI Checks\n\nPR: #{pr_number}\nExit code: {}\n\
                 Note: CI wait skipped due to unresolved review comments.\n```\n{}\n```\n",
                first.code, first.stdout
            );
            files.push(section("ci", &ci_content));
        } else {
            let ci = wait_for_ci(pr_number, branch, Some((counts, first.code, first.stdout))).await;
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

async fn collect_context_strings(branch_commits: &Option<String>) -> ContextStrings {
    let (diff, main) = tokio::join!(
        sh("git diff --stat origin/main...HEAD"),
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
        ("review-threads", "review threads (GraphQL)".into()),
        ("review-bot", "bot comments".into()),
        ("review-pr", "PR reviews".into()),
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

async fn write_diff_files(changed_files: &[String]) -> String {
    let mut result = String::new();
    for fname in changed_files {
        if let Some(diff) = sh(&format!("git diff origin/main...HEAD -- {fname}")).await {
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

// ── Review log context ───────────────────────────────────────────────────────

fn get_review_log_context(pr_number: &Option<String>) -> (String, String) {
    let Some(pr) = pr_number else {
        return (String::new(), String::new());
    };
    let log_dir = home_dir().join(format!(".claude/pr-logs/{pr}"));
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
        "\nRight before 'Phase 7: Final Status', after fixing any issues in 'Phase 6: Custom review' \
         save a brief summary to `{}/review-{next_n}.md`. \
         Include which issues the user did not want to fix.\n",
        log_dir.display()
    );
    (prior, instruction)
}

// ── PR area analysis ─────────────────────────────────────────────────────────

fn analyze_pr_areas(
    diff_files_str: &str,
    changed_files_str: &str,
    pr_commits_str: &str,
) -> Option<serde_json::Value> {
    let prompt = format!(
        r#"
git log origin/main..HEAD --oneline:
{pr_commits_str}

git diff --stat origin/main...HEAD:
{changed_files_str}

Per-file diffs:
{diff_files_str}

# Instructions

Explore the changes done in this PR and make a list of the high-level areas that are covered.
Read the per-file diff files to understand the changes.
If the PR is small, and there's only one area covered, then output only one area.
For each area, list a name, a description (a few sentances), and list the files or directories that are the most relevant.
Format the output as json. Include only json and nothing else.

Also add a potential_for_bugs estimate according to the following scale examples:

1. No code changed.
2. Minor changes that preserve existing semantics perfectly.
3. Changes to cli tools that aren't actually used in production.
4. Changes to non-critical services.
6. Non-trivial, but well encapsulated, changes to core services.
8. Non-trivial changes to core services affecting many parts of the codebase, making it very likely some part is missed.
10. Large scale changes or many subtle edge cases.

Focus on the potential for bugs causing issues in production.

## Example

```
{{
    "areas": [
        {{
            "name": "Frontend SSE streaming",
            "description": "Refactored the streaming logic of agent and user messages from a long-polling http endpoint to use websockets...",
            "files": ["app/src/lib/trajectory", "app/proto/generated_types.ts"],
            "potential_for_bugs": 8,
        }},
        {{
            "name": "Fixed off-by-one error in backend trajectory endpoint",
            "description": "The Limit parameter had an off-by-one error which resulted in too many results being returned...",
            "files": ["go/api/endpoints.go"],
            "potential_for_bugs": 3,
        }}
    ]
}}
```
"#
    );

    let settings = push_and_fix_settings_expanded();
    let result = std::process::Command::new("claude")
        .args([
            "--print",
            "--dangerously-skip-permissions",
            "--model",
            "haiku",
            "--settings",
            &settings,
            "--system-prompt",
            &prompt,
            "Analyze the PR",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let output = String::from_utf8_lossy(&result.stdout).trim().to_string();
    extract_json_from_end(&output)
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

// ── Prompt building ──────────────────────────────────────────────────────────

const PUSH_AND_FIX_SETTINGS: &str = "~/cloud/Programming/claude-code/push-and-check-settings.json";

fn push_and_fix_settings_expanded() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    PUSH_AND_FIX_SETTINGS.replacen('~', &home, 1)
}

const REVIEW_AGGRESSIVE: &str = "\
This PR has high potential for bugs. Be thorough:
Trace through ALL code paths touched by this PR. Follow the call chains — don't just read the diff in isolation.
Use multiple sub-agents in parallel to review different areas simultaneously.
Look for subtle issues: race conditions, missing error handling, incorrect assumptions about state, edge cases in new logic.
Leave no stone unturned — the goal is to be confident nothing was missed.

Use the potential_for_bugs field in the area breakdown as a guide for what to focus on in particular.
Include the paths to the precalculated diff files for files that are relevant for each subagent.
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
) -> String {
    let notes = if let Some(reason) = skip_ci {
        format!(" CI was skipped due to {reason} — investigate those first.")
    } else if files.iter().any(|f| f.path.to_string_lossy().contains("failures")) {
        " CI stopped at first failure; other checks may still be running.".into()
    } else {
        String::new()
    };

    let max_bug_potential = pr_areas
        .as_ref()
        .and_then(|v| v.get("areas"))
        .and_then(|a| a.as_array())
        .map(|areas| {
            areas
                .iter()
                .filter_map(|a| a.get("potential_for_bugs").and_then(|v| v.as_u64()))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    let review_instructions = if max_bug_potential >= 6 { REVIEW_AGGRESSIVE } else { "" };
    let skill_text = skill::PUSH_AND_FIX_SKILL.replace("CUSTOM_REVIEW_PLACEHOLDER", review_instructions);

    format!(
        "{skill_text}\n\n\
         # Instructions\n\n\
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
         {prior_reviews_str}{review_instruction}\n\
         Read only the files you need. Start with the smallest/most relevant ones.\n\n\
         Continue with the next relevant phase, and read the instructions carefully.\n",
        ctx.main_commits, ctx.pr_commits, ctx.changed_files,
    )
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let force = std::env::args().any(|a| a == "--force");

    let push_result = push(force).await;
    if push_result.code != 0 {
        println!("⚠️  Push had issues: {}", push_result.stderr);
    }

    // Launch independent checks in parallel
    let bg_status = sh_bg("git status -b --porcelain=v2");
    let bg_merge = sh_bg("git merge-tree --write-tree --name-only origin/main HEAD");
    let bg_pr = sh_bg("gh pr view --json number,url,isDraft 2>/dev/null");

    let git_status = sh_wait(bg_status).await.unwrap_or_default();
    let push_content = build_push_content(&push_result, &git_status);
    let mut files = vec![section("push", &push_content)];

    let merge = build_merge_content(sh3_wait(bg_merge).await).await;
    files.push(section("merge", &merge.content));

    let branch_commits = sh("git log origin/main..HEAD --oneline").await;
    let pr_info = find_or_create_pr(bg_pr, &branch_commits).await;

    let mut skip_ci = None;
    let mut failed_names = Vec::new();
    if let Some(ref pr_num) = pr_info.number {
        if let Some(ref pr_url) = pr_info.url {
            let ci = collect_reviews_and_ci(pr_num, pr_url, &push_result.branch, merge.has_conflicts).await;
            files.extend(ci.files);
            skip_ci = ci.skip_ci;
            failed_names = ci.failed_names;
        }
    }

    let files_index = build_files_index(&files, merge.has_conflicts, &failed_names);
    println!("\n   Result files:\n{files_index}");
    println!("   Launching Claude Code...\n");

    let changed_files = get_changed_files().await;
    let diff_files_str = write_diff_files(&changed_files).await;
    let ctx = collect_context_strings(&branch_commits).await;

    println!("   Analyzing PR areas...");
    let pr_areas = analyze_pr_areas(&diff_files_str, &ctx.changed_files, &ctx.pr_commits);
    let pr_areas_str = pr_areas
        .as_ref()
        .map(|v| {
            format!(
                "\nPR area analysis:\n```json\n{}\n```\n",
                serde_json::to_string_pretty(v).unwrap()
            )
        })
        .unwrap_or_default();

    let (prior_reviews, review_instruction) = get_review_log_context(&pr_info.number);
    let pr_status = if pr_info.is_draft {
        "draft"
    } else if pr_info.number.is_some() {
        "ready for review"
    } else {
        "none"
    };

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
    );

    let settings = push_and_fix_settings_expanded();
    let err = std::process::Command::new("claude")
        .args(["--dangerously-skip-permissions", "--settings", &settings])
        .arg(&prompt)
        .exec();
    eprintln!("Failed to exec claude: {err}");
    std::process::exit(1);
}
