//! Score guide chunks against a PR's diff via `lov rag score`. Drives the
//! Lovable knowledge-RAG scorer (`KnowledgeRAGService.ScoreFiles`) from
//! arbitrary chunks instead of the registered knowledge-base.

use crate::guide_chunks::GuideChunk;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

#[derive(serde::Deserialize)]
struct LovScore {
    file: String,
    score: f64,
}

/// Spawn `lov rag score` once with every chunk materialized as a temp file,
/// then return scores parallel to `chunks` (index-aligned). Chunks the
/// scorer didn't return — same model that already drops files at 0 when
/// parsing fails (`scores.go:327-361`) — come back as 0.0; treat 0 as drop.
///
/// `LOV_PATH` overrides the binary; otherwise `lov` on PATH is used. The
/// rag-score subcommand lives on an unmerged worktree branch as of this
/// writing, so PATH may need to point at that build.
pub async fn score_chunks(chunks: &[GuideChunk], query: &str) -> Result<Vec<f64>, String> {
    if chunks.is_empty() {
        return Ok(vec![]);
    }

    let tmp_dir = std::env::temp_dir().join(format!(
        "dragonfly-chunks-{}-{}",
        std::process::id(),
        // Distinguish concurrent score_chunks calls in the same process.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("create tmp dir: {e}"))?;

    // Opaque filenames so the LLM gets no naming hint — only the body
    // (which already includes the heading line) informs scoring.
    let mut path_to_idx: HashMap<String, usize> = HashMap::with_capacity(chunks.len());
    let mut chunk_paths: Vec<PathBuf> = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let fname = tmp_dir.join(format!("chunk_{i:04}.md"));
        std::fs::write(&fname, &chunk.body).map_err(|e| format!("write chunk: {e}"))?;
        path_to_idx.insert(fname.to_string_lossy().into_owned(), i);
        chunk_paths.push(fname);
    }

    let lov = std::env::var_os("LOV_PATH").unwrap_or_else(|| "lov".into());
    let mut cmd = Command::new(&lov);
    cmd.arg("rag").arg("score").arg("--query").arg(query);
    for p in &chunk_paths {
        cmd.arg(p);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd.output().await.map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            format!(
                "`{}` not found — install with `lov-install` from a worktree that has `rag score`, \
                 or set LOV_PATH to its absolute path",
                lov.to_string_lossy(),
            )
        } else {
            format!("spawn lov: {e}")
        }
    })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("lov rag score failed: {}", err.trim()));
    }

    let scored: Vec<LovScore> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parse lov output: {e}"))?;

    let mut scores = vec![0.0_f64; chunks.len()];
    for s in scored {
        if let Some(&idx) = path_to_idx.get(&s.file) {
            scores[idx] = s.score;
        }
    }

    // Best-effort cleanup. Leaving the tmp dir around on error is fine —
    // dragonfly never runs as a long-lived service.
    let _ = std::fs::remove_dir_all(&tmp_dir);

    Ok(scores)
}

/// Render kept chunks as a `<relevant-context>` block grouped by source
/// file. Chunks from the same path are concatenated in the order they
/// appeared in the original guide; files appear in the order they were
/// first encountered in `kept`. `path` attributes are relative to `cwd`
/// when the guide lives under it; falls back to the absolute path
/// otherwise (rare — guides are walked from CWD-rooted git toplevel).
///
/// Returns an empty string when `kept` is empty so callers can splice
/// the result unconditionally.
pub fn render_relevant_context(
    chunks: &[GuideChunk],
    kept: &[usize],
    cwd: &Path,
) -> String {
    if kept.is_empty() {
        return String::new();
    }

    let mut files_in_order: Vec<PathBuf> = Vec::new();
    let mut by_file: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for &i in kept {
        let p = &chunks[i].path;
        if !by_file.contains_key(p) {
            files_in_order.push(p.clone());
        }
        by_file.entry(p.clone()).or_default().push(i);
    }

    let mut out = String::from("<relevant-context>\n");
    for path in &files_in_order {
        let rel = path
            .strip_prefix(cwd)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.clone());
        out.push_str(&format!("<excerpt path=\"{}\">\n", rel.display()));
        for &i in by_file.get(path).unwrap() {
            out.push_str(&chunks[i].body);
            if !chunks[i].body.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str("</excerpt>\n");
    }
    out.push_str("</relevant-context>\n");
    out
}

/// TSV sorted by score desc. Columns:
/// `score | level | heading_chain | path | lines | preview`.
/// `preview` is the first non-blank, non-heading body line truncated to
/// 120 chars; tabs and newlines in any field are replaced with spaces so
/// the file parses as TSV.
pub fn write_scores_tsv<W: Write>(
    writer: &mut W,
    chunks: &[GuideChunk],
    scores: &[f64],
) -> io::Result<()> {
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));

    writeln!(writer, "score\tlevel\theading_chain\tpath\tlines\tpreview")?;
    for i in order {
        let chunk = &chunks[i];
        let chain = chunk
            .breadcrumbs
            .iter()
            .map(|h| h.title.as_str())
            .collect::<Vec<_>>()
            .join(" > ");
        let lines = chunk.body.lines().count();
        let preview = chunk
            .body
            .lines()
            .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .unwrap_or("")
            .chars()
            .take(120)
            .collect::<String>();
        writeln!(
            writer,
            "{:.1}\t{}\t{}\t{}\t{}\t{}",
            scores[i],
            chunk.level,
            tsv_sanitize(&chain),
            chunk.path.display(),
            lines,
            tsv_sanitize(&preview),
        )?;
    }
    Ok(())
}

fn tsv_sanitize(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide_chunks::HeadingCrumb;

    fn chunk(path: &str, level: u8, title: &str, body: &str) -> GuideChunk {
        GuideChunk {
            path: PathBuf::from(path),
            breadcrumbs: vec![HeadingCrumb { level, title: title.into() }],
            level,
            body: body.into(),
        }
    }

    #[test]
    fn render_groups_by_file_concatenates_in_original_order_relative_paths() {
        let cwd = Path::new("/repo");
        let chunks = vec![
            chunk("/repo/A.md", 1, "Alpha", "# Alpha\nintro\n"),       // 0
            chunk("/repo/A.md", 2, "Beta",  "## Beta\nbody beta\n"),   // 1
            chunk("/repo/B.md", 1, "Gamma", "# Gamma\nbody gamma\n"),  // 2
            chunk("/repo/A.md", 2, "Delta", "## Delta\nbody delta\n"), // 3
        ];
        // Mixed order: A's chunks should still appear in 0,1,3 order
        // inside one excerpt; B's stays in its own excerpt; files appear
        // in the order they're first encountered in `kept`.
        let kept = vec![1, 2, 3, 0];
        let out = render_relevant_context(&chunks, &kept, cwd);
        let expected = "\
<relevant-context>
<excerpt path=\"A.md\">
## Beta
body beta
## Delta
body delta
# Alpha
intro
</excerpt>
<excerpt path=\"B.md\">
# Gamma
body gamma
</excerpt>
</relevant-context>
";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_empty_kept_returns_empty_string() {
        let chunks = vec![chunk("/repo/A.md", 1, "X", "# X\n")];
        assert_eq!(render_relevant_context(&chunks, &[], Path::new("/repo")), "");
    }

    #[test]
    fn render_falls_back_to_absolute_when_path_outside_cwd() {
        let chunks = vec![chunk("/elsewhere/A.md", 1, "X", "# X\nbody\n")];
        let out = render_relevant_context(&chunks, &[0], Path::new("/repo"));
        assert!(out.contains("path=\"/elsewhere/A.md\""), "got: {out}");
    }

    #[test]
    fn render_adds_trailing_newline_to_bodies_missing_one() {
        let chunks = vec![
            chunk("/repo/A.md", 1, "X", "# X\nno trailing newline"),
            chunk("/repo/A.md", 2, "Y", "## Y\nalso none"),
        ];
        let out = render_relevant_context(&chunks, &[0, 1], Path::new("/repo"));
        // Each body should be followed by exactly one \n before the next
        // line / closing tag — no missing newline runs the heading of the
        // next chunk onto the previous body line.
        assert!(out.contains("no trailing newline\n## Y"), "got: {out}");
    }
}
