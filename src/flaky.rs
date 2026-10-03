//! `dragonfly ci flaky`: how often a named check passes vs fails outside this PR.
//!
//! Sources, fetched concurrently:
//! - GitHub Actions check runs on the last N commits of main.
//! - Buildkite jobs on main builds, and on other branches' builds of pipelines
//!   that build PRs (PR-only steps such as `pull-request-checks` never run on main).
//!
//! A Buildkite job that failed and then passed on retry within one build is a
//! confirmed flake, independent of any pass/fail ratio.

use crate::{home_dir, sh};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// Requests in flight per source; bounds load on the GitHub and Buildkite APIs.
const CONCURRENCY: usize = 10;
/// A commit with no run of the check this long after it was committed never gets one.
const NO_RUN_FINAL_AFTER_SECS: i64 = 6 * 3600;
/// How far back `gha_report` pages looking for runs of a path-filtered check.
const MAX_COMMITS: usize = 200;
const PIPELINE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

#[derive(Default)]
struct Tally {
    pass: usize,
    fail: usize,
    skip: usize,
    other: usize,
}

impl Tally {
    fn add(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Pass => self.pass += 1,
            Outcome::Fail => self.fail += 1,
            Outcome::Skip => self.skip += 1,
            Outcome::Other => self.other += 1,
        }
    }

    fn summary(&self) -> String {
        format!(
            "  ✅ {} pass    ❌ {} fail    ⏭ {} skip/none    ? {} other",
            self.pass, self.fail, self.skip, self.other
        )
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Outcome {
    Pass,
    Fail,
    Skip,
    Other,
}

pub async fn run(name: String, limit: usize) -> i32 {
    let limit = limit.clamp(1, 100);
    let (gha, bk) = tokio::join!(gha_report(&name, limit), buildkite_report(&name, limit));
    let mut printed = false;
    for section in [gha, bk].into_iter().flatten() {
        if printed {
            println!();
        }
        print!("{section}");
        printed = true;
    }
    if !printed {
        println!(
            "No runs of `{name}` found on GitHub Actions (last {limit} main commits) \
             or Buildkite (recent builds). Check the name with `dragonfly ci status --all`."
        );
    }
    0
}

// ── GitHub Actions ───────────────────────────────────────────────────────────

struct CommitResult {
    sha: String,
    outcome: Outcome,
    label: String,
    url: String,
}

/// Collects the last `limit` main commits where the check ran, paging back up to
/// [MAX_COMMITS]. Path-filtered checks skip most commits, so a fixed commit window
/// holds only a handful of real data points.
///
/// Returns None when the check ran on none of the inspected commits.
async fn gha_report(name: &str, limit: usize) -> Option<String> {
    let mut ran: Vec<CommitResult> = Vec::new();
    let mut skipped = 0;
    let mut inspected = 0;
    'pages: for page in 1..=MAX_COMMITS / 100 {
        let commits = main_commits(page).await?;
        if commits.is_empty() {
            break;
        }
        for chunk in commits.chunks(CONCURRENCY * 2) {
            let results: Vec<CommitResult> = stream::iter(chunk.iter().cloned())
                .map(|(sha, ts)| gha_commit_result(name, sha, ts))
                .buffered(CONCURRENCY)
                .collect()
                .await;
            for r in results {
                inspected += 1;
                if r.outcome == Outcome::Skip {
                    skipped += 1;
                    continue;
                }
                ran.push(r);
                if ran.len() == limit {
                    break 'pages;
                }
            }
        }
    }
    if ran.is_empty() {
        return None;
    }

    let mut tally = Tally::default();
    let mut rows = String::new();
    for r in &ran {
        tally.add(r.outcome);
        rows.push_str(&format!("{} {}", &r.sha[..7], r.label));
        if !r.url.is_empty() {
            rows.push_str(&format!("  {}", r.url));
        }
        rows.push('\n');
    }
    let mut out = format!(
        "GitHub Actions `{name}` on the last {} main commits where it ran \
         ({inspected} inspected, {skipped} skipped or not run, bot / [skip ci] commits excluded):\n",
        ran.len()
    );
    out.push_str(&tally.summary());
    out.push_str("\n\n");
    out.push_str(&rows);
    if ran.len() < limit {
        out.push_str(&format!(
            "\nOnly {} runs in the last {MAX_COMMITS} main commits.\n",
            ran.len()
        ));
    }
    out.push_str(&format!("\nVerdict: {}\n", main_verdict(&tally, 0)));
    Some(out)
}

/// One page of main's history as (sha, commit time), bot commits removed.
async fn main_commits(page: usize) -> Option<Vec<(String, i64)>> {
    let raw = sh(&format!(
        "gh api 'repos/{{owner}}/{{repo}}/commits?sha=main&per_page=100&page={page}' \
         --jq '.[] | \"\\(.sha) \\(.commit.committer.date) \\(.author.type // \"\") \\(.commit.message | split(\"\\n\")[0])\"'"
    ))
    .await?;
    Some(
        raw.lines()
            .filter_map(|l| {
                let mut parts = l.splitn(4, ' ');
                let (sha, date, author_type) = (parts.next()?, parts.next()?, parts.next()?);
                let subject = parts.next().unwrap_or("");
                if skips_ci(author_type, subject) {
                    return None;
                }
                let ts = chrono::DateTime::parse_from_rfc3339(date).ok()?.timestamp();
                Some((sha.to_string(), ts))
            })
            .collect(),
    )
}

/// Bot commits on main (`chore(build-go): record built images … [skip ci]`,
/// `chore(k8s): deploy …`) run no CI, so counting them reads as "check skipped".
fn skips_ci(author_type: &str, subject: &str) -> bool {
    author_type == "Bot" || subject.contains("[skip ci]") || subject.contains("[ci skip]")
}

async fn gha_commit_result(name: &str, sha: String, committed_at: i64) -> CommitResult {
    let cache = home_dir()
        .join(".dragonfly/cache/flaky/gha")
        .join(sanitize(name))
        .join(&sha);
    let line = match std::fs::read_to_string(&cache) {
        Ok(cached) => cached,
        Err(_) => {
            let fetched = sh(&format!(
                "gh api 'repos/{{owner}}/{{repo}}/commits/{sha}/check-runs?check_name={}&filter=latest' \
                 --jq '.check_runs[0] // empty | \"\\(.conclusion // .status) \\(.status) \\(.html_url)\"'",
                percent_encode(name)
            ))
            .await
            .unwrap_or_default();
            // Contract: only immutable answers are cached. A finished run never changes;
            // a missing run is final once CI has had time to start.
            let age = chrono::Utc::now().timestamp() - committed_at;
            let is_final = if fetched.is_empty() {
                age > NO_RUN_FINAL_AFTER_SECS
            } else {
                fetched.split(' ').nth(1) == Some("completed")
            };
            if is_final {
                if let Some(dir) = cache.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&cache, &fetched);
            }
            fetched
        }
    };
    let mut parts = line.splitn(3, ' ');
    let conclusion = parts.next().unwrap_or("").to_string();
    let url = parts.nth(1).unwrap_or("").trim().to_string();
    let outcome = match conclusion.as_str() {
        "success" => Outcome::Pass,
        "failure" | "timed_out" => Outcome::Fail,
        // A newer push cancels main's in-flight run; that says nothing about the test.
        "skipped" | "neutral" | "cancelled" | "" => Outcome::Skip,
        _ => Outcome::Other,
    };
    let label = if conclusion.is_empty() {
        "no-run".to_string()
    } else {
        conclusion
    };
    CommitResult {
        sha,
        outcome,
        label,
        url,
    }
}

// ── Buildkite ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BkPipeline {
    slug: String,
    provider: Option<BkProvider>,
}

#[derive(Deserialize)]
struct BkProvider {
    settings: Option<BkProviderSettings>,
}

#[derive(Deserialize)]
struct BkProviderSettings {
    #[serde(default)]
    build_pull_requests: bool,
}

#[derive(Deserialize)]
struct BkBuild {
    number: u64,
    branch: String,
    commit: String,
    web_url: String,
    pipeline: BkBuildPipeline,
    #[serde(default)]
    jobs: Vec<BkJob>,
}

#[derive(Deserialize)]
struct BkBuildPipeline {
    slug: String,
}

#[derive(Deserialize, Clone)]
struct BkJob {
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    state: Option<String>,
    step_key: Option<String>,
    created_at: Option<String>,
    #[serde(default)]
    retried: bool,
    #[serde(default)]
    soft_failed: bool,
}

/// The matched step's result in one build.
struct StepResult {
    pipeline: String,
    number: u64,
    branch: String,
    commit: String,
    web_url: String,
    outcome: Outcome,
    state: String,
    /// An earlier attempt failed and a retry in the same build passed.
    flipped: bool,
}

async fn bk_get(path: &str) -> Option<String> {
    sh(&format!("bk api '{path}' 2>/dev/null")).await
}

/// Returns None when `bk` is unavailable or no build ran a matching step.
async fn buildkite_report(name: &str, limit: usize) -> Option<String> {
    let query = normalize_step(name.strip_prefix("buildkite/").unwrap_or(name));
    let current_branch = sh("git rev-parse --abbrev-ref HEAD")
        .await
        .unwrap_or_default();

    // Only finished builds: running ones carry no outcome and would crowd the rows.
    let main_path = format!(
        "/builds?branch=main&per_page={}&include_retried_jobs=true&state[]=passed&state[]=failed",
        (limit * 3).min(100)
    );
    let (pr_slugs, main_raw) = tokio::join!(pr_pipeline_slugs(), bk_get(&main_path));
    let pr_slugs = pr_slugs?;
    let pr_builds: Vec<BkBuild> = stream::iter(pr_slugs)
        .map(|slug| async move {
            let raw = bk_get(&format!(
                "/pipelines/{slug}/builds?per_page={limit}&include_retried_jobs=true&state[]=passed&state[]=failed"
            ))
            .await?;
            serde_json::from_str::<Vec<BkBuild>>(&raw).ok()
        })
        .buffered(CONCURRENCY)
        .filter_map(|b| async move { b })
        .flat_map(stream::iter)
        .collect()
        .await;

    let main_builds: Vec<BkBuild> = main_raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let main_results: Vec<StepResult> = main_builds
        .iter()
        .filter_map(|b| step_result(b, &query))
        .collect();
    let pr_results: Vec<StepResult> = pr_builds
        .iter()
        .filter(|b| b.branch != "main" && b.branch != current_branch)
        .filter_map(|b| step_result(b, &query))
        .collect();
    if main_results.is_empty() && pr_results.is_empty() {
        return None;
    }

    let mut out = String::new();
    if !main_results.is_empty() {
        let (tally, flips, rows) = summarize(&main_results);
        out.push_str(&format!(
            "Buildkite `{name}` on {} recent main builds:\n{}\n\n{rows}\nVerdict: {}\n",
            main_results.len(),
            tally.summary(),
            main_verdict(&tally, flips)
        ));
    }
    if !pr_results.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        let (tally, flips, rows) = summarize(&pr_results);
        let failing_branches: BTreeSet<&str> = pr_results
            .iter()
            .filter(|r| r.outcome == Outcome::Fail)
            .map(|r| r.branch.as_str())
            .collect();
        let branches: BTreeSet<&str> = pr_results.iter().map(|r| r.branch.as_str()).collect();
        out.push_str(&format!(
            "Buildkite `{name}` on {} recent builds of other branches ({} branches):\n{}\n\n{rows}\nVerdict: {}\n",
            pr_results.len(),
            branches.len(),
            tally.summary(),
            pr_verdict(&tally, flips, failing_branches.len())
        ));
    }
    Some(out)
}

/// Slugs of pipelines that build pull requests, cached for a day.
///
/// The listing costs ~2 s and every per-pipeline fetch waits on it, so caching it
/// halves the Buildkite critical path.
async fn pr_pipeline_slugs() -> Option<Vec<String>> {
    let cache = home_dir().join(".dragonfly/cache/flaky/bk-pr-pipelines");
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < PIPELINE_CACHE_TTL);
    if fresh && let Ok(cached) = std::fs::read_to_string(&cache) {
        return Some(cached.lines().map(str::to_string).collect());
    }
    let raw = bk_get("/pipelines?per_page=100").await?;
    let pipelines: Vec<BkPipeline> = serde_json::from_str(&raw).ok()?;
    let slugs: Vec<String> = pipelines
        .into_iter()
        .filter(|p| {
            p.provider
                .as_ref()
                .and_then(|pr| pr.settings.as_ref())
                .is_some_and(|s| s.build_pull_requests)
        })
        .map(|p| p.slug)
        .collect();
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&cache, slugs.join("\n"));
    Some(slugs)
}

/// Finds the jobs in `build` whose step matches `query` and folds their attempts.
fn step_result(build: &BkBuild, query: &str) -> Option<StepResult> {
    let mut steps: BTreeMap<String, Vec<&BkJob>> = BTreeMap::new();
    for job in &build.jobs {
        let Some(name) = job.name.as_deref() else {
            continue;
        };
        if job.kind != "script" || !normalize_step(name).contains(query) {
            continue;
        }
        let key = job.step_key.clone().unwrap_or_else(|| name.to_string());
        steps.entry(key).or_default().push(job);
    }
    if steps.is_empty() {
        return None;
    }

    // A build can match several steps (e.g. a query of `test`): the worst one wins.
    let mut best: Option<(Outcome, String, bool)> = None;
    for attempts in steps.values_mut() {
        attempts.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let last = attempts
            .iter()
            .rev()
            .find(|j| !j.retried)
            .unwrap_or(attempts.last().unwrap());
        let state = last.state.clone().unwrap_or_default();
        let outcome = job_outcome(last);
        let flipped = outcome == Outcome::Pass
            && attempts
                .iter()
                .any(|j| j.retried && job_outcome(j) == Outcome::Fail);
        let rank = |o: Outcome| match o {
            Outcome::Fail => 3,
            Outcome::Other => 2,
            Outcome::Pass => 1,
            Outcome::Skip => 0,
        };
        let replace = match &best {
            None => true,
            Some((o, _, f)) => {
                rank(outcome) > rank(*o) || (rank(outcome) == rank(*o) && flipped && !f)
            }
        };
        if replace {
            best = Some((outcome, state, flipped));
        }
    }
    let (outcome, state, flipped) = best?;
    Some(StepResult {
        pipeline: build.pipeline.slug.clone(),
        number: build.number,
        branch: build.branch.clone(),
        commit: build.commit.chars().take(7).collect(),
        web_url: build.web_url.clone(),
        outcome,
        state,
        flipped,
    })
}

fn job_outcome(job: &BkJob) -> Outcome {
    // Gated steps report through their name, e.g. "bazel build: not required".
    let name = job.name.as_deref().unwrap_or("").to_lowercase();
    if name.contains("not required") || name.contains("nothing to") {
        return Outcome::Skip;
    }
    match job.state.as_deref().unwrap_or("") {
        "passed" => Outcome::Pass,
        "failed" | "timed_out" | "expired" if job.soft_failed => Outcome::Pass,
        "failed" | "timed_out" | "expired" | "canceled" => Outcome::Fail,
        "broken" | "skipped" | "not_run" => Outcome::Skip,
        _ => Outcome::Other,
    }
}

/// Rows show only non-passing builds and retry flips; passes are in the tally.
fn summarize(results: &[StepResult]) -> (Tally, usize, String) {
    let mut tally = Tally::default();
    let mut flips = 0;
    let mut rows = String::new();
    let mut shown = 0;
    for r in results {
        tally.add(r.outcome);
        if r.flipped {
            flips += 1;
        }
        if (r.outcome == Outcome::Pass && !r.flipped) || r.outcome == Outcome::Skip {
            continue;
        }
        shown += 1;
        if shown > 20 {
            continue;
        }
        let state = if r.flipped {
            "failed→passed on retry".to_string()
        } else {
            r.state.clone()
        };
        rows.push_str(&format!(
            "{} #{} {} {} {state}  {}\n",
            r.pipeline, r.number, r.branch, r.commit, r.web_url
        ));
    }
    if shown > 20 {
        rows.push_str(&format!("… {} more\n", shown - 20));
    }
    if rows.is_empty() {
        rows.push_str("(no failures)\n");
    }
    (tally, flips, rows)
}

fn main_verdict(t: &Tally, flips: usize) -> &'static str {
    if flips > 0 {
        "Failed then passed on retry within a build — confirmed flaky. Consider rerunning."
    } else if t.pass + t.fail == 0 {
        "No data — this check doesn't run on main. Compare against other branches instead."
    } else if t.fail == 0 {
        "Consistently passing on main — failure is likely caused by this PR."
    } else if t.pass == 0 {
        "Consistently failing on main — pre-existing issue, do not fix in this PR without confirmation."
    } else {
        "Mixed pass/fail on main — likely flaky. Consider rerunning."
    }
}

fn pr_verdict(t: &Tally, flips: usize, failing_branches: usize) -> &'static str {
    if flips > 0 {
        "Failed then passed on retry within a build — confirmed flaky. Consider rerunning."
    } else if t.fail == 0 {
        "Passing on every other branch — failure is likely caused by this PR."
    } else if failing_branches >= 3 {
        "Failing on several unrelated branches — likely flaky or broken on main. Check main and consider rerunning."
    } else {
        "Fails on only a few other branches — those are likely their own breakages; this failure is probably caused by this PR."
    }
}

/// Lowercases and drops `:emoji:` shortcodes and any `: suffix` status, so
/// ":bazel: bazel build: not required" and "bazel build" compare equal.
fn normalize_step(name: &str) -> String {
    let no_emoji: String = name
        .split_whitespace()
        .filter(|w| !(w.len() > 2 && w.starts_with(':') && w.ends_with(':')))
        .collect::<Vec<_>>()
        .join(" ");
    let base = no_emoji.split(": ").next().unwrap_or("");
    base.trim().to_lowercase()
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, state: &str, retried: bool, at: &str) -> BkJob {
        BkJob {
            kind: "script".into(),
            name: Some(name.into()),
            state: Some(state.into()),
            step_key: Some("k".into()),
            created_at: Some(at.into()),
            retried,
            soft_failed: false,
        }
    }

    fn build(jobs: Vec<BkJob>) -> BkBuild {
        BkBuild {
            number: 1,
            branch: "b".into(),
            commit: "abcdef0123".into(),
            web_url: String::new(),
            pipeline: BkBuildPipeline { slug: "p".into() },
            jobs,
        }
    }

    #[test]
    fn gated_step_name_matches_its_base_name() {
        assert_eq!(
            normalize_step(":bazel: bazel build: not required"),
            "bazel build"
        );
        assert_eq!(normalize_step("Bazel Build"), "bazel build");
    }

    #[test]
    fn retry_that_passes_is_a_flip_not_a_failure() {
        let b = build(vec![
            job(
                ":bazel: bazel build",
                "failed",
                true,
                "2026-10-01T00:00:00Z",
            ),
            job(
                ":bazel: bazel build",
                "passed",
                false,
                "2026-10-01T00:10:00Z",
            ),
        ]);
        let r = step_result(&b, "bazel build").unwrap();
        assert_eq!(r.outcome, Outcome::Pass);
        assert!(r.flipped);
    }

    #[test]
    fn gated_not_required_step_counts_as_skip() {
        let b = build(vec![job(
            ":bazel: bazel build: not required",
            "passed",
            false,
            "t",
        )]);
        assert_eq!(
            step_result(&b, "bazel build").unwrap().outcome,
            Outcome::Skip
        );
    }
}
