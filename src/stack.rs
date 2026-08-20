//! PR stack detection for native GitHub stacks (`gh stack`) and Graphite.
//!
//! A stack is discovered per process and cached: detection costs GitHub API
//! calls, and every diff in a run has to agree on which ref the PR is measured
//! against.
//!
//! GitHub wins when both tools claim the branch. `gh stack`'s own local
//! tracking is missing in fresh worktrees, so the chain is read from the open
//! PRs' base refs instead — that works wherever `gh` is authenticated, and it
//! is also what GitHub itself merges against.

use crate::{IGNORED_CHECKS, parse_checks, sh, sh3};
use serde::Deserialize;
use std::path::PathBuf;
use tokio::sync::OnceCell;

/// Branch hops followed in either direction while linking a chain. Bounds the
/// API calls a cyclic or pathological base-ref graph can trigger.
const MAX_DEPTH: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackKind {
    /// Native GitHub stacked PRs (`gh stack`), chained by PR base refs.
    GitHub,
    Graphite,
}

/// A multi-branch stack the current branch belongs to.
#[derive(Clone, Debug)]
pub struct StackInfo {
    pub kind: StackKind,
    /// Stack branches, furthest from trunk first, trunk excluded.
    pub branches: Vec<String>,
    /// The current branch's parent branch, i.e. what its PR merges into.
    pub parent: Option<String>,
    /// Stack listing for the prompt.
    pub viz: String,
    /// True when `gh stack` has no local tracking for this branch, so its
    /// commands refuse to run here. Worktrees and fresh clones see this even
    /// though the stack exists on GitHub.
    pub untracked_locally: bool,
}

/// A stack plus the per-PR CI summary, which costs two API calls per branch.
pub struct StackDetails {
    pub info: StackInfo,
    pub ci_status: String,
}

/// Single-quote a value for a shell command line.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

async fn current_branch() -> Option<String> {
    sh("git branch --show-current")
        .await
        .filter(|b| !b.is_empty())
}

/// The branch stacks are measured from: Graphite's configured trunk, else the
/// remote's default branch, else `main`.
pub async fn trunk_branch() -> String {
    if let Some(t) = graphite_trunk().await {
        return t;
    }
    sh("git symbolic-ref --short refs/remotes/origin/HEAD")
        .await
        .and_then(|r| r.strip_prefix("origin/").map(String::from))
        .unwrap_or_else(|| "main".to_string())
}

async fn graphite_trunk() -> Option<String> {
    // --git-common-dir so this works inside worktrees (where .git is a file).
    let git_common_dir = sh("git rev-parse --git-common-dir").await?;
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
}

/// The stack the current branch is in, or None when it is a standalone branch.
pub async fn detect_stack() -> Option<StackInfo> {
    static CACHE: OnceCell<Option<StackInfo>> = OnceCell::const_new();
    CACHE
        .get_or_init(|| async {
            let trunk = trunk_branch().await;
            // Trunk itself is in no stack. Without this, `gt log short --stack`
            // on trunk reports every tracked branch in the repo as one stack.
            if current_branch().await.as_deref() == Some(trunk.as_str()) {
                return None;
            }
            match github_stack(&trunk).await {
                Some(s) => Some(s),
                None => graphite_stack(&trunk).await,
            }
        })
        .await
        .clone()
}

// ── Native GitHub stacks ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PrLink {
    #[serde(rename = "headRefName")]
    head: String,
    #[serde(rename = "baseRefName")]
    base: String,
}

async fn open_prs(filter: &str) -> Vec<PrLink> {
    let r = sh3(&format!(
        "gh pr list {filter} --state open --json headRefName,baseRefName --limit 5 2>/dev/null"
    ))
    .await;
    serde_json::from_str(&r.stdout).unwrap_or_default()
}

/// The open PR whose head is `branch`, if it has one.
async fn pr_for_head(branch: &str) -> Option<PrLink> {
    open_prs(&format!("--head {}", shq(branch)))
        .await
        .into_iter()
        .find(|p| p.head == branch)
}

/// The single open PR stacked directly on `branch`. A branch point (two PRs
/// sharing a base) has no linear successor, so the chain stops there.
async fn pr_stacked_on(branch: &str) -> Option<PrLink> {
    let children: Vec<PrLink> = open_prs(&format!("--base {}", shq(branch)))
        .await
        .into_iter()
        .filter(|p| p.base == branch)
        .collect();
    match children.len() {
        1 => children.into_iter().next(),
        _ => None,
    }
}

/// Links the open PR base refs around the current branch into a linear stack.
///
/// Requires the current branch to have an open PR: without one there is no
/// recorded parent, and a branch merging straight into trunk is not a stack.
async fn github_stack(trunk: &str) -> Option<StackInfo> {
    let current = current_branch().await?;
    let pr = pr_for_head(&current).await?;
    let parent = pr.base.clone();

    let mut below = Vec::new();
    let mut base = parent.clone();
    while base != *trunk && below.len() < MAX_DEPTH {
        below.push(base.clone());
        match pr_for_head(&base).await {
            Some(p) => base = p.base,
            // An ancestor without an open PR (merged, or never submitted) ends
            // the chain but still leaves the branches below it stacked.
            None => break,
        }
    }

    let mut above = Vec::new();
    let mut top = current.clone();
    while above.len() < MAX_DEPTH {
        let Some(child) = pr_stacked_on(&top).await else {
            break;
        };
        top = child.head.clone();
        above.push(child.head);
    }

    if below.is_empty() && above.is_empty() {
        return None;
    }

    above.reverse();
    let branches: Vec<String> = above
        .into_iter()
        .chain(std::iter::once(current.clone()))
        .chain(below)
        .collect();

    let tracked = sh3("gh stack view --short 2>/dev/null").await;
    let locally_tracked = tracked.code == 0 && !tracked.stdout.trim().is_empty();
    let viz = if locally_tracked {
        tracked.stdout
    } else {
        render_viz(&branches, &current, trunk)
    };

    Some(StackInfo {
        kind: StackKind::GitHub,
        branches,
        parent: Some(parent),
        viz,
        untracked_locally: !locally_tracked,
    })
}

fn render_viz(branches: &[String], current: &str, trunk: &str) -> String {
    let mut out = String::new();
    for b in branches {
        let (marker, suffix) = if b == current {
            ("◉", "  (current)")
        } else {
            ("○", "")
        };
        out.push_str(&format!("{marker} {b}{suffix}\n│\n"));
    }
    out.push_str(&format!("◇ {trunk}  (trunk)"));
    out
}

// ── Graphite stacks ──────────────────────────────────────────────────────────

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
pub async fn is_graphite_branch() -> bool {
    let r = sh3("gt log short --stack 2>/dev/null").await;
    if r.code != 0 || r.stdout.is_empty() {
        return false;
    }
    let trunk = trunk_branch().await;
    !parse_stack_branches(&r.stdout, &trunk).is_empty()
}

/// Uses `gt log short --stack`, which limits output to the current linear
/// stack — no sibling-stack filtering needed.
async fn graphite_stack(trunk: &str) -> Option<StackInfo> {
    let r = sh3("gt log short --stack 2>/dev/null").await;
    if r.code != 0 || r.stdout.is_empty() {
        return None;
    }
    let branches = parse_stack_branches(&r.stdout, trunk);
    // A stack needs at least 2 non-trunk branches (current + ancestor/descendant).
    if branches.len() < 2 {
        return None;
    }
    let current = current_branch().await.unwrap_or_default();
    let parent = branches
        .iter()
        .position(|b| *b == current)
        .and_then(|i| branches.get(i + 1))
        .cloned();
    Some(StackInfo {
        kind: StackKind::Graphite,
        branches,
        parent,
        viz: r.stdout,
        untracked_locally: false,
    })
}

// ── Base ref ─────────────────────────────────────────────────────────────────

/// Returns the git ref to compare HEAD against for "what's in this PR" diffs:
/// the stack parent when the branch is stacked, else `origin/<trunk>`.
///
/// Merge-conflict checks deliberately stay against `origin/main` and use raw
/// strings rather than this helper.
pub async fn pr_base_ref() -> String {
    let trunk = trunk_branch().await;
    let default = format!("origin/{trunk}");
    let Some(parent) = detect_stack().await.and_then(|s| s.parent) else {
        return default;
    };
    // Sanity: the ref we hand to `git diff` must itself be an ancestor of HEAD,
    // not just the local branch it is named after. A parent tip that moved
    // ahead (a restacked or amended ancestor) is not an ancestor, and diffing
    // against it yields the union of both branches' changes.
    for candidate in [format!("origin/{parent}"), parent.clone()] {
        if is_ancestor_of_head(&candidate).await {
            return candidate;
        }
        // Fork point instead: this is the base GitHub itself diffs the PR
        // against, so the range stays the PR's own commits rather than
        // widening to everything since trunk.
        if let Some(base) = merge_base_with_head(&candidate).await {
            return base;
        }
    }
    default
}

/// The fork point of `git_ref` and HEAD, or None when the ref is unknown.
async fn merge_base_with_head(git_ref: &str) -> Option<String> {
    sh(&format!("git merge-base {} HEAD", shq(git_ref)))
        .await
        .filter(|s| !s.is_empty())
}

/// True when `git_ref` exists and is an ancestor of HEAD.
///
/// `merge-base --is-ancestor` exits non-zero for an unknown ref too, so this
/// doubles as an existence check.
async fn is_ancestor_of_head(git_ref: &str) -> bool {
    sh3(&format!(
        "git merge-base --is-ancestor {} HEAD",
        shq(git_ref)
    ))
    .await
    .code
        == 0
}

// ── Per-branch CI status ─────────────────────────────────────────────────────

async fn branch_ci_status(branch: String, is_current: bool) -> String {
    let view_cmd = format!("gh pr view {} --json number 2>/dev/null", shq(&branch));
    let checks_cmd = format!("gh pr checks {} 2>/dev/null", shq(&branch));
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

async fn collect_ci_status(branches: &[String], current: &str) -> String {
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

pub async fn build_stack_details() -> Option<StackDetails> {
    let info = detect_stack().await?;
    let current = current_branch().await.unwrap_or_default();
    let ci_status = collect_ci_status(&info.branches, &current).await;
    Some(StackDetails { info, ci_status })
}

// ── Prompt section ───────────────────────────────────────────────────────────

const GITHUB_WORKFLOW: &str = "\
This branch is part of a native GitHub PR stack. Prefer `gh stack` over raw git so every PR in the stack keeps pointing at the right commits:\n\n\
- `gh stack submit` — push every branch and create/update its PR. A rebase or amend anywhere in the stack rewrites the branches above it, so pushing only the current branch leaves those PRs pointing at orphaned commits.\n\
- `gh stack rebase` — restack the branches above after amending an ancestor; `gh stack sync` pulls trunk and restacks the whole stack.\n\
- `gh stack view` shows the stack; `gh stack up` / `gh stack down` move between its branches.\n\
- Never retarget a PR with `gh pr edit --base`; the stack owns the base refs.\n";

const GITHUB_UNTRACKED: &str = "\
- **This checkout has no local `gh stack` tracking** (the stack exists on GitHub, but `gh stack view` reports the branch is not in a stack — usual in a worktree). `gh stack` commands will refuse to run until `gh stack checkout <pr>` sets tracking up. Until then push single branches with `git push --force-with-lease`, and push the branches above this one too if this branch was rebased or amended.\n";

const GRAPHITE_WORKFLOW: &str = "\
This branch is in a Graphite stack. Prefer `gt` over raw git so stack metadata stays in sync:\n\n\
- `gt submit --no-edit --stack` — push/update the whole stack. `gt absorb` and `gt restack` rewrite ancestor/descendant commits, so pushing only the current branch would leave those PRs pointing at orphaned commits on GitHub. `gt submit` ignores `--title`/`--body`; use `gh pr edit` for those.\n\
- `gt absorb --dry-run` → `gt absorb` — route a staged fix into the ancestor branch whose lines it touches, instead of piling a commit on the current branch.\n\
- `gt restack` — rebase dependents after amending an ancestor or when trunk has moved. Don't use `gt get --force` here; it force-updates siblings from remote.\n";

pub fn stack_section(details: &StackDetails) -> String {
    let (heading, workflow) = match details.info.kind {
        StackKind::GitHub => ("GitHub PR Stack", GITHUB_WORKFLOW),
        StackKind::Graphite => ("Graphite Stack", GRAPHITE_WORKFLOW),
    };
    let untracked = if details.info.untracked_locally {
        GITHUB_UNTRACKED
    } else {
        ""
    };
    format!(
        "\n## {heading}\n\n{workflow}{untracked}\n\
         Current stack:\n\
         ```\n{}\n```\n\n\
         ### Stack PR CI status\n\n\
         **Only the current branch's CI blocks this run.** Ancestor-PR failures are informational — mention them in the final summary, but don't block on or fix them unless the user asks.\n\n\
         {}\n",
        details.info.viz.trim_end(),
        details.ci_status,
    )
}

/// One line naming what a stacked branch needs instead of `git rebase origin/main`.
pub fn restack_hint(kind: StackKind) -> &'static str {
    match kind {
        StackKind::GitHub => "GitHub stack — run `gh stack sync` to restack",
        StackKind::Graphite => "Graphite branch — run `gt sync` to restack",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphite_branches_drop_bullets_trunk_and_annotations() {
        let out = "◉  aron/top (current)\n│\n◯  aron/middle (needs restack)\n│\n◯  main\n";
        assert_eq!(
            parse_stack_branches(out, "main"),
            vec!["aron/top".to_string(), "aron/middle".to_string()]
        );
    }

    #[test]
    fn viz_marks_the_current_branch_and_ends_at_trunk() {
        let branches = vec!["a/top".to_string(), "a/bottom".to_string()];
        let viz = render_viz(&branches, "a/bottom", "main");
        assert_eq!(viz, "○ a/top\n│\n◉ a/bottom  (current)\n│\n◇ main  (trunk)");
    }

    #[test]
    fn untracked_stack_section_says_gh_stack_will_refuse() {
        let mk = |untracked| StackDetails {
            info: StackInfo {
                kind: StackKind::GitHub,
                branches: vec!["a/top".into()],
                parent: Some("a/bottom".into()),
                viz: "○ a/top".into(),
                untracked_locally: untracked,
            },
            ci_status: "- `a/top` — no PR".into(),
        };
        assert!(stack_section(&mk(true)).contains("no local `gh stack` tracking"));
        assert!(!stack_section(&mk(false)).contains("no local `gh stack` tracking"));
    }

    #[test]
    fn shq_survives_a_quote_in_a_branch_name() {
        assert_eq!(shq("a/o'brien"), r"'a/o'\''brien'");
    }
}
