// MCP channel server: pushes CI and PR-review events into a Claude Code
// session (`dragonfly watch-mcp`, spawned by plugin/dragonfly-watcher).
//
// Claude Code starts this process over stdio and treats it as a *channel*
// because the initialize response declares the experimental `claude/channel`
// capability. Every `notifications/claude/channel` notification we write is
// injected into the live session as a `<channel source="pr-watch">` message,
// even while the agent is idle — that is the entire point of this server
// over the pull-style `ci watch` subcommand.
// Protocol: https://code.claude.com/docs/en/channels-reference
//
// Event policy: the first successful fetch is a silent baseline; only
// *transitions* observed while the session is live become events. Without
// the baseline, every session start in a repo with an old red PR would spam
// stale failures the agent didn't cause and can't contextualize.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::{GqlThreadsResponse, PrCheck, WatchAnchor, checks_for_sha, sh};

const INSTRUCTIONS: &str = "Events from the pr-watch channel arrive as \
<channel source=\"pr-watch\" kind=\"...\"> and report GitHub state changes for \
the current branch's PR. They are one-way; there is no reply tool. Kinds: \
ci_check_failed (a check just failed; run `dragonfly ci failures` for error \
logs before fixing), ci_settled (all checks finished; content has the tally), \
review_comment / pr_comment (a new comment arrived; address it, replying or \
resolving via `dragonfly pr thread ...` where appropriate), demo (plumbing \
test; just acknowledge it). If an event is stale or irrelevant to what the \
user is doing, say so briefly instead of acting.";

// Body length cap for comment events. A channel event lands verbatim in the
// session context, so an unbounded bot comment (Graphite tables, CI dumps)
// would waste the whole context budget of a turn.
const MAX_BODY: usize = 1500;

pub async fn watch_mcp_cmd(pr: Option<String>, interval: u64, demo: bool) -> i32 {
    let (tx, mut rx) = unbounded_channel::<Value>();

    // Single writer task: JSON-RPC responses and channel notifications share
    // stdout, and interleaved partial lines would corrupt the transport.
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(v) = rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&v) else {
                continue;
            };
            line.push(b'\n');
            if out.write_all(&line).await.is_err() {
                break;
            }
            let _ = out.flush().await;
        }
    });

    // Polling must not start before the client's `initialized` notification:
    // notifications sent mid-handshake are dropped silently.
    let (init_tx, init_rx) = tokio::sync::oneshot::channel::<()>();
    let poll_tx = tx.clone();
    let poller = tokio::spawn(async move {
        if init_rx.await.is_err() {
            return;
        }
        if demo {
            demo_loop(poll_tx).await;
        } else {
            poll_loop(poll_tx, pr, interval).await;
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut init_tx = Some(init_tx);
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        match (method, id) {
            ("initialize", Some(id)) => {
                let proto = msg
                    .pointer("/params/protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("2025-06-18");
                let _ = tx.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": proto,
                        "capabilities": { "experimental": { "claude/channel": {} } },
                        "serverInfo": {
                            "name": "pr-watch",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "instructions": INSTRUCTIONS,
                    }
                }));
            }
            ("notifications/initialized", None) => {
                if let Some(t) = init_tx.take() {
                    let _ = t.send(());
                }
            }
            ("ping", Some(id)) => {
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {}}));
            }
            // Some clients probe these regardless of declared capabilities;
            // an empty list is friendlier than a protocol error.
            ("tools/list", Some(id)) => {
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}));
            }
            ("prompts/list", Some(id)) => {
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {"prompts": []}}));
            }
            ("resources/list", Some(id)) => {
                let _ =
                    tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {"resources": []}}));
            }
            (m, Some(id)) => {
                let _ = tx.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {m}") }
                }));
            }
            _ => {} // unknown notifications are ignored per JSON-RPC
        }
    }

    // Stdin EOF means Claude Code exited; there is nobody left to notify.
    poller.abort();
    drop(tx);
    let _ = writer.await;
    0
}

fn channel_event(tx: &UnboundedSender<Value>, content: String, meta: &[(&str, String)]) {
    let meta: serde_json::Map<String, Value> = meta
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.clone())))
        .collect();
    let _ = tx.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": { "content": content, "meta": meta }
    }));
}

async fn demo_loop(tx: UnboundedSender<Value>) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    channel_event(
        &tx,
        "Demo event from `dragonfly watch-mcp --demo`: channel plumbing works. \
         Acknowledge this event to the user, then continue."
            .to_string(),
        &[("kind", "demo".to_string())],
    );
    // Stay alive: exiting here would surface as a failed MCP server in /mcp.
    std::future::pending::<()>().await;
}

fn short(sha: &str) -> &str {
    &sha[..7.min(sha.len())]
}

fn truncate_body(body: &str) -> String {
    let body = body.trim();
    if body.len() <= MAX_BODY {
        return body.to_string();
    }
    let mut end = MAX_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[... truncated]", &body[..end])
}

// Issue (top-level PR conversation) comments, REST shape.
#[derive(Deserialize)]
struct IssueComment {
    id: u64,
    user: Option<IssueUser>,
    body: Option<String>,
}
#[derive(Deserialize)]
struct IssueUser {
    login: String,
}

struct CiState {
    anchor: WatchAnchor,
    sha: String,
    /// check name -> last seen bucket
    buckets: HashMap<String, String>,
    settled_announced: bool,
    /// Cancelled-set debounce, mirroring `ci_watch_sha`: a push
    /// cancel-supersedes the old head's runs, so a cancelled set only counts
    /// as terminal when the identical set survives a second poll.
    cancel_settle: Option<Vec<String>>,
    /// First fetch after (re)anchoring absorbs state silently.
    baseline: bool,
}

async fn poll_loop(tx: UnboundedSender<Value>, pr_arg: Option<String>, interval: u64) {
    let interval = Duration::from_secs(interval.max(5));
    // Own comments echo right back after `dragonfly pr thread comment`; they
    // are never news to the agent that just posted them.
    // DRAGONFLY_WATCH_INCLUDE_SELF=1 disables the filter so a single-account
    // test can observe its own comments as events.
    let self_login = if std::env::var("DRAGONFLY_WATCH_INCLUDE_SELF").is_ok() {
        String::new()
    } else {
        sh("gh api user --jq .login").await.unwrap_or_default()
    };

    let mut ci: Option<CiState> = None;
    let mut pr_number: Option<String> = None;
    let mut owner_repo: Option<(String, String)> = None;
    let mut seen_comments: HashSet<String> = HashSet::new();
    let mut comments_baselined = false;

    loop {
        // Anchor lazily and re-anchor on a fresh push, like `ci watch`.
        match &mut ci {
            None => {
                if let Some((sha, branch)) = crate::resolve_watch_anchor(pr_arg.clone()).await {
                    let anchor = match &pr_arg {
                        Some(p) => WatchAnchor::Pr(p.clone()),
                        None => WatchAnchor::Branch(branch),
                    };
                    ci = Some(CiState {
                        anchor,
                        sha,
                        buckets: HashMap::new(),
                        settled_announced: false,
                        cancel_settle: None,
                        baseline: true,
                    });
                }
            }
            Some(state) => {
                if let Some(head) = state.anchor.current_head().await
                    && head != state.sha
                {
                    state.sha = head;
                    state.buckets.clear();
                    state.settled_announced = false;
                    state.cancel_settle = None;
                    // Not a baseline: the push happened during the session,
                    // so pending->fail transitions on the new head are news.
                    state.baseline = false;
                }
            }
        }

        if let Some(state) = &mut ci {
            poll_ci(&tx, state, pr_number.as_deref()).await;
        }

        if pr_number.is_none() {
            pr_number = crate::resolve_pr_number(pr_arg.clone()).await;
        }
        if let Some(pr) = &pr_number {
            if owner_repo.is_none() {
                owner_repo = resolve_owner_repo(pr).await;
            }
            if let Some((owner, repo)) = &owner_repo {
                let fetched = poll_comments(
                    &tx,
                    owner,
                    repo,
                    pr,
                    &self_login,
                    &mut seen_comments,
                    comments_baselined,
                )
                .await;
                // Only a successful fetch establishes the baseline; treating
                // a transient gh failure as one would replay the entire
                // comment history as "new" on the next success.
                comments_baselined = comments_baselined || fetched;
            }
        }

        tokio::time::sleep(interval).await;
    }
}

/// Base-repo owner/name parsed from the PR URL, same idiom as `pr_comments`.
async fn resolve_owner_repo(pr: &str) -> Option<(String, String)> {
    let url = sh(&format!("gh pr view {pr} --json url --jq .url")).await?;
    let parts: Vec<&str> = url.split('/').collect();
    if parts.len() < 5 {
        return None;
    }
    Some((parts[3].to_string(), parts[4].to_string()))
}

async fn poll_ci(tx: &UnboundedSender<Value>, state: &mut CiState, pr: Option<&str>) {
    let checks: Vec<PrCheck> = checks_for_sha(&state.sha)
        .await
        .into_iter()
        .filter(|c| !crate::WATCH_IGNORED_CHECKS.contains(&c.name.as_str()))
        .collect();
    if checks.is_empty() {
        return; // no runs registered yet, or a transient gh failure
    }
    let pr_label = pr.map(|p| format!("PR #{p}, ")).unwrap_or_default();

    for c in &checks {
        let prev = state.buckets.get(&c.name).map(String::as_str);
        if !state.baseline && c.bucket == "fail" && prev != Some("fail") {
            let mut content = format!(
                "CI check failed ({pr_label}commit {}): {}",
                short(&state.sha),
                c.name
            );
            if !c.description.is_empty() {
                content += &format!("\n{}", c.description);
            }
            if !c.link.is_empty() {
                content += &format!("\n{}", c.link);
            }
            content += "\nRun `dragonfly ci failures` for the error log.";
            channel_event(
                tx,
                content,
                &[
                    ("kind", "ci_check_failed".to_string()),
                    ("check", c.name.clone()),
                    ("sha", short(&state.sha).to_string()),
                ],
            );
        }
        state.buckets.insert(c.name.clone(), c.bucket.clone());
    }

    let pending = checks.iter().filter(|c| c.bucket == "pending").count();
    let cancelled: Vec<String> = checks
        .iter()
        .filter(|c| c.bucket == "cancelled")
        .map(|c| c.name.clone())
        .collect();
    let mut settled = pending == 0;
    if settled && !cancelled.is_empty() {
        match &state.cancel_settle {
            Some(prev) if *prev == cancelled => {}
            _ => {
                state.cancel_settle = Some(cancelled.clone());
                settled = false;
            }
        }
    } else if cancelled.is_empty() {
        state.cancel_settle = None;
    }

    if settled && !state.settled_announced {
        if !state.baseline {
            let failed: Vec<&str> = checks
                .iter()
                .filter(|c| c.bucket == "fail")
                .map(|c| c.name.as_str())
                .collect();
            let passed = checks.iter().filter(|c| c.bucket == "pass").count();
            let skipped = checks.iter().filter(|c| c.bucket == "skipping").count();
            let mut content = format!(
                "CI settled ({pr_label}commit {}): {passed} passed",
                short(&state.sha)
            );
            if !failed.is_empty() {
                content += &format!(", {} failed ({})", failed.len(), failed.join(", "));
            }
            if skipped > 0 {
                content += &format!(", {skipped} skipped");
            }
            if !cancelled.is_empty() {
                content += &format!(", {} cancelled ({})", cancelled.len(), cancelled.join(", "));
            }
            content += if failed.is_empty() && cancelled.is_empty() {
                ". All green."
            } else {
                "."
            };
            channel_event(
                tx,
                content,
                &[
                    ("kind", "ci_settled".to_string()),
                    ("sha", short(&state.sha).to_string()),
                    ("failed", failed.len().to_string()),
                ],
            );
        }
        state.settled_announced = true;
    }
    state.baseline = false;
}

/// Returns true when both fetches succeeded (baseline may be established).
async fn poll_comments(
    tx: &UnboundedSender<Value>,
    owner: &str,
    repo: &str,
    pr: &str,
    self_login: &str,
    seen: &mut HashSet<String>,
    baselined: bool,
) -> bool {
    let threads_cmd = format!(
        "gh api graphql -f query='{}' -f owner='{owner}' -f repo='{repo}' -F pr={pr}",
        crate::REVIEW_THREADS_QUERY
    );
    let issues_cmd =
        format!("gh api repos/{owner}/{repo}/issues/{pr}/comments --paginate");
    let (threads_out, issues_out) = tokio::join!(sh(&threads_cmd), sh(&issues_cmd));
    let (Some(threads_out), Some(issues_out)) = (threads_out, issues_out) else {
        return false;
    };

    let threads = serde_json::from_str::<GqlThreadsResponse>(&threads_out)
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .and_then(|p| p.review_threads)
        .map(|t| t.nodes)
        .unwrap_or_default();
    for t in &threads {
        let comments = t.comments.as_ref().map(|c| c.nodes.as_slice()).unwrap_or(&[]);
        for c in comments {
            if !seen.insert(c.id.clone()) {
                continue;
            }
            let author = c.author.as_ref().map(|a| a.login.as_str()).unwrap_or("unknown");
            if !baselined || author == self_login {
                continue;
            }
            let loc = match (&t.path, t.line) {
                (Some(p), Some(l)) => format!(" on {p}:{l}"),
                (Some(p), None) => format!(" on {p}"),
                _ => String::new(),
            };
            let status = if t.is_resolved { "resolved" } else { "unresolved" };
            channel_event(
                tx,
                format!(
                    "New review comment by {author}{loc} (thread {}, {status}):\n{}",
                    t.id,
                    truncate_body(&c.body)
                ),
                &[
                    ("kind", "review_comment".to_string()),
                    ("author", author.to_string()),
                    ("thread_id", t.id.clone()),
                ],
            );
        }
    }

    if let Some(issue_comments) = crate::parse_json::<Vec<IssueComment>>(&issues_out) {
        for c in issue_comments {
            let key = format!("issue-{}", c.id);
            if !seen.insert(key) {
                continue;
            }
            let author = c.user.as_ref().map(|u| u.login.as_str()).unwrap_or("unknown");
            if !baselined || author == self_login {
                continue;
            }
            let body = c.body.unwrap_or_default();
            if body.trim().is_empty() {
                continue;
            }
            channel_event(
                tx,
                format!(
                    "New PR comment by {author} on #{pr}:\n{}",
                    truncate_body(&body)
                ),
                &[
                    ("kind", "pr_comment".to_string()),
                    ("author", author.to_string()),
                ],
            );
        }
    }
    true
}
