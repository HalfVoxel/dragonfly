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
//
// Stderr is the diagnostic surface: Claude Code records an MCP server's
// stderr in its debug log, so `eprintln!` here is how field issues get
// traced without affecting the protocol stream.

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

/// One channel event: content plus `<channel>` tag attributes.
/// Meta keys must be identifier-shaped; others are dropped by Claude Code.
type Event = (String, Vec<(&'static str, String)>);

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
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {"resources": []}}));
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

fn send_event(tx: &UnboundedSender<Value>, (content, meta): Event) {
    let meta: serde_json::Map<String, Value> = meta
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v)))
        .collect();
    let _ = tx.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": { "content": content, "meta": meta }
    }));
}

async fn demo_loop(tx: UnboundedSender<Value>) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    send_event(
        &tx,
        (
            "Demo event from `dragonfly watch-mcp --demo`: channel plumbing works. \
             Acknowledge this event to the user, then continue."
                .to_string(),
            vec![("kind", "demo".to_string())],
        ),
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
    /// First non-empty fetch after session start absorbs state silently.
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
                    eprintln!("pr-watch: anchored on {} (branch {branch})", short(&sha));
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
                    eprintln!("pr-watch: new push {}, re-anchoring", short(&head));
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
            let checks: Vec<PrCheck> = checks_for_sha(&state.sha)
                .await
                .into_iter()
                .filter(|c| !crate::WATCH_IGNORED_CHECKS.contains(&c.name.as_str()))
                .collect();
            for ev in ci_events(state, &checks, pr_number.as_deref()) {
                send_event(&tx, ev);
            }
        }

        if pr_number.is_none() {
            pr_number = crate::resolve_pr_number(pr_arg.clone()).await;
            if let Some(pr) = &pr_number {
                eprintln!("pr-watch: watching PR #{pr}");
            }
        }
        if let Some(pr) = &pr_number {
            if owner_repo.is_none() {
                owner_repo = resolve_owner_repo(pr).await;
            }
            if let Some((owner, repo)) = &owner_repo {
                if let Some((threads_json, issues_json)) =
                    fetch_comments_json(owner, repo, pr).await
                {
                    let events = comment_events(
                        &threads_json,
                        &issues_json,
                        pr,
                        &self_login,
                        &mut seen_comments,
                        comments_baselined,
                    );
                    if !comments_baselined {
                        eprintln!(
                            "pr-watch: comment baseline for #{pr}: {} comments",
                            seen_comments.len()
                        );
                    }
                    // Only a successful fetch establishes the baseline;
                    // treating a transient gh failure as one would replay the
                    // entire comment history as "new" on the next success.
                    comments_baselined = true;
                    for ev in events {
                        send_event(&tx, ev);
                    }
                }
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

async fn fetch_comments_json(owner: &str, repo: &str, pr: &str) -> Option<(String, String)> {
    let threads_cmd = format!(
        "gh api graphql -f query='{}' -f owner='{owner}' -f repo='{repo}' -F pr={pr}",
        crate::REVIEW_THREADS_QUERY
    );
    let issues_cmd = format!("gh api repos/{owner}/{repo}/issues/{pr}/comments --paginate");
    let (threads, issues) = tokio::join!(sh(&threads_cmd), sh(&issues_cmd));
    match (threads, issues) {
        (Some(t), Some(i)) => Some((t, i)),
        _ => {
            eprintln!("pr-watch: comment fetch failed for #{pr} (transient?)");
            None
        }
    }
}

/// Diff freshly fetched checks against the last poll's state.
///
/// Emits ci_check_failed for every check newly entering `fail`, and one
/// ci_settled tally when no check is pending anymore. The baseline poll
/// absorbs pre-session state silently, including an already-settled run.
fn ci_events(state: &mut CiState, checks: &[PrCheck], pr: Option<&str>) -> Vec<Event> {
    if checks.is_empty() {
        return Vec::new(); // no runs registered yet, or a transient gh failure
    }
    let mut events = Vec::new();
    let pr_label = pr.map(|p| format!("PR #{p}, ")).unwrap_or_default();

    for c in checks {
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
            events.push((
                content,
                vec![
                    ("kind", "ci_check_failed".to_string()),
                    ("check", c.name.clone()),
                    ("sha", short(&state.sha).to_string()),
                ],
            ));
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
            events.push((
                content,
                vec![
                    ("kind", "ci_settled".to_string()),
                    ("sha", short(&state.sha).to_string()),
                    ("failed", failed.len().to_string()),
                ],
            ));
        }
        state.settled_announced = true;
    }
    state.baseline = false;
    events
}

/// Diff fetched review-thread + issue comments against the seen set.
///
/// Every comment id is recorded in `seen`; ids first observed after the
/// baseline (and not authored by `self_login`) become events.
fn comment_events(
    threads_json: &str,
    issues_json: &str,
    pr: &str,
    self_login: &str,
    seen: &mut HashSet<String>,
    baselined: bool,
) -> Vec<Event> {
    let mut events = Vec::new();

    let threads = serde_json::from_str::<GqlThreadsResponse>(threads_json)
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .and_then(|p| p.review_threads)
        .map(|t| t.nodes)
        .unwrap_or_default();
    for t in &threads {
        let comments = t
            .comments
            .as_ref()
            .map(|c| c.nodes.as_slice())
            .unwrap_or(&[]);
        for c in comments {
            if !seen.insert(c.id.clone()) {
                continue;
            }
            let author = c
                .author
                .as_ref()
                .map(|a| a.login.as_str())
                .unwrap_or("unknown");
            if !baselined || author == self_login {
                continue;
            }
            let loc = match (&t.path, t.line) {
                (Some(p), Some(l)) => format!(" on {p}:{l}"),
                (Some(p), None) => format!(" on {p}"),
                _ => String::new(),
            };
            let status = if t.is_resolved {
                "resolved"
            } else {
                "unresolved"
            };
            events.push((
                format!(
                    "New review comment by {author}{loc} (thread {}, {status}):\n{}",
                    t.id,
                    truncate_body(&c.body)
                ),
                vec![
                    ("kind", "review_comment".to_string()),
                    ("author", author.to_string()),
                    ("thread_id", t.id.clone()),
                ],
            ));
        }
    }

    if let Some(issue_comments) = crate::parse_json::<Vec<IssueComment>>(issues_json) {
        for c in issue_comments {
            let key = format!("issue-{}", c.id);
            if !seen.insert(key) {
                continue;
            }
            let author = c
                .user
                .as_ref()
                .map(|u| u.login.as_str())
                .unwrap_or("unknown");
            if !baselined || author == self_login {
                continue;
            }
            let body = c.body.unwrap_or_default();
            if body.trim().is_empty() {
                continue;
            }
            events.push((
                format!(
                    "New PR comment by {author} on #{pr}:\n{}",
                    truncate_body(&body)
                ),
                vec![
                    ("kind", "pr_comment".to_string()),
                    ("author", author.to_string()),
                ],
            ));
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, bucket: &str) -> PrCheck {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","bucket":"{bucket}","link":"","workflow":"","description":""}}"#
        ))
        .unwrap()
    }

    fn fresh_state(baseline: bool) -> CiState {
        CiState {
            anchor: WatchAnchor::Branch("b".into()),
            sha: "abcdef1234".into(),
            buckets: HashMap::new(),
            settled_announced: false,
            cancel_settle: None,
            baseline,
        }
    }

    fn kinds(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .map(|(_, m)| {
                m.iter()
                    .find(|(k, _)| *k == "kind")
                    .map(|(_, v)| v.as_str())
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn ci_baseline_absorbs_even_failures_then_transitions_fire() {
        let mut state = fresh_state(true);
        // Baseline: an already-red already-settled run stays silent.
        let evs = ci_events(&mut state, &[check("a", "fail"), check("b", "pass")], None);
        assert!(evs.is_empty());
        assert!(state.settled_announced);

        // A check flipping to fail later is news.
        let evs = ci_events(&mut state, &[check("a", "fail"), check("b", "fail")], None);
        assert_eq!(kinds(&evs), ["ci_check_failed"]);
        assert!(evs[0].0.contains(": b"));
    }

    #[test]
    fn ci_pending_to_settled_emits_tally() {
        let mut state = fresh_state(true);
        assert!(ci_events(&mut state, &[check("a", "pending")], Some("42")).is_empty());
        let evs = ci_events(&mut state, &[check("a", "pass")], Some("42"));
        assert_eq!(kinds(&evs), ["ci_settled"]);
        assert!(evs[0].0.contains("PR #42"));
        assert!(evs[0].0.contains("All green"));
        // Settle announced once.
        assert!(ci_events(&mut state, &[check("a", "pass")], Some("42")).is_empty());
    }

    #[test]
    fn ci_fail_then_settle_in_one_poll() {
        let mut state = fresh_state(true);
        assert!(
            ci_events(
                &mut state,
                &[check("a", "pending"), check("b", "pending")],
                None
            )
            .is_empty()
        );
        let evs = ci_events(&mut state, &[check("a", "fail"), check("b", "pass")], None);
        assert_eq!(kinds(&evs), ["ci_check_failed", "ci_settled"]);
        assert!(evs[1].0.contains("1 failed (a)"));
    }

    #[test]
    fn ci_cancelled_set_needs_two_polls_to_settle() {
        let mut state = fresh_state(true);
        assert!(ci_events(&mut state, &[check("a", "pending")], None).is_empty());
        // First poll with the cancelled set: debounced, no settle.
        assert!(ci_events(&mut state, &[check("a", "cancelled")], None).is_empty());
        // Identical set again: settles.
        let evs = ci_events(&mut state, &[check("a", "cancelled")], None);
        assert_eq!(kinds(&evs), ["ci_settled"]);
        assert!(evs[0].0.contains("cancelled (a)"));
    }

    #[test]
    fn ci_empty_fetch_keeps_baseline_pending() {
        let mut state = fresh_state(true);
        assert!(ci_events(&mut state, &[], None).is_empty());
        // Baseline unconsumed: the first real fetch is still silent.
        assert!(state.baseline);
        assert!(ci_events(&mut state, &[check("a", "fail")], None).is_empty());
        assert!(!state.baseline);
    }

    const THREADS: &str = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[
        {"id":"PRRT_1","isResolved":false,"isOutdated":false,"path":"src/a.rs","line":7,
         "comments":{"nodes":[
            {"id":"C_1","author":{"login":"reviewer"},"body":"needs a guard","createdAt":null},
            {"id":"C_2","author":{"login":"me"},"body":"fixed","createdAt":null}
         ]}}
    ]}}}}}"#;

    const ISSUES: &str = r#"[
        {"id":11,"user":{"login":"botty"},"body":"deploy preview ready"},
        {"id":12,"user":{"login":"me"},"body":"thanks"}
    ]"#;

    #[test]
    fn comments_baseline_is_silent_then_new_ids_fire() {
        let mut seen = HashSet::new();
        // Pre-baseline pass records everything, emits nothing.
        let evs = comment_events(THREADS, ISSUES, "42", "me", &mut seen, false);
        assert!(evs.is_empty());
        assert_eq!(seen.len(), 4);
        // Same payload again: all seen, nothing new.
        assert!(comment_events(THREADS, ISSUES, "42", "me", &mut seen, true).is_empty());

        // A new non-self review comment and a new self issue comment arrive.
        let threads2 = THREADS.replace(
            r#"{"id":"C_1","#,
            r#"{"id":"C_3","author":{"login":"reviewer"},"body":"still broken","createdAt":null},{"id":"C_1","#,
        );
        let issues2 = ISSUES.replace(
            r#"{"id":11,"#,
            r#"{"id":13,"user":{"login":"me"},"body":"self reply"},{"id":11,"#,
        );
        let evs = comment_events(&threads2, &issues2, "42", "me", &mut seen, true);
        assert_eq!(kinds(&evs), ["review_comment"]);
        assert!(evs[0].0.contains("still broken"));
        assert!(evs[0].0.contains("src/a.rs:7"));
        assert!(evs[0].0.contains("PRRT_1"));
    }

    #[test]
    fn comments_self_filter_off_when_login_empty() {
        let mut seen = HashSet::new();
        comment_events(THREADS, ISSUES, "42", "", &mut seen, false);
        let issues2 = ISSUES.replace(
            r#"{"id":11,"#,
            r#"{"id":13,"user":{"login":"me"},"body":"self reply"},{"id":11,"#,
        );
        let evs = comment_events(THREADS, &issues2, "42", "", &mut seen, true);
        assert_eq!(kinds(&evs), ["pr_comment"]);
        assert!(evs[0].0.contains("self reply"));
    }

    #[test]
    fn truncate_body_caps_long_comments() {
        let long = "x".repeat(MAX_BODY + 100);
        let out = truncate_body(&long);
        assert!(out.ends_with("[... truncated]"));
        assert!(out.len() < long.len());
    }
}
