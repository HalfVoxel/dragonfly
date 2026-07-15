// Recent Claude Code session discovery + formatting.
//
// Sessions live at `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` where the
// encoded path is the absolute cwd with every `/`, `.`, and `_` replaced by `-`.
// We pick sessions modified in the last 48h whose events match the current
// branch, strip tools/thinking/sidechains, and write a compact
// `<user>/<agent>` transcript to a temp file.

use chrono::{DateTime, Local};
use regex::Regex;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const MAX_AGE: Duration = Duration::from_secs(48 * 60 * 60);

pub struct SessionFile {
    pub path: PathBuf,
    pub session_id: String,
    pub started_at: DateTime<Local>,
    pub user_messages: usize,
    pub lines: usize,
}

pub fn encode_cwd(path: &str) -> String {
    path.chars()
        .map(|c| if matches!(c, '/' | '.' | '_') { '-' } else { c })
        .collect()
}

/// Strip harness-injected wrapper tags that show up inside user messages —
/// `! cmd` invocations (`<bash-input>`/`<bash-stdout>`/`<bash-stderr>`) and
/// slash-command artifacts (`<command-name>`, `<local-command-*>`, …).
/// Whatever the human actually typed remains.
fn strip_shell_artifacts(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?s)<(?:bash-input|bash-stdout|bash-stderr|local-command-caveat|local-command-stdout|local-command-stderr|command-name|command-message|command-args)\b[^>]*>.*?</(?:bash-input|bash-stdout|bash-stderr|local-command-caveat|local-command-stdout|local-command-stderr|command-name|command-message|command-args)>",
        )
        .expect("valid regex")
    });
    re.replace_all(s, "").trim().to_string()
}

fn strip_cwd<'a>(file_path: &'a str, cwd: &str) -> &'a str {
    if cwd.is_empty() {
        return file_path;
    }
    if let Some(rest) = file_path.strip_prefix(cwd) {
        rest.strip_prefix('/').unwrap_or(rest)
    } else {
        file_path
    }
}

fn projects_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claude").join("projects")
}

pub fn find_recent_sessions(cwd: &str, branch: &str) -> Vec<SessionFile> {
    let dir = projects_dir().join(encode_cwd(cwd));
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let now = SystemTime::now();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = meta.modified().unwrap_or(now);
        if now.duration_since(modified).unwrap_or_default() > MAX_AGE {
            continue;
        }
        if let Some(s) = format_session(&path, branch) {
            out.push(s);
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    out
}

fn format_session(path: &Path, branch: &str) -> Option<SessionFile> {
    let content = std::fs::read_to_string(path).ok()?;
    let session_id = path.file_stem()?.to_string_lossy().to_string();

    // Skip non-interactive sessions (e.g. dragonfly's own PR-areas analysis runs
    // via `claude --print`, which records entrypoint=sdk-cli). The first few
    // events (queue-operation, etc.) often don't carry the field, so probe the
    // first event that does.
    let entrypoint = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| {
            v.get("entrypoint")
                .and_then(|e| e.as_str())
                .map(str::to_string)
        });
    if entrypoint.as_deref() == Some("sdk-cli") {
        return None;
    }

    let mut out = String::new();
    let mut user_messages = 0usize;
    let mut started_at: Option<DateTime<Local>> = None;

    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if v.get("isSidechain")
            .and_then(|s| s.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if v.get("isMeta").and_then(|s| s.as_bool()).unwrap_or(false) {
            continue;
        }

        let event_branch = v.get("gitBranch").and_then(|b| b.as_str()).unwrap_or("");
        if event_branch != branch {
            continue;
        }

        if started_at.is_none() {
            if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                started_at = DateTime::parse_from_rfc3339(ts)
                    .ok()
                    .map(|d| d.with_timezone(&Local));
            }
        }

        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                let role = v
                    .pointer("/message/role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                if role != "user" {
                    continue;
                }
                if let Some(s) = v.pointer("/message/content").and_then(|c| c.as_str()) {
                    // The first user message of a dragonfly session is the
                    // auto-generated prompt — collapse to a short placeholder.
                    if user_messages == 0 && s.trim_start().starts_with("# Dragonfly") {
                        out += "<user>...pull request review instructions...</user>\n";
                        user_messages += 1;
                        continue;
                    }
                    let cleaned = strip_shell_artifacts(s);
                    if cleaned.is_empty() {
                        continue;
                    }
                    out += &format!("<user>{}</user>\n", html_escape::encode_text(&cleaned));
                    user_messages += 1;
                }
                // Array-content user events are tool_results — skip.
            }
            Some("assistant") => {
                let blocks = match v.pointer("/message/content").and_then(|c| c.as_array()) {
                    Some(a) => a,
                    None => continue,
                };
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                let trimmed = t.trim();
                                if !trimmed.is_empty() {
                                    out += &format!(
                                        "<agent>{}</agent>\n",
                                        html_escape::encode_text(trimmed)
                                    );
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                            let verb = match name {
                                "Edit" => Some("Updated"),
                                "Write" => Some("Wrote"),
                                "NotebookEdit" => Some("Updated notebook"),
                                _ => None,
                            };
                            if let Some(verb) = verb {
                                let fp = block
                                    .pointer("/input/file_path")
                                    .and_then(|p| p.as_str())
                                    .or_else(|| {
                                        block
                                            .pointer("/input/notebook_path")
                                            .and_then(|p| p.as_str())
                                    })
                                    .unwrap_or("(unknown)");
                                let event_cwd = v.get("cwd").and_then(|c| c.as_str()).unwrap_or("");
                                let rel = strip_cwd(fp, event_cwd);
                                out += &format!("<agent>{verb} file {rel}</agent>\n");
                            }
                            // Other tools intentionally ignored.
                        }
                        _ => {} // thinking, etc. ignored
                    }
                }
            }
            _ => {} // system, attachment, etc. ignored
        }
    }

    if user_messages == 0 {
        return None;
    }

    let started_at = started_at.unwrap_or_else(Local::now);
    let lines = out.lines().count();

    let tf = tempfile::Builder::new()
        .prefix("psc-session-")
        .suffix(".txt")
        .tempfile_in("/tmp")
        .ok()?;
    let (mut f, output_path) = tf.keep().ok()?;
    f.write_all(out.as_bytes()).ok()?;

    Some(SessionFile {
        path: output_path,
        session_id,
        started_at,
        user_messages,
        lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_shell_artifacts_removes_bash_blocks() {
        let s = "real prompt\n\n<bash-stdout>/some/path</bash-stdout><bash-stderr></bash-stderr>";
        assert_eq!(strip_shell_artifacts(s), "real prompt");

        // `! pwd`-style: only an input/output, no human content
        let s = "<bash-input>pwd</bash-input>";
        assert_eq!(strip_shell_artifacts(s), "");

        // multiline content inside a stripped tag
        let s = "before\n<local-command-caveat>line 1\nline 2</local-command-caveat>\nafter";
        assert_eq!(strip_shell_artifacts(s), "before\n\nafter");
    }

    #[test]
    fn strip_cwd_basic() {
        assert_eq!(strip_cwd("/x/y/go/api/foo.go", "/x/y"), "go/api/foo.go");
        assert_eq!(strip_cwd("/x/y", "/x/y"), "");
        assert_eq!(strip_cwd("/other/path", "/x/y"), "/other/path");
        assert_eq!(strip_cwd("/x/y/foo.go", ""), "/x/y/foo.go");
    }

    #[test]
    fn encode_cwd_replaces_slashes_dots_underscores() {
        assert_eq!(
            encode_cwd("/home/arong/projects/lovable/lovable.aron-fix_tool_cancel"),
            "-home-arong-projects-lovable-lovable-aron-fix-tool-cancel"
        );
    }

    /// Ignored: requires a real dragonfly session file on disk.
    #[test]
    #[ignore]
    fn format_session_collapses_pnc_prompt() {
        let p = Path::new(
            "/home/arong/.claude/projects/-home-arong-projects-lovable-lovable-aron-fix-tool-cancel/f5d3c50c-ad27-46d0-8d9d-a6ab4bc0ab10.jsonl",
        );
        let s = format_session(p, "aron/fix_tool_cancel").expect("formatted");
        let out = std::fs::read_to_string(&s.path).unwrap();
        assert!(
            out.starts_with("<user>...pull request review instructions...</user>"),
            "expected placeholder at start, got: {}",
            &out[..out.len().min(200)]
        );
    }

    /// Ignored by default — only useful when the referenced session file actually exists locally.
    /// Run with: `cargo test --release -- --ignored format_session_smoke --nocapture`
    #[test]
    #[ignore]
    fn format_session_smoke() {
        let p = Path::new(
            "/home/arong/.claude/projects/-home-arong-projects-lovable-lovable-aron-fix-tool-cancel/55ee3d2c-7441-4074-bf7e-3c0bd6cdfcc0.jsonl",
        );
        let s = format_session(p, "aron/fix_tool_cancel").expect("format succeeded");
        let out = std::fs::read_to_string(&s.path).unwrap();
        eprintln!("path: {}", s.path.display());
        eprintln!("user_messages={} lines={}", s.user_messages, s.lines);
        eprintln!("--- first 1000 chars ---\n{}", &out[..out.len().min(1000)]);
        assert!(out.contains("<user>"));
        assert!(out.contains("<agent>"));
        assert!(s.user_messages > 0);
    }
}

pub fn render_section(sessions: &[SessionFile], _branch: &str) -> String {
    if sessions.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "\nAgent Sessions:\n\nSimplified transcripts of recent (last 48h) ai agent sessions. Only user prompts, agent text, and file edits remain. These can be good to read to understand why the PR was created, and how the reasoning went. It's recommended to pass these file paths to the subagent writing the PR description if they are relevant. Avoid reading unnecessarily as they can be large. Note that they likely contain some incorrect details and wrong turns.\n\n"
    );
    for sf in sessions {
        let plural = if sf.user_messages == 1 { "" } else { "s" };
        s += &format!(
            "- `{}` ({} lines) — session `{}…` started {}, {} user message{plural}\n",
            sf.path.display(),
            sf.lines,
            &sf.session_id.chars().take(8).collect::<String>(),
            sf.started_at.format("%Y-%m-%d %H:%M"),
            sf.user_messages,
        );
    }
    s
}
