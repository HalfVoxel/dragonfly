//! Generator for the dragonfly-review plugin's vendored agent bodies.
//!
//! The plugin ships copies of `agents/*.md` because an installed plugin is
//! copied into Claude Code's plugin cache and cannot reference files outside
//! its own root. Hand-maintaining those copies let real content drift in (the
//! plugin's test-reviewer silently lost its synctest and flakiness criteria for
//! months), so the bodies are derived instead: `agents/<name>.md` is the single
//! source of truth and [gen_plugin_agents] rewrites the plugin copy from it.
//!
//! What generation owns: the body only. Frontmatter is deliberately divergent
//! (the plugin pins `tools:` for read-only enforcement, uses `model: inherit`,
//! and its `description` names the skill that spawns it), so the plugin file's
//! own frontmatter is preserved verbatim.
//!
//! Two body deltas are declared per agent instead of tolerated:
//!
//! - `Read: @../code-comments.md` expands to the guide inlined verbatim.
//!   `@`-refs resolve against the session cwd (the repo under review), never
//!   the agent file's directory, so the reference cannot survive into a plugin.
//! - `comment-reviewer` reads inlined `<diff>` blocks, because its hook runs
//!   `dragonfly prompt review-agent --inline-diffs`.
//!
//! Every declared substitution must match exactly once. Editing the source
//! sentence in `agents/` therefore fails generation loudly rather than
//! producing a plugin body that quietly kept the old wording.

use std::path::{Path, PathBuf};

/// A repo agent and the body edits its plugin copy needs.
struct AgentSpec {
    name: &'static str,
    /// Applied in order; each must match exactly once.
    subs: &'static [(&'static str, &'static str)],
}

const GUIDE_REF: &str = "Read: @../code-comments.md";

const SPECS: &[AgentSpec] = &[
    AgentSpec {
        name: "review-agent",
        subs: &[],
    },
    AgentSpec {
        name: "comment-reviewer",
        subs: &[(
            "Read the per-file diff files listed in the context.",
            "Read the inlined `<diff name=\"…\">` blocks.",
        )],
    },
    AgentSpec {
        name: "dedup-reviewer",
        subs: &[],
    },
    AgentSpec {
        name: "test-reviewer",
        subs: &[],
    },
];

/// Repo checkout the generator reads and writes. Compiled in, so the command
/// works from any cwd but only ever targets the source tree it was built from.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_agent(name: &str) -> PathBuf {
    repo_root().join(format!("agents/{name}.md"))
}

fn plugin_agent(name: &str) -> PathBuf {
    repo_root().join(format!("plugin/dragonfly-review/agents/{name}.md"))
}

/// Splits a leading `---\n…\n---\n` block, returning (frontmatter, body) with
/// the fence lines dropped. Errors on a file without one.
fn split_raw(text: &str, what: &Path) -> Result<(String, String), String> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| format!("{}: no leading '---'", what.display()))?;
    let end = rest
        .find("\n---\n")
        .ok_or_else(|| format!("{}: unterminated frontmatter", what.display()))?;
    let fm = rest[..end].to_string();
    let body = rest[end + "\n---\n".len()..]
        .trim_start_matches('\n')
        .to_string();
    Ok((fm, body))
}

fn apply_once(body: String, old: &str, new: &str, agent: &str) -> Result<String, String> {
    match body.matches(old).count() {
        1 => Ok(body.replace(old, new)),
        n => Err(format!(
            "{agent}: expected 1 occurrence of {old:?} in agents/{agent}.md, found {n}. \
             Update SPECS in src/gen_plugin.rs to match the new wording."
        )),
    }
}

/// Renders the plugin copy of one agent: the plugin file's frontmatter over a
/// body derived from the repo agent.
fn render(spec: &AgentSpec) -> Result<String, String> {
    let repo_path = repo_agent(spec.name);
    let plugin_path = plugin_agent(spec.name);
    let repo_src =
        std::fs::read_to_string(&repo_path).map_err(|e| format!("{}: {e}", repo_path.display()))?;
    let plugin_src = std::fs::read_to_string(&plugin_path)
        .map_err(|e| format!("{}: {e}", plugin_path.display()))?;

    let (_, mut body) = split_raw(&repo_src, &repo_path)?;
    let (frontmatter, _) = split_raw(&plugin_src, &plugin_path)?;

    if body.contains(GUIDE_REF) {
        body = apply_once(
            body,
            GUIDE_REF,
            crate::skill::CODE_COMMENTS_GUIDE.trim(),
            spec.name,
        )?;
    }
    for (old, new) in spec.subs {
        body = apply_once(body, old, new, spec.name)?;
    }
    // Invariant: no @-ref survives into a plugin body. One that did would
    // resolve against the reviewed repo's cwd and silently miss.
    if let Some(i) = body.find("@../") {
        return Err(format!(
            "{}: unexpanded @-ref at byte {i}: {:?}. Declare it in SPECS.",
            spec.name,
            &body[i..(i + 40).min(body.len())]
        ));
    }

    Ok(format!("---\n{frontmatter}\n---\n\n{body}"))
}

/// Regenerates every plugin agent body from `agents/`.
///
/// With `check`, reports which files are stale and writes nothing. Returns the
/// process exit code: 0 when everything is in sync (or was written), 1 on a
/// stale file under `check` or on any render error.
pub fn gen_plugin_agents(check: bool) -> i32 {
    let mut stale = Vec::new();
    for spec in SPECS {
        let want = match render(spec) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gen-plugin-agents: {e}");
                return 1;
            }
        };
        let path = plugin_agent(spec.name);
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        if have == want {
            continue;
        }
        stale.push(spec.name);
        if check {
            continue;
        }
        if let Err(e) = std::fs::write(&path, &want) {
            eprintln!("gen-plugin-agents: {}: {e}", path.display());
            return 1;
        }
        println!("wrote {}", path.display());
    }
    if stale.is_empty() {
        println!("gen-plugin-agents: all {} agents in sync", SPECS.len());
        return 0;
    }
    if check {
        eprintln!(
            "gen-plugin-agents: stale plugin agents: {}\n\
             Run `dragonfly gen-plugin-agents` to regenerate.",
            stale.join(", ")
        );
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the plugin's test-reviewer lost two review criteria that
    /// were added to `agents/test-reviewer.md`, and nothing failed. Any edit to
    /// `agents/*.md` must be reflected in the vendored copy.
    #[test]
    fn plugin_agents_match_generated() {
        for spec in SPECS {
            let want = render(spec).expect("render");
            let have = std::fs::read_to_string(plugin_agent(spec.name)).expect("read plugin agent");
            assert_eq!(
                have, want,
                "plugin/dragonfly-review/agents/{}.md is stale; run `cargo run -- gen-plugin-agents`",
                spec.name
            );
        }
    }

    #[test]
    fn render_inlines_the_guide_and_keeps_plugin_frontmatter() {
        for name in ["comment-reviewer", "test-reviewer"] {
            let spec = SPECS.iter().find(|s| s.name == name).unwrap();
            let out = render(spec).unwrap();
            assert!(out.contains(crate::skill::CODE_COMMENTS_GUIDE.trim()));
            assert!(!out.contains(GUIDE_REF));
            // The plugin's read-only tool pin is frontmatter, so it survives.
            assert!(out.contains("tools: Read, Grep, Glob, Bash"));
        }
    }

    #[test]
    fn apply_once_rejects_a_missing_or_repeated_match() {
        assert!(apply_once("a b".into(), "zz", "y", "x").is_err());
        assert!(apply_once("a a".into(), "a", "y", "x").is_err());
        assert_eq!(apply_once("a b".into(), "a", "y", "x").unwrap(), "y b");
    }
}
