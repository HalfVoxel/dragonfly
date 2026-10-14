//! Failure reports for one commit's CI: which checks failed, the error lines from
//! each log, and whether main fails the same way.
//!
//! Pipeline:
//!   failing checks at the SHA -> fetch logs in parallel -> clean -> excerpt
//!   -> group checks with the same first error -> compare with main -> render
//!
//! The per-check facts (failed step, main health, log path) are deterministic and
//! always printed. An LLM only condenses the excerpts, and only when they are large,
//! so a bad summary cannot hide which check failed or where its log is.

use crate::flaky::{self, Health, MainRun, Outcome};
use crate::{
    Model, PrCheck, REPORT_IGNORED_CHECKS, call_kit_llm, gate_reason, home_dir, is_gate,
    is_ignored_check,
};
use futures::stream::{self, StreamExt};
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::process::Command;

/// Logs fetched per report; the rest of the failing checks are listed without one.
const MAX_LOGS: usize = 12;
const FETCH_CONCURRENCY: usize = 6;
/// Lines kept on each side of an error line.
const CONTEXT: usize = 2;
/// Error-context lines per check: the first [CONTEXT_HEAD] and the last of the rest.
const CONTEXT_CAP: usize = 60;
const CONTEXT_HEAD: usize = 20;
const TAIL: usize = 25;
const LINE_MAX: usize = 600;
/// Excerpts up to this many bytes are printed as is; larger ones are distilled.
const VERBATIM_MAX: usize = 4000;
/// Main runs compared per check.
const MAIN_RUNS: usize = 10;
/// Cheapest accurate model first; the fallbacks run on other providers (as in rgo).
const DISTILL_MODELS: &[&str] = &[
    "baseten/glm-5.3-flash",
    "vertex/gemini-3.8-flash",
    "anthropic/claude-haiku-4-5",
];
const DISTILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub struct Options {
    /// Print every excerpt instead of distilling large ones.
    pub raw: bool,
    /// Distill with only this model instead of [DISTILL_MODELS].
    pub model: Option<Model>,
    /// Compare each failure with the check's recent runs on main.
    pub main_history: bool,
}

pub struct Report {
    pub text: String,
    /// Fixable failing checks; empty when only gates, reruns or cancels are red.
    pub failed: Vec<String>,
    /// Every fact and excerpt, for when the printed text was distilled.
    pub file: Option<PathBuf>,
}

/// The checks of one commit a report covers, by what the reader can do about them.
#[derive(Default)]
pub struct Selection<'a> {
    pub failing: Vec<&'a PrCheck>,
    /// Red until a human approves; no code change turns them green.
    pub gates: Vec<&'a PrCheck>,
    /// Running again after a failed attempt.
    pub rerunning: Vec<&'a PrCheck>,
    pub cancelled: Vec<&'a PrCheck>,
}

/// Sorts a commit's checks into a [Selection], dropping advisory bots.
///
/// A name reported twice (a check run and a commit status) counts as failing when
/// either copy failed: showing a stale failure costs a line, hiding a real one
/// costs a CI round trip.
pub fn select(checks: &[PrCheck]) -> Selection<'_> {
    let mut by_name: BTreeMap<&str, &PrCheck> = BTreeMap::new();
    for c in checks {
        if is_ignored_check(&c.name, REPORT_IGNORED_CHECKS) {
            continue;
        }
        let keep = by_name
            .get(c.name.as_str())
            .is_none_or(|prev| prev.bucket != "fail" && c.bucket == "fail");
        if keep {
            by_name.insert(&c.name, c);
        }
    }
    let mut s = Selection::default();
    for c in by_name.into_values() {
        match c.bucket.as_str() {
            "fail" if is_gate(c) => s.gates.push(c),
            "fail" => s.failing.push(c),
            "pending" if c.rerun_of_failure => s.rerunning.push(c),
            "cancelled" => s.cancelled.push(c),
            _ => {}
        }
    }
    s
}

/// Builds the failure report for the checks of commit `sha`.
///
/// `label` names the target in the header ("PR #123" or "Commit"). Writes each
/// fetched log and the full report under /tmp/dragonfly-ci/.
pub async fn build(label: &str, sha: &str, checks: &[PrCheck], opts: &Options) -> Report {
    let sel = select(checks);
    let short = &sha[..7.min(sha.len())];
    let notes = render_notes(&sel);
    if sel.failing.is_empty() {
        let verdict =
            if sel.gates.is_empty() && sel.rerunning.is_empty() && sel.cancelled.is_empty() {
                "no failing checks"
            } else {
                "no fixable failures"
            };
        return Report {
            text: format!("{label} @ {short}: {verdict}.\n{notes}"),
            failed: Vec::new(),
            file: None,
        };
    }

    let dir = report_dir(label, short);
    let fetch_count = sel.failing.len().min(MAX_LOGS);
    // Owned inputs: borrowed ones make the future non-`Send`, and the main flow
    // runs this report inside `tokio::spawn`.
    let main_history = opts.main_history;
    let base_at = if main_history {
        base_time(sha).await
    } else {
        None
    };
    let to_fetch: Vec<PrCheck> = sel
        .failing
        .iter()
        .take(MAX_LOGS)
        .map(|c| (*c).clone())
        .collect();
    let mut items: Vec<Item> = stream::iter(to_fetch)
        .map(|c| {
            let (sha, dir) = (sha.to_string(), dir.clone());
            async move { analyze(&c, &sha, &dir, main_history, base_at).await }
        })
        .buffered(FETCH_CONCURRENCY)
        .collect()
        .await;
    for c in sel.failing.iter().skip(fetch_count) {
        items.push(Item::unfetched(c));
    }
    mark_duplicates(&mut items);

    let aggregators = items.iter().filter(|i| i.aggregator).count();
    let mut header = format!(
        "{label} @ {short}: {} failing check{}",
        items.len(),
        plural(items.len())
    );
    if aggregators > 0 {
        header += &format!(" ({aggregators} aggregator{})", plural(aggregators));
    }
    header.push('\n');

    let facts = render_facts(&items);
    let excerpts = render_excerpts(&items);
    let file = dir.join("report.md");
    let full = format!("{header}\n{facts}{notes}\n{excerpts}");
    let file = std::fs::write(&file, &full).is_ok().then_some(file);
    let footer = file
        .as_ref()
        .map(|f| format!("\nFull report with every excerpt: {}\n", f.display()))
        .unwrap_or_default();

    let failed = items
        .iter()
        .filter(|i| !i.aggregator)
        .map(|i| i.check.name.clone())
        .collect();
    if opts.raw || excerpts.len() <= VERBATIM_MAX {
        return Report {
            text: format!("{header}\n{facts}{notes}\n{excerpts}{footer}"),
            failed,
            file,
        };
    }
    let models: Vec<&str> = match opts.model {
        Some(m) => vec![m.kit_id()],
        None => DISTILL_MODELS.to_vec(),
    };
    let summary = match distill(&format!("{facts}\n{excerpts}"), &dir, &models).await {
        Some(s) => s,
        None => format!(
            "(distill failed; the first excerpts follow)\n\n{}",
            crate::truncate(&excerpts, 3 * VERBATIM_MAX)
        ),
    };
    Report {
        text: format!("{header}\n{facts}{notes}\n{}\n{footer}", summary.trim_end()),
        failed,
        file,
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn report_dir(label: &str, short: &str) -> PathBuf {
    let slug: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let dir = PathBuf::from("/tmp/dragonfly-ci").join(format!("{slug}-{short}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ── Per-check analysis ───────────────────────────────────────────────────────

struct Item {
    check: PrCheck,
    provider: &'static str,
    /// Failed steps, e.g. `13 "TypeScript check with reviewdog"`.
    steps: Option<String>,
    /// Why there is no log, when there is none.
    note: Option<String>,
    excerpt: Option<Excerpt>,
    log_path: Option<PathBuf>,
    health: Option<Health>,
    aggregator: bool,
    /// The check whose first error line this one repeats.
    same_as: Option<String>,
}

impl Item {
    fn unfetched(c: &PrCheck) -> Item {
        Item {
            check: c.clone(),
            provider: crate::classify_provider_label(&c.link),
            steps: None,
            note: Some(format!("not fetched: over the {MAX_LOGS}-log cap")),
            excerpt: None,
            log_path: None,
            health: None,
            aggregator: false,
            same_as: None,
        }
    }
}

/// Where a check's log comes from, read off its link.
#[derive(Debug, PartialEq)]
enum Source {
    /// One GitHub Actions job.
    Actions {
        job: u64,
    },
    /// A commit status linking a whole Actions run: its jobs are checks of their own.
    ActionsRun,
    BuildkiteJob {
        pipeline: String,
        number: u64,
        job: String,
    },
    BuildkiteBuild {
        pipeline: String,
        number: u64,
    },
    /// Only the check-run output or status description GitHub stores.
    Output,
}

fn source(link: &str) -> Source {
    static GHA: OnceLock<Regex> = OnceLock::new();
    static BK: OnceLock<Regex> = OnceLock::new();
    let gha = GHA.get_or_init(|| {
        Regex::new(r"github\.com/[^/]+/[^/]+/actions/runs/(\d+)(?:/job/(\d+))?").unwrap()
    });
    let bk = BK.get_or_init(|| {
        Regex::new(r"buildkite\.com/[^/]+/([^/]+)/builds/(\d+)(?:#([0-9a-f-]{36}))?").unwrap()
    });
    if let Some(c) = gha.captures(link) {
        return match c.get(2).and_then(|j| j.as_str().parse().ok()) {
            Some(job) => Source::Actions { job },
            None => Source::ActionsRun,
        };
    }
    if let Some(c) = bk.captures(link) {
        let pipeline = c[1].to_string();
        let number = c[2].parse().unwrap_or(0);
        return match c.get(3) {
            Some(job) => Source::BuildkiteJob {
                pipeline,
                number,
                job: job.as_str().into(),
            },
            None => Source::BuildkiteBuild { pipeline, number },
        };
    }
    Source::Output
}

/// Where `c`'s log comes from, and the link that says so.
///
/// Buildkite's GitHub app posts check runs that link to github.com; only their
/// output names the Buildkite job, so the output is searched when the link is
/// not a log source.
fn check_source(c: &PrCheck) -> (Source, String) {
    let src = source(&c.link);
    if src != Source::Output {
        return (src, c.link.clone());
    }
    static BK_URL: OnceLock<Regex> = OnceLock::new();
    let re = BK_URL.get_or_init(|| {
        Regex::new(r"https://buildkite\.com/[^\s)\]]+/builds/\d+(#[0-9a-f-]{36})?").unwrap()
    });
    let found = re
        .find_iter(&c.output)
        .map(|m| m.as_str())
        .max_by_key(|u| u.contains('#'));
    match found {
        Some(url) => (source(url), url.to_string()),
        None => (src, c.link.clone()),
    }
}

/// A fetched log plus what the comparison with main needs.
#[derive(Default)]
struct Fetched {
    log: String,
    steps: Option<String>,
    note: Option<String>,
    /// For Actions: the job's name and run, to find the same job on main.
    job_name: Option<String>,
    run_id: Option<u64>,
    /// For Buildkite: the step name and pipeline.
    bk_step: Option<(String, String)>,
    started_at: Option<i64>,
}

async fn analyze(
    c: &PrCheck,
    sha: &str,
    dir: &Path,
    main_history: bool,
    base_at: Option<i64>,
) -> Item {
    let (src, link) = check_source(c);
    let fetched = match &src {
        Source::Actions { job } => fetch_actions(*job, sha).await,
        Source::ActionsRun => Fetched {
            note: Some("aggregates the jobs of its Actions run".into()),
            ..Default::default()
        },
        Source::BuildkiteJob {
            pipeline,
            number,
            job,
        } => fetch_buildkite_job(pipeline, *number, job).await,
        Source::BuildkiteBuild { pipeline, number } => {
            fetch_buildkite_build(pipeline, *number).await
        }
        Source::Output => Fetched::default(),
    };
    let mut log = clean(&fetched.log);
    // Check-run output (Buildkite's summaries, Wiz, Spacelift) is often the best
    // text there is, and it is free: it came with the check list.
    let output = clean(if c.output.is_empty() {
        &c.description
    } else {
        &c.output
    });
    if log.trim().is_empty() {
        log = output.clone();
    } else if !output.trim().is_empty() && !matches!(src, Source::Actions { .. }) {
        log = format!("{output}\n{log}");
    }

    let excerpt = (!log.trim().is_empty()).then(|| excerpt(&log));
    let log_path = (!log.trim().is_empty()).then(|| {
        let p = dir.join(format!("{}.log", file_stem(&c.name)));
        let _ = std::fs::write(&p, &log);
        p
    });
    let aggregator = matches!(src, Source::ActionsRun)
        || excerpt.as_ref().is_some_and(|e| {
            !e.aggregates.is_empty() && (c.name.contains("result") || e.lines < 400)
        });
    let health = if main_history && !aggregator {
        main_health(&fetched, base_at.or(fetched.started_at)).await
    } else {
        None
    };
    let note = fetched.note.or_else(|| {
        log.trim()
            .is_empty()
            .then(|| "no log source; open the link".to_string())
    });
    Item {
        check: c.clone(),
        provider: crate::classify_provider_label(&link),
        steps: fetched.steps,
        note,
        excerpt,
        log_path,
        health,
        aggregator,
        same_as: None,
    }
}

fn file_stem(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.chars().take(64).collect()
}

/// Runs `bin args...` without a shell; stdout on success, else the last stderr line.
async fn cmd(bin: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("failed");
    Err(format!("{bin}: {}", crate::truncate(last.trim(), 200)))
}

#[derive(Deserialize)]
struct ActionsJob {
    name: String,
    run_id: u64,
    head_sha: String,
    started_at: Option<String>,
    #[serde(default)]
    steps: Vec<ActionsStep>,
}

#[derive(Deserialize)]
struct ActionsStep {
    number: u64,
    name: String,
    conclusion: Option<String>,
}

/// Fetches one Actions job's log through the jobs API.
///
/// Unlike `gh run view --log`, the jobs API serves a finished job's log while
/// the rest of its run is still going, which is when a fail-fast watch asks.
async fn fetch_actions(job: u64, sha: &str) -> Fetched {
    let meta_ep = format!("repos/{{owner}}/{{repo}}/actions/jobs/{job}");
    let log_ep = format!("{meta_ep}/logs");
    let meta_args = ["api", meta_ep.as_str()];
    let (meta, log) = tokio::join!(cmd("gh", &meta_args), actions_log(&log_ep));
    let mut f = Fetched::default();
    if let Some(j) = meta
        .ok()
        .and_then(|m| serde_json::from_str::<ActionsJob>(&m).ok())
    {
        let failed: Vec<String> = j
            .steps
            .iter()
            .filter(|s| {
                matches!(
                    s.conclusion.as_deref(),
                    Some("failure" | "timed_out" | "cancelled")
                )
            })
            .map(|s| format!("{} \"{}\"", s.number, s.name))
            .collect();
        f.steps = (!failed.is_empty()).then(|| failed.join(", "));
        if !sha.is_empty() && j.head_sha != sha {
            f.note = Some(format!(
                "job ran on {}, not on this commit",
                &j.head_sha[..7.min(j.head_sha.len())]
            ));
        }
        f.started_at = j.started_at.as_deref().and_then(parse_time);
        f.job_name = Some(j.name);
        f.run_id = Some(j.run_id);
    }
    match log {
        Ok(text) => f.log = text,
        Err(e) => {
            f.note
                .get_or_insert(format!("log not available yet ({e}); retry in a minute"));
        }
    }
    f
}

async fn actions_log(endpoint: &str) -> Result<String, String> {
    // gh >= 2.102 refuses to print a body holding terminal escapes without the flag.
    match cmd("gh", &["api", "--allow-escape-sequences", endpoint]).await {
        Err(e) if e.contains("unknown flag") => cmd("gh", &["api", endpoint]).await,
        r => r,
    }
}

#[derive(Deserialize)]
struct BkBuild {
    #[serde(default)]
    jobs: Vec<BkJob>,
}

#[derive(Deserialize)]
struct BkJob {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    state: Option<String>,
    #[serde(default)]
    soft_failed: bool,
    started_at: Option<String>,
}

async fn bk_build(pipeline: &str, number: u64) -> Result<BkBuild, String> {
    let raw = cmd(
        "bk",
        &["api", &format!("/pipelines/{pipeline}/builds/{number}")],
    )
    .await?;
    serde_json::from_str(&raw).map_err(|e| format!("bk api: {e}"))
}

async fn bk_log(job: &str) -> Result<String, String> {
    cmd("bk", &["job", "log", job, "--no-timestamps"]).await
}

/// Fetches a Buildkite job's log with the `bk` CLI, which carries its own auth.
async fn fetch_buildkite_job(pipeline: &str, number: u64, job: &str) -> Fetched {
    let (build, log) = tokio::join!(bk_build(pipeline, number), bk_log(job));
    let mut f = Fetched::default();
    if let Some(j) = build
        .ok()
        .and_then(|b| b.jobs.into_iter().find(|j| j.id == job))
    {
        f.started_at = j.started_at.as_deref().and_then(parse_time);
        f.bk_step = j.name.map(|n| (pipeline.to_string(), n));
    }
    match log {
        Ok(text) => f.log = text,
        Err(e) => f.note = Some(format!("no Buildkite log ({e})")),
    }
    f
}

/// A status linking a whole Buildkite build: the logs of its failed jobs, up to three.
async fn fetch_buildkite_build(pipeline: &str, number: u64) -> Fetched {
    let build = match bk_build(pipeline, number).await {
        Ok(b) => b,
        Err(e) => {
            return Fetched {
                note: Some(format!("no Buildkite log ({e})")),
                ..Default::default()
            };
        }
    };
    let failed: Vec<BkJob> = build
        .jobs
        .into_iter()
        .filter(|j| {
            j.kind == "script"
                && !j.soft_failed
                && matches!(j.state.as_deref(), Some("failed" | "timed_out"))
        })
        .take(3)
        .collect();
    let ids: Vec<String> = failed.iter().map(|j| j.id.clone()).collect();
    let logs =
        futures::future::join_all(ids.into_iter().map(|id| async move { bk_log(&id).await })).await;
    let mut f = Fetched::default();
    for (j, log) in failed.iter().zip(logs) {
        let name = j.name.as_deref().unwrap_or("unnamed job");
        f.log += &format!("--- Buildkite job: {name}\n{}\n", log.unwrap_or_else(|e| e));
    }
    if failed.is_empty() {
        f.note = Some("no failed job in the build".into());
    }
    f
}

fn parse_time(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp())
}

// ── Main history ─────────────────────────────────────────────────────────────

/// How the check fared on main at `at`.
async fn main_health(f: &Fetched, at: Option<i64>) -> Option<Health> {
    let runs = if let (Some(name), Some(run)) = (&f.job_name, f.run_id) {
        actions_main_runs(name, run, at).await?
    } else if let Some((pipeline, step)) = &f.bk_step {
        flaky::buildkite_main_runs(pipeline, step, MAIN_RUNS, at).await?
    } else {
        return None;
    };
    // A job that never runs on main (a PR-only pipeline) has nothing to say.
    if runs
        .iter()
        .all(|r| !matches!(r.outcome, Outcome::Pass | Outcome::Fail))
    {
        return None;
    }
    Some(flaky::health(&runs, at))
}

/// Slack for main's CI to start on a just-merged commit: its run is created
/// shortly after the commit's committer date.
const BASE_RUN_SLACK_SECS: i64 = 180;

/// When `sha`'s merge base with main landed, plus [BASE_RUN_SLACK_SECS].
///
/// Most jobs check out the PR head, so main's state at the merge base is what a
/// failure can inherit; when a job started is a weaker stand-in, used only when
/// this lookup fails.
async fn base_time(sha: &str) -> Option<i64> {
    let ep = format!("repos/{{owner}}/{{repo}}/compare/main...{sha}");
    let out = cmd(
        "gh",
        &[
            "api",
            &ep,
            "--jq",
            ".merge_base_commit.commit.committer.date",
        ],
    )
    .await
    .ok()?;
    parse_time(out.trim()).map(|t| t + BASE_RUN_SLACK_SECS)
}

#[derive(Deserialize)]
struct WorkflowRun {
    id: u64,
    head_sha: String,
    created_at: String,
    subject: String,
}

#[derive(Deserialize)]
struct RunJob {
    name: String,
    conclusion: Option<String>,
}

/// The job named `name` on main's pushes around `at`, via the workflow of `pr_run`.
///
/// One workflow-runs call finds main's runs of the same workflow, which unlike a
/// per-commit scan ignores commits the workflow's path filters skip. A second
/// call pins the runs from before `at`: for a PR job that ran days ago, main's
/// newest runs all postdate it and say nothing about main when it ran.
async fn actions_main_runs(name: &str, pr_run: u64, at: Option<i64>) -> Option<Vec<MainRun>> {
    let wf = cmd(
        "gh",
        &[
            "api",
            &format!("repos/{{owner}}/{{repo}}/actions/runs/{pr_run}"),
            "--jq",
            ".workflow_id",
        ],
    )
    .await
    .ok()?;
    let base = format!(
        "repos/{{owner}}/{{repo}}/actions/workflows/{}/runs?branch=main&event=push&status=completed&per_page={MAIN_RUNS}",
        wf.trim()
    );
    let before = at
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| format!("{base}&created=<={}", t.format("%Y-%m-%dT%H:%M:%SZ")));
    let (newest, older) = tokio::join!(workflow_runs(&base), async {
        match &before {
            Some(ep) => workflow_runs(ep).await,
            None => Vec::new(),
        }
    });
    let mut runs: Vec<WorkflowRun> = newest;
    for r in older {
        if !runs.iter().any(|x| x.id == r.id) {
            runs.push(r);
        }
    }
    if runs.is_empty() {
        return None;
    }
    runs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let ids: Vec<u64> = runs.iter().map(|r| r.id).collect();
    let outcomes = stream::iter(ids)
        .map(|id| {
            let name = name.to_string();
            async move { job_outcome_in_run(id, &name).await }
        })
        .buffered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    Some(
        runs.into_iter()
            .zip(outcomes)
            .map(|(r, outcome)| MainRun {
                sha: r.head_sha,
                subject: r.subject,
                created_at: parse_time(&r.created_at).unwrap_or(0),
                outcome,
            })
            .collect(),
    )
}

async fn workflow_runs(endpoint: &str) -> Vec<WorkflowRun> {
    let jq = r#".workflow_runs[] | {id, head_sha, created_at, subject: (.head_commit.message // "" | split("\n")[0])}"#;
    let Ok(raw) = cmd("gh", &["api", endpoint, "--jq", jq]).await else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The outcome of job `name` in a finished run; its job list is cached on disk.
async fn job_outcome_in_run(run: u64, name: &str) -> Outcome {
    let cache = home_dir()
        .join(".dragonfly/cache/ci/run-jobs")
        .join(run.to_string());
    let raw = match std::fs::read_to_string(&cache) {
        Ok(s) => s,
        Err(_) => {
            let ep = format!(
                "repos/{{owner}}/{{repo}}/actions/runs/{run}/jobs?per_page=100&filter=latest"
            );
            let Ok(s) = cmd(
                "gh",
                &[
                    "api",
                    "--paginate",
                    &ep,
                    "--jq",
                    ".jobs[] | {name, conclusion}",
                ],
            )
            .await
            else {
                return Outcome::Other;
            };
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&cache, &s);
            s
        }
    };
    let job = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<RunJob>(l).ok())
        .find(|j| j.name == name);
    match job.and_then(|j| j.conclusion).as_deref() {
        Some("success") => Outcome::Pass,
        Some("failure" | "timed_out") => Outcome::Fail,
        Some(_) | None => Outcome::Skip,
    }
}

fn health_line(h: &Health, name: &str) -> String {
    match h {
        Health::Passing { runs } => {
            format!("main: ✅ passing (last {runs} runs) → likely caused by this PR")
        }
        Health::Flaky { failed, runs } => format!(
            "main: ⚠️ intermittent ({failed} of {runs} recent runs failed) → may be flaky; `dragonfly ci rerun {name}`"
        ),
        Health::Broken { failed, runs } => format!(
            "main: ❌ failing too ({failed} of {runs} runs) → not caused by this PR; look for a fix on main"
        ),
        Health::BrokenThen { failed, runs } => format!(
            "main: ❌ failing at this PR's base too ({failed} of {runs} runs before it), no later run to compare → likely inherited; rebase onto main"
        ),
        Health::FixedSince { sha, .. } => format!(
            "main: failing at this PR's base, passing now (at {sha} and later) → rebase onto main"
        ),
        Health::Recovered {
            failed,
            sha,
            subject,
        } => format!(
            "main: was red for {failed} runs but fixed by {sha} \"{subject}\" before this PR's base → likely caused by this PR"
        ),
        Health::NoData => "main: no finished run of this job to compare".into(),
    }
}

// ── Log cleaning and excerpts ────────────────────────────────────────────────

/// Strips what a reader never needs from a raw log: timestamps, ANSI and other
/// control sequences, and `actions/checkout` ref and progress spam.
pub fn clean(raw: &str) -> String {
    static NOISE: OnceLock<[Regex; 3]> = OnceLock::new();
    let [ts, ansi, checkout] = NOISE.get_or_init(|| {
        [
            Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z ?").unwrap(),
            Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b_bk;t=\d+\x07?|_bk;t=\d+\x07?").unwrap(),
            Regex::new(concat!(
                r"\[(new (branch|tag|ref)|tag update|deleted)\]",
                r"|-> origin/\S+( \(forced update\))?$",
                r"|\b(Updating files|Receiving objects|Resolving deltas|Compressing objects",
                r"|Counting objects|Enumerating objects|Filtering content|Checking out files): +\d",
            ))
            .unwrap(),
        ]
    });
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        // `gh run view` renders ESC as a literal `^[`.
        let line = line.replace("^[", "\x1b");
        let line = ansi.replace_all(&line, "");
        let line = ts.replace(&line, "");
        let line: String = line
            .chars()
            .filter(|c| *c == '\t' || !c.is_control())
            .collect();
        if checkout.is_match(&line) {
            continue;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub struct Excerpt {
    pub text: String,
    /// Lines in the whole log.
    pub lines: usize,
    /// The first line that reads as the error itself, for grouping checks.
    pub primary: Option<String>,
    /// Checks this log reports as failed, when it aggregates others.
    pub aggregates: Vec<String>,
}

fn keyword() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"##\[error\]|(?i:\berror\b|\bfail(s|ed|ure|ures)?\b|\bfatal\b|\bpanic(ked)?\b",
            r"|\bexception\b|\btraceback\b|\bassert(ion)?(error)?\b|\btimed out\b|segmentation fault)",
            r"|ERR!|✗|✘|exit(ed with)? (code|status) [1-9]",
        ))
        .unwrap()
    })
}

/// Lines that read as the error itself rather than its echo or wrapper.
fn strong() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"\berror( TS\d+|\[E\d+\])?:|\bError:|--- FAIL|^FAIL\b|\bFAIL:|panic:|fatal:|Traceback",
            r"|✗|✘|ERR!|^\S+\.\w+:\d+(:\d+)?: ",
        ))
        .unwrap()
    })
}

/// Test-runner tallies with nothing failed: `ℹ fail 0`, `0 failed`, `Errors: 0`.
fn zero_tally() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(fail(ed|ures?)?|errors?)\b:?\s+0\b|\b0\s+(fail(ed|ures?)?|errors?)\b")
            .unwrap()
    })
}

/// Generic wrapper lines that say a step failed without saying why.
fn generic() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"Process completed with exit code|##\[error\]Process completed|reported by reviewdog|^\s*shell: ").unwrap()
    })
}

/// True for each line inside an Actions `##[group]Run …` block: the step's script
/// echoed before it runs. Its `exit 1` and `echo "::error::"` lines are source code,
/// not output, and seeding the excerpt with them buries the real error.
fn script_echo(lines: &[&str]) -> Vec<bool> {
    let mut inside = false;
    lines
        .iter()
        .map(|l| {
            if l.starts_with("##[group]Run ") {
                inside = true;
            } else if l.starts_with("##[endgroup]") {
                inside = false;
                return true;
            }
            inside
        })
        .collect()
}

/// Cuts a cleaned log down to its error lines with context plus its tail.
///
/// Lines carry their number in the full log, so a reader can open the log at the
/// right place. Bounded by [CONTEXT_CAP] and [TAIL] whatever the log's size.
pub fn excerpt(log: &str) -> Excerpt {
    static AGG: OnceLock<Regex> = OnceLock::new();
    let agg = AGG.get_or_init(|| {
        Regex::new(r"^\s*(?:❌\s*)?([A-Za-z0-9_./-]+?)(?: result)?(?::| has) (?:failure|failed|cancelled)\s*$|^\s*❌ ([A-Za-z0-9_./-]+) failed\s*$").unwrap()
    });
    let lines: Vec<&str> = log.lines().collect();
    let echo = script_echo(&lines);
    let tail_start = lines.len().saturating_sub(TAIL);

    let mut keep = vec![false; lines.len()];
    let mut primary = None;
    let mut aggregates = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if echo[i] {
            continue;
        }
        if let Some(c) = agg.captures(line) {
            let name = c.get(1).or_else(|| c.get(2)).unwrap().as_str().to_string();
            if !aggregates.contains(&name) {
                aggregates.push(name);
            }
        }
        if !keyword().is_match(line) || generic().is_match(line) || zero_tally().is_match(line) {
            continue;
        }
        if primary.is_none() && strong().is_match(line) {
            primary = Some(line.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        let lo = i.saturating_sub(CONTEXT);
        let hi = (i + CONTEXT + 1).min(tail_start);
        for slot in keep.iter_mut().take(hi).skip(lo) {
            *slot = true;
        }
    }

    let mut kept: Vec<usize> = (0..tail_start).filter(|&i| keep[i]).collect();
    let elided = kept.len().saturating_sub(CONTEXT_CAP);
    if elided > 0 {
        let tail_keep = kept.split_off(CONTEXT_HEAD).split_off(elided);
        kept.extend(tail_keep);
    }
    let width = lines.len().to_string().len();
    let fmt = |i: usize| format!("{:>width$}| {}", i + 1, crate::truncate(lines[i], LINE_MAX));
    let mut out: Vec<String> = Vec::new();
    for (n, &i) in kept.iter().enumerate() {
        if n == CONTEXT_HEAD && elided > 0 {
            out.push(format!("… {elided} error-context lines elided"));
        } else if n > 0 && i != kept[n - 1] + 1 {
            out.push("…".into());
        }
        out.push(fmt(i));
    }
    if tail_start > 0 && kept.last().is_none_or(|&l| l + 1 != tail_start) {
        out.push(if kept.is_empty() {
            format!("… (no error lines before the last {TAIL})")
        } else {
            "…".into()
        });
    }
    out.extend((tail_start..lines.len()).map(fmt));
    Excerpt {
        text: out.join("\n"),
        lines: lines.len(),
        primary,
        aggregates,
    }
}

/// Marks each check whose first error line repeats an earlier check's.
///
/// One root cause routinely fails several jobs (a missing module breaks the
/// build, the bundle budget and e2e); one excerpt per cause is enough.
fn mark_duplicates(items: &mut [Item]) {
    let mut seen: Vec<(String, String)> = Vec::new();
    for item in items.iter_mut() {
        if item.aggregator {
            continue;
        }
        let Some(primary) = item.excerpt.as_ref().and_then(|e| e.primary.clone()) else {
            continue;
        };
        match seen.iter().find(|(p, _)| *p == primary) {
            Some((_, first)) => item.same_as = Some(first.clone()),
            None => seen.push((primary, item.check.name.clone())),
        }
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn render_facts(items: &[Item]) -> String {
    let failing_names: HashSet<&str> = items.iter().map(|i| i.check.name.as_str()).collect();
    let mut out = String::new();
    for i in items {
        let name = &i.check.name;
        if i.aggregator {
            let agg = i
                .excerpt
                .as_ref()
                .map(|e| e.aggregates.join(", "))
                .unwrap_or_default();
            let what = if agg.is_empty() {
                i.note.clone().unwrap_or_default()
            } else {
                format!("aggregates: {agg}")
            };
            out += &format!("↳ {name} ({}) — {what}\n", i.provider);
            continue;
        }
        let mut line = format!("❌ {name} ({})", i.provider);
        if let Some(s) = &i.steps {
            line += &format!(" — failed step {s}");
        }
        if let Some(first) = &i.same_as {
            line += &format!(" — same first error as {first}");
        }
        out += &line;
        out.push('\n');
        if let Some(h) = &i.health {
            out += &format!("   {}\n", health_line(h, name));
        }
        if let Some(n) = &i.note {
            out += &format!("   ({n}) {}\n", i.check.link);
        }
        if let (Some(p), Some(e)) = (&i.log_path, &i.excerpt) {
            out += &format!("   log: {} ({} lines)\n", p.display(), e.lines);
        }
    }
    // Aggregators whose upstream checks are not failing at this commit (rerunning,
    // or named differently) are all there is: say so instead of pointing nowhere.
    let orphaned = items.iter().all(|i| i.aggregator)
        || items.iter().any(|i| {
            i.aggregator
                && i.excerpt.as_ref().is_some_and(|e| {
                    !e.aggregates.is_empty()
                        && e.aggregates
                            .iter()
                            .all(|a| !failing_names.contains(a.as_str()))
                })
        });
    if orphaned && items.iter().all(|i| i.aggregator) {
        out += "   (only aggregators failed: the jobs they report are not failing at this commit, so they may be rerunning)\n";
    }
    out
}

fn render_notes(sel: &Selection) -> String {
    let mut out = String::new();
    for g in &sel.gates {
        out += &format!(
            "🔒 {} — {} (no code change turns it green)\n",
            g.name,
            gate_reason(g)
        );
    }
    if !sel.rerunning.is_empty() {
        let names: Vec<&str> = sel.rerunning.iter().map(|c| c.name.as_str()).collect();
        out += &format!(
            "⏳ rerunning after a failed attempt: {}\n",
            names.join(", ")
        );
    }
    if !sel.cancelled.is_empty() {
        let names: Vec<&str> = sel.cancelled.iter().map(|c| c.name.as_str()).collect();
        out += &format!(
            "🔁 cancelled (usually superseded; `dragonfly ci rerun <name>` if not): {}\n",
            names.join(", ")
        );
    }
    out
}

/// The excerpts to read: one per distinct first error, aggregators only when
/// nothing else failed.
fn render_excerpts(items: &[Item]) -> String {
    let only_aggregators = items.iter().all(|i| i.aggregator);
    let mut out = String::new();
    for i in items {
        if i.same_as.is_some() || (i.aggregator && !only_aggregators) {
            continue;
        }
        let Some(e) = &i.excerpt else { continue };
        out += &format!(
            "## {} (excerpt of {} lines)\n{}\n\n",
            i.check.name, e.lines, e.text
        );
    }
    out
}

const DISTILL_PROMPT: &str = "\
You get CI failure excerpts from the checks of one commit. A coding agent fixes the failures from your summary.
The input opens with one fact line per check (failed step, comparison with main, log path); you do not need to repeat those.
Each `## <check>` section holds numbered lines from that check's log.

Rules:
- Markdown, no code fence around the whole output, under 40 lines.
- Group checks that fail for the same reason into one bucket: a one-line headline naming the error and the checks it affects, then the exact error lines verbatim with their log line numbers (file:line, compiler or linter message, assertion expected/actual, panic message and first in-repo frame).
- Infrastructure failures (runner lost, network, download, out of memory, a timeout with no failing test) are a bucket of their own, labelled infrastructure.
- If a section shows no error line, say \"no error line in the excerpt\" for that check and point at its log. Never claim that a log was truncated.
- No remediation advice and no speculation beyond what the lines show.
";

/// Condenses `text` with the report prompt, for `ci distill`.
pub async fn distill_text(text: &str, model: Option<Model>) -> Option<String> {
    let models: Vec<&str> = match model {
        Some(m) => vec![m.kit_id()],
        None => DISTILL_MODELS.to_vec(),
    };
    let dir = PathBuf::from("/tmp/dragonfly-ci/distill");
    std::fs::create_dir_all(&dir).ok()?;
    distill(text, &dir, &models).await
}

async fn distill(input: &str, dir: &Path, models: &[&str]) -> Option<String> {
    let file = dir.join("distill-input.md");
    std::fs::write(&file, input).ok()?;
    for model in models {
        match tokio::time::timeout(DISTILL_TIMEOUT, call_kit_llm(model, DISTILL_PROMPT, &file))
            .await
        {
            Ok(Some(out)) => return Some(out),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, bucket: &str) -> PrCheck {
        PrCheck {
            name: name.into(),
            bucket: bucket.into(),
            ..Default::default()
        }
    }

    #[test]
    fn source_reads_provider_and_ids_from_the_link() {
        assert_eq!(
            source("https://github.com/o/r/actions/runs/38533041400/job/115749280592"),
            Source::Actions { job: 115749280592 }
        );
        assert_eq!(
            source("https://github.com/o/r/actions/runs/38533041400"),
            Source::ActionsRun
        );
        assert_eq!(
            source(
                "https://buildkite.com/lovable/pull-request-checks/builds/195420#01a13976-74d0-4f7c-a306-a6a762b06e77"
            ),
            Source::BuildkiteJob {
                pipeline: "pull-request-checks".into(),
                number: 195420,
                job: "01a13976-74d0-4f7c-a306-a6a762b06e77".into()
            }
        );
        assert_eq!(
            source("https://buildkite.com/lovable/governance/builds/524598"),
            Source::BuildkiteBuild {
                pipeline: "governance".into(),
                number: 524598
            }
        );
        assert_eq!(source("https://depthfirst.com"), Source::Output);
    }

    #[test]
    fn buildkite_app_checks_find_their_job_in_the_output() {
        let c = PrCheck {
            name: "buildkite/workers-test-result".into(),
            bucket: "fail".into(),
            link: "https://github.com/o/r/runs/123".into(),
            output: "Passing phases: [Buildkite build](https://buildkite.com/lovable/pull-request-checks/builds/195852) · \
                     [Buildkite job](https://buildkite.com/lovable/pull-request-checks/builds/195852#01a139a2-f329-4738-a745-103783fdedc8)"
                .into(),
            ..Default::default()
        };
        let (src, link) = check_source(&c);
        assert_eq!(
            src,
            Source::BuildkiteJob {
                pipeline: "pull-request-checks".into(),
                number: 195852,
                job: "01a139a2-f329-4738-a745-103783fdedc8".into()
            }
        );
        assert!(link.ends_with("#01a139a2-f329-4738-a745-103783fdedc8"));
    }

    // Regression: test-e2e sat on the wait-ignore list, so `ci failures` never reported it.
    #[test]
    fn select_reports_slow_checks_and_sorts_out_gates_and_reruns() {
        let mut rerun = check("test-go", "pending");
        rerun.rerun_of_failure = true;
        let checks = vec![
            check("test-e2e", "fail"),
            check("review", "fail"),
            check("Cursor Bugbot", "fail"),
            rerun,
            check("test-web", "pending"),
            check("lint-go", "cancelled"),
        ];
        let s = select(&checks);
        let names = |v: &[&PrCheck]| v.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&s.failing), ["test-e2e"]);
        assert_eq!(names(&s.gates), ["review"]);
        assert_eq!(names(&s.rerunning), ["test-go"]);
        assert_eq!(names(&s.cancelled), ["lint-go"]);
    }

    #[test]
    fn select_keeps_the_failing_copy_of_a_duplicated_name() {
        let checks = vec![
            check("workers-test-result", "pass"),
            check("workers-test-result", "fail"),
        ];
        assert_eq!(select(&checks).failing.len(), 1);
    }

    #[test]
    fn clean_strips_timestamps_escapes_and_checkout_noise() {
        let raw = "\u{feff}2026-10-14T08:13:20.7484732Z \x1b[31merror\x1b[0m: boom\n\
                   2026-10-14T08:10:08Z  * [new branch]  fix-error -> origin/fix-error\n\
                   _bk;t=1728900000000\x07:bazel: build\n";
        assert_eq!(clean(raw), "error: boom\n:bazel: build\n");
    }

    // Regression: a step's echoed script (`echo "::error::…"`, `exit 1`) seeded the
    // excerpt ahead of the real TypeScript error (PR #126305).
    #[test]
    fn excerpt_skips_script_echo_and_finds_the_real_error() {
        let mut lines = vec!["##[group]Run PNPMSTORE=$(pnpm store path)".to_string()];
        lines.push("  echo \"::error::pnpm store path returned empty\"".into());
        lines.push("  exit 1".into());
        lines.push("##[endgroup]".into());
        lines.extend((0..200).map(|i| format!("building module {i}")));
        lines.push("##[error][typecheck] reported by reviewdog 🐶".into());
        lines.push("web/a.tsx(18,8): error TS2307: Cannot find module '#shared/x'.".into());
        lines.push("##[error]Process completed with exit code 1.".into());
        lines.extend((0..100).map(|i| format!("Test script tag {i}")));
        let e = excerpt(&lines.join("\n"));
        assert!(e.text.contains("TS2307"));
        assert!(!e.text.contains("pnpm store path returned empty"));
        assert_eq!(
            e.primary.as_deref(),
            Some("web/a.tsx(18,8): error TS2307: Cannot find module '#shared/x'.")
        );
        assert!(e.text.contains("206| web/a.tsx(18,8)"));
    }

    // Regression: keyword extraction was unbounded and printed 3.5 MB for PR #124462.
    #[test]
    fn zero_failure_tallies_are_not_errors() {
        for l in [
            "ℹ fail 0",
            "Tests: 0 failed, 12 passed",
            "Errors: 0",
            "17 tests: 17 passed, 0 failed, 0 flaky",
        ] {
            assert!(zero_tally().is_match(l), "{l}");
        }
        for l in [
            "ℹ fail 1",
            "2 failed",
            "error TS2739: Type",
            "Errors  1 error",
        ] {
            assert!(!zero_tally().is_match(l), "{l}");
        }
    }

    #[test]
    fn excerpt_is_bounded_on_huge_noisy_logs() {
        let log: String = (0..50_000)
            .map(|i| format!("line {i}: failed to fetch chunk\n"))
            .collect();
        let e = excerpt(&log);
        assert!(
            e.text.lines().count() <= CONTEXT_CAP + TAIL + 3,
            "{} lines",
            e.text.lines().count()
        );
        assert!(e.text.contains("error-context lines elided"));
        assert_eq!(e.lines, 50_000);
    }

    #[test]
    fn excerpt_names_the_checks_an_aggregator_reports() {
        let log = "js_checks result: failure\nweb_route_tree_check result: skipped\n\
                   test-e2e: failure\n❌ test-web-bundle-budget failed\n";
        assert_eq!(
            excerpt(log).aggregates,
            ["js_checks", "test-e2e", "test-web-bundle-budget"]
        );
    }

    #[test]
    fn duplicates_share_one_excerpt() {
        let item = |name: &str, primary: &str| Item {
            check: check(name, "fail"),
            provider: "github",
            steps: None,
            note: None,
            excerpt: Some(Excerpt {
                text: String::new(),
                lines: 10,
                primary: Some(primary.into()),
                aggregates: Vec::new(),
            }),
            log_path: None,
            health: None,
            aggregator: false,
            same_as: None,
        };
        let mut items = vec![
            item(
                "Build & Deploy Preview",
                "Error: [vite]: Rolldown failed to resolve import",
            ),
            item(
                "test-e2e",
                "Error: [vite]: Rolldown failed to resolve import",
            ),
            item("js_checks", "error TS2307: Cannot find module"),
        ];
        mark_duplicates(&mut items);
        assert_eq!(items[1].same_as.as_deref(), Some("Build & Deploy Preview"));
        assert_eq!(items[2].same_as, None);
        let shown = render_excerpts(&items);
        assert!(shown.contains("## Build & Deploy Preview"));
        assert!(!shown.contains("## test-e2e"));
    }
}
