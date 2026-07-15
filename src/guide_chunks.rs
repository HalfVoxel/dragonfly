//! Split CLAUDE.md / AGENTS.md guides into chunks at heading boundaries,
//! preserving hierarchy via per-chunk breadcrumbs.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct HeadingCrumb {
    pub level: u8,
    pub title: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GuideChunk {
    /// Absolute path of the source guide.
    pub path: PathBuf,
    /// Heading chain from H1 down to this chunk's own heading, in order.
    /// Empty for the preamble (content before the first heading) and for
    /// files with no headings at all.
    pub breadcrumbs: Vec<HeadingCrumb>,
    /// This chunk's own heading level (1–6). 0 means no heading: either the
    /// preamble or a heading-less file collapsed into a single chunk.
    pub level: u8,
    /// Body of the chunk. Includes the heading line itself when `level > 0`,
    /// so a chunk can be fed to an LLM standalone.
    pub body: String,
}

/// Read each path and split it into chunks at every ATX (`#`-prefixed) or
/// setext (`===` / `---` underline) heading. See [chunk_guide_text] for the
/// exact splitting rules.
///
/// Unreadable paths are skipped silently — callers that care should pre-check.
#[allow(dead_code)]
pub fn chunk_guides<P: AsRef<Path>>(paths: &[P]) -> Vec<GuideChunk> {
    paths
        .iter()
        .flat_map(|p| chunk_guide_file(p.as_ref()))
        .collect()
}

/// See [chunk_guides]. Returns an empty Vec for pure-pointer files (every
/// non-blank line starts with `@`) — e.g. `go/CLAUDE.md` is just
/// `@AGENTS.md`. The pointed-at guide is already pulled in by the @-ref
/// walk in `collect_relevant_guides`, so chunking the pointer produces
/// zero-information rows that just waste scorer tokens.
#[allow(dead_code)]
pub fn chunk_guide_file(path: &Path) -> Vec<GuideChunk> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    if is_pointer_only(&text) {
        return vec![];
    }
    chunk_guide_text(path, &text)
}

/// Expand `selected` to include every ancestor chunk in the same source
/// file. Y is an ancestor of X iff `Y.path == X.path` and Y's breadcrumb
/// chain is a strict, equal-from-the-root prefix of X's.
///
/// Use this after thresholding by score: a chunk like "A > B > C" usually
/// makes more sense to a reviewer alongside its parent headings ("A" and
/// "A > B"), even when those score below the threshold themselves. The
/// preamble (level 0, empty breadcrumbs) is never pulled in — it's a
/// sibling to the first heading, not an ancestor.
///
/// Returns indices into `chunks`, ascending and deduplicated.
#[allow(dead_code)]
pub fn with_ancestors(chunks: &[GuideChunk], selected: &[usize]) -> Vec<usize> {
    use std::collections::BTreeSet;
    let mut result: BTreeSet<usize> = selected.iter().copied().collect();
    for &i in selected {
        let target = &chunks[i];
        if target.breadcrumbs.is_empty() {
            continue;
        }
        for (j, candidate) in chunks.iter().enumerate() {
            if candidate.path != target.path {
                continue;
            }
            if is_strict_prefix(&candidate.breadcrumbs, &target.breadcrumbs) {
                result.insert(j);
            }
        }
    }
    result.into_iter().collect()
}

fn is_strict_prefix(prefix: &[HeadingCrumb], full: &[HeadingCrumb]) -> bool {
    if prefix.is_empty() || prefix.len() >= full.len() {
        return false;
    }
    prefix.iter().zip(full.iter()).all(|(a, b)| a == b)
}

/// True when every non-blank line of `text` starts with `@`, and at least
/// one such line exists. These are wrapper files (typically `CLAUDE.md` →
/// `@AGENTS.md`) that only redirect to a sibling guide.
pub fn is_pointer_only(text: &str) -> bool {
    let mut any = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if !t.starts_with('@') {
            return false;
        }
        any = true;
    }
    any
}

/// Splitting rules:
/// - YAML frontmatter (a `---` … `---` block at the very top) is skipped.
/// - Headings inside fenced code blocks (``` or ~~~) do not split.
///   Regression: `book/src/cheatsheet.md` opens a `bash` fence with dozens
///   of `# shell-style comment` lines; without this guard each one would
///   register as an H1.
/// - ATX headings allow 0–3 leading spaces and an optional trailing `#`
///   sequence, per CommonMark.
/// - Setext underlines require a non-blank, non-`#` previous line and
///   `=`-only / `-`-only underline of length ≥ 2. Setext is rare in guides
///   we surveyed, supported for completeness.
/// - The preamble (content before any heading) is emitted as `level == 0`
///   with empty breadcrumbs iff it has non-whitespace content. A file with
///   zero headings collapses to one such chunk carrying the whole body.
/// - Each chunk's `body` includes its heading line, so the chunk text is
///   self-describing when handed to an LLM. Hierarchy lives in `breadcrumbs`.
#[allow(dead_code)]
pub fn chunk_guide_text(path: &Path, text: &str) -> Vec<GuideChunk> {
    let lines: Vec<&str> = text.lines().collect();
    let start = strip_frontmatter(&lines);

    let mut chunks: Vec<GuideChunk> = Vec::new();
    let mut stack: Vec<HeadingCrumb> = Vec::new();
    let mut cur_body = String::new();
    let mut cur_level: u8 = 0;
    let mut cur_breadcrumbs: Vec<HeadingCrumb> = Vec::new();
    let mut fence: Option<char> = None;

    let mut i = start;
    while i < lines.len() {
        let line = lines[i];

        if let Some(open_char) = fence {
            if is_fence_close(line, open_char) {
                fence = None;
            }
            push_line(&mut cur_body, line);
            i += 1;
            continue;
        }
        if let Some(open_char) = detect_fence_open(line) {
            fence = Some(open_char);
            push_line(&mut cur_body, line);
            i += 1;
            continue;
        }

        if let Some((level, title)) = parse_atx_heading(line) {
            flush_chunk(
                &mut chunks,
                path,
                &mut cur_body,
                cur_level,
                &cur_breadcrumbs,
            );
            stack.retain(|c| c.level < level);
            stack.push(HeadingCrumb { level, title });
            cur_breadcrumbs = stack.clone();
            cur_level = level;
            push_line(&mut cur_body, line);
            i += 1;
            continue;
        }

        if i + 1 < lines.len() {
            if let Some(level) = parse_setext_underline(lines[i + 1], line) {
                flush_chunk(
                    &mut chunks,
                    path,
                    &mut cur_body,
                    cur_level,
                    &cur_breadcrumbs,
                );
                stack.retain(|c| c.level < level);
                stack.push(HeadingCrumb {
                    level,
                    title: line.trim().to_string(),
                });
                cur_breadcrumbs = stack.clone();
                cur_level = level;
                push_line(&mut cur_body, line);
                push_line(&mut cur_body, lines[i + 1]);
                i += 2;
                continue;
            }
        }

        push_line(&mut cur_body, line);
        i += 1;
    }

    flush_chunk(
        &mut chunks,
        path,
        &mut cur_body,
        cur_level,
        &cur_breadcrumbs,
    );
    chunks
}

fn push_line(buf: &mut String, line: &str) {
    buf.push_str(line);
    buf.push('\n');
}

/// Emit the current chunk unless it's empty *and* heading-less (preambles
/// of fully-empty files, or no-op flushes when the very first non-frontmatter
/// line is already a heading).
fn flush_chunk(
    chunks: &mut Vec<GuideChunk>,
    path: &Path,
    body: &mut String,
    level: u8,
    breadcrumbs: &[HeadingCrumb],
) {
    let is_empty_preamble = level == 0 && body.trim().is_empty();
    if !is_empty_preamble {
        chunks.push(GuideChunk {
            path: path.to_path_buf(),
            breadcrumbs: breadcrumbs.to_vec(),
            level,
            body: std::mem::take(body),
        });
    } else {
        body.clear();
    }
}

/// First line after a leading `---` … `---` block, or 0 if no frontmatter.
/// An unterminated `---` at the top is treated as content, not frontmatter.
fn strip_frontmatter(lines: &[&str]) -> usize {
    if lines.first().map(|l| l.trim()) != Some("---") {
        return 0;
    }
    for i in 1..lines.len() {
        if lines[i].trim() == "---" {
            return i + 1;
        }
    }
    0
}

/// ATX heading per CommonMark: up to 3 leading spaces, 1–6 `#`, then a space
/// or end-of-line. Returns (level, title) with optional trailing `#`s stripped.
fn parse_atx_heading(line: &str) -> Option<(u8, String)> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let mut level = 0u8;
    for c in rest.chars() {
        if c == '#' && level < 6 {
            level += 1;
        } else {
            break;
        }
    }
    if level == 0 {
        return None;
    }
    let after = &rest[level as usize..];
    if !after.is_empty() && !after.starts_with(' ') && !after.starts_with('\t') {
        return None;
    }
    let title = after.trim().trim_end_matches('#').trim().to_string();
    Some((level, title))
}

/// Setext underline: `===` (H1) or `---` (H2), length ≥ 2, on its own line,
/// preceded by a non-blank text line that isn't itself an ATX heading.
fn parse_setext_underline(underline: &str, prev_line: &str) -> Option<u8> {
    if prev_line.trim().is_empty() || parse_atx_heading(prev_line).is_some() {
        return None;
    }
    let u = underline.trim();
    if u.len() < 2 {
        return None;
    }
    if u.chars().all(|c| c == '=') {
        return Some(1);
    }
    if u.chars().all(|c| c == '-') {
        return Some(2);
    }
    None
}

/// Returns the fence character (`` ` `` or `~`) if `line` opens a fenced
/// code block, else None. Allows up to 3 leading spaces and an optional
/// info string after the marker.
fn detect_fence_open(line: &str) -> Option<char> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    if t.starts_with("```") {
        return Some('`');
    }
    if t.starts_with("~~~") {
        return Some('~');
    }
    None
}

/// True when `line` is a valid close for a fence opened with `open_char`.
/// The close must be ≥ 3 of `open_char` with no info string.
fn is_fence_close(line: &str, open_char: char) -> bool {
    let t = line.trim();
    t.len() >= 3 && t.chars().all(|c| c == open_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(chunks: &[GuideChunk]) -> Vec<(u8, &str)> {
        chunks
            .iter()
            .map(|c| {
                (
                    c.level,
                    c.breadcrumbs.last().map(|h| h.title.as_str()).unwrap_or(""),
                )
            })
            .collect()
    }

    #[test]
    fn splits_nested_hierarchy_and_carries_breadcrumbs() {
        let text = "\
# A
intro under A
## B
content under B
### C
content under C
## D
content under D
";
        let chunks = chunk_guide_text(Path::new("x.md"), text);
        assert_eq!(
            titles(&chunks),
            vec![(1, "A"), (2, "B"), (3, "C"), (2, "D")],
        );
        let c = &chunks[2];
        assert_eq!(
            c.breadcrumbs
                .iter()
                .map(|h| (h.level, h.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "A"), (2, "B"), (3, "C")],
        );
        // D pops back to level 2 — A still there, B and C gone.
        assert_eq!(
            chunks[3]
                .breadcrumbs
                .iter()
                .map(|h| (h.level, h.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "A"), (2, "D")],
        );
        assert!(chunks[1].body.starts_with("## B\n"));
        assert!(!chunks[1].body.contains("content under C"));
    }

    #[test]
    fn ignores_headings_inside_fenced_code_blocks() {
        // Regression: book/src/cheatsheet.md opens a `bash` fence then has
        // many `# shell-comment` lines. Without fence tracking each one
        // would split as a new H1.
        let text = "\
# Real H1

```bash
# not a heading
## also not a heading
```

## Real H2
";
        let chunks = chunk_guide_text(Path::new("x.md"), text);
        assert_eq!(titles(&chunks), vec![(1, "Real H1"), (2, "Real H2")]);
        assert!(chunks[0].body.contains("# not a heading"));
        assert!(chunks[0].body.contains("```"));
    }

    #[test]
    fn skips_yaml_frontmatter() {
        // Regression: .cursor/rules/*.mdc files start with a YAML block
        // that contains a `description:` line — without skipping it we'd
        // pick up no headings and emit a malformed preamble.
        let text = "\
---
description: foo
globs: /app/**/*.tsx
---

# Title

body
";
        let chunks = chunk_guide_text(Path::new("x.mdc"), text);
        assert_eq!(titles(&chunks), vec![(1, "Title")]);
        assert!(!chunks[0].body.contains("description:"));
    }

    #[test]
    fn preamble_before_first_heading_is_its_own_chunk() {
        let text = "\
some intro text

before any heading

# First
under first
";
        let chunks = chunk_guide_text(Path::new("x.md"), text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].level, 0);
        assert!(chunks[0].breadcrumbs.is_empty());
        assert!(chunks[0].body.contains("some intro text"));
        assert_eq!(chunks[1].level, 1);
    }

    #[test]
    fn file_with_no_headings_collapses_to_one_chunk() {
        let text = "intro paragraph\nsecond line\n";
        let chunks = chunk_guide_text(Path::new("README.md"), text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].level, 0);
        assert_eq!(chunks[0].body, "intro paragraph\nsecond line\n");
    }

    fn chunk_at(path: &str, levels_titles: &[(u8, &str)]) -> GuideChunk {
        GuideChunk {
            path: PathBuf::from(path),
            breadcrumbs: levels_titles
                .iter()
                .map(|(l, t)| HeadingCrumb {
                    level: *l,
                    title: (*t).into(),
                })
                .collect(),
            level: levels_titles.last().map(|(l, _)| *l).unwrap_or(0),
            body: String::new(),
        }
    }

    #[test]
    fn with_ancestors_pulls_in_parents_in_same_file() {
        let chunks = vec![
            chunk_at("F.md", &[(1, "A")]),                     // 0
            chunk_at("F.md", &[(1, "A"), (2, "B")]),           // 1
            chunk_at("F.md", &[(1, "A"), (2, "B"), (3, "C")]), // 2
            chunk_at("F.md", &[(1, "A"), (2, "D")]),           // 3 — sibling of B, not ancestor
        ];
        let kept = with_ancestors(&chunks, &[2]);
        assert_eq!(kept, vec![0, 1, 2]);
    }

    #[test]
    fn with_ancestors_does_not_cross_files() {
        let chunks = vec![
            chunk_at("F.md", &[(1, "A")]),           // 0 — different file
            chunk_at("G.md", &[(1, "A"), (2, "B")]), // 1 — selected
        ];
        let kept = with_ancestors(&chunks, &[1]);
        assert_eq!(
            kept,
            vec![1],
            "ancestor in F.md must not be pulled into G.md's chain"
        );
    }

    #[test]
    fn with_ancestors_ignores_preamble() {
        // Preamble (level 0, empty breadcrumbs) is a sibling, not an
        // ancestor of the first heading; it must not be auto-pulled.
        let chunks = vec![
            chunk_at("F.md", &[]),         // 0 — preamble
            chunk_at("F.md", &[(1, "A")]), // 1
        ];
        let kept = with_ancestors(&chunks, &[1]);
        assert_eq!(kept, vec![1]);
    }

    #[test]
    fn with_ancestors_dedupes_and_handles_multi_select() {
        let chunks = vec![
            chunk_at("F.md", &[(1, "A")]),                     // 0
            chunk_at("F.md", &[(1, "A"), (2, "B")]),           // 1
            chunk_at("F.md", &[(1, "A"), (2, "B"), (3, "C")]), // 2
            chunk_at("F.md", &[(1, "A"), (2, "D")]),           // 3
        ];
        // Select two leaves whose chains overlap at "A".
        let kept = with_ancestors(&chunks, &[2, 3]);
        assert_eq!(kept, vec![0, 1, 2, 3]); // A once, not twice
    }

    #[test]
    fn pointer_only_files_are_detected() {
        assert!(is_pointer_only("@AGENTS.md\n"));
        assert!(is_pointer_only("@AGENTS.md"));
        assert!(is_pointer_only("\n@AGENTS.md\n\n"));
        assert!(is_pointer_only("@AGENTS.md\n@OTHER.md\n"));
        assert!(!is_pointer_only(""));
        assert!(!is_pointer_only("\n\n"));
        assert!(!is_pointer_only("@AGENTS.md\nactual content\n"));
        assert!(!is_pointer_only("# Heading\n@AGENTS.md\n"));
    }

    #[test]
    fn empty_file_yields_no_chunks() {
        assert!(chunk_guide_text(Path::new("x.md"), "").is_empty());
        assert!(chunk_guide_text(Path::new("x.md"), "\n\n").is_empty());
    }

    #[test]
    fn setext_headings_split_correctly() {
        let text = "\
Top Title
=========

intro

Section Two
-----------

more
";
        let chunks = chunk_guide_text(Path::new("x.md"), text);
        assert_eq!(titles(&chunks), vec![(1, "Top Title"), (2, "Section Two")]);
    }

    #[test]
    fn thematic_break_after_blank_is_not_a_setext_underline() {
        // `---` after a blank line is a horizontal rule, not a setext H2.
        let text = "\
# H1

---

paragraph
";
        let chunks = chunk_guide_text(Path::new("x.md"), text);
        assert_eq!(titles(&chunks), vec![(1, "H1")]);
        assert!(chunks[0].body.contains("---"));
    }

    #[test]
    fn deeper_heading_does_not_pop_shallower_siblings() {
        let text = "\
# A
## B
### C
content
";
        let chunks = chunk_guide_text(Path::new("x.md"), text);
        let c = chunks.last().unwrap();
        assert_eq!(
            c.breadcrumbs
                .iter()
                .map(|h| (h.level, h.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "A"), (2, "B"), (3, "C")],
        );
    }
}
