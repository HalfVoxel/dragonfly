//! Duplicate-function hints for the current PR.
//!
//! Ports the lovable funcsim pipeline: extract every Go function in the repo,
//! summarize each one's behavior in one line with an LLM (`kit llm`), embed
//! the summaries with Vertex `text-embedding-005`, and report existing
//! functions whose summary embedding is cosine-similar to a function this PR
//! added or changed. Summaries and embeddings are cached under
//! `~/.dragonfly/dedup/` keyed by content hash, so only new or edited
//! functions ever hit the LLM. Confirmed false positives are recorded as
//! pairwise exclusions (per origin URL, shared across worktrees) and never
//! reported again.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::home_dir;

// 0.90 finds only near-exact behavior matches; 0.80 also surfaces the
// "same family, maybe foldable" tier. Erring low is fine: the agent
// verifies every hint and dismissals persist.
pub const DEFAULT_THRESHOLD: f64 = 0.80;
pub const DEFAULT_LIMIT: usize = 5;

// Pipeline knobs, carried over from funcsim's tuned defaults.
const MIN_CHARS: usize = 120;
const MAX_CHARS: usize = 6000;
const SUMMARY_MODEL: &str = "vertex/gemini-3.1-flash-lite-preview";
const SUMMARY_BATCH: usize = 20;
const SUMMARY_CONCURRENCY: usize = 8;
const SUMMARY_MAX_FUNC_CHARS: usize = 3000;
const EMBED_MODEL: &str = "text-embedding-005";
const EMBED_DIM: usize = 768;
const EMBED_LOCATION: &str = "europe-west4";
const EMBED_TASK: &str = "CLUSTERING";
const DEFAULT_GCP_PROJECT: &str = "gpt-engineer-dev";
const EMBED_BATCH_INSTANCES: usize = 128;
const EMBED_BATCH_TOKENS: usize = 15000;
const EMBED_CONCURRENCY: usize = 6;
// Neutralizes the "which object" signal so same-receiver methods don't
// cluster on the receiver type alone.
const GENERIC_RECV: &str = "(_ R)";
// Cap on changed functions rendered into <potential-duplicates>.
const BLOCK_MAX_FUNCS: usize = 20;

const SUMMARY_SYSTEM_PROMPT: &str = "You summarize what each Go function does in one terse line \
    describing its specific operation and effect. Ignore the receiver, the function name, \
    error-wrapping, logging, and generic scaffolding; focus on the distinctive logic that \
    sets it apart. Output exactly one line per function in the form '<id>: <summary>', and nothing else.";

// ── Extraction ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct GoFn {
    /// Repo-relative path.
    path: String,
    /// 1-based line of the `func` keyword.
    line: usize,
    /// Package clause name (identity fallback for repo-root files).
    pkg: String,
    /// Receiver type for methods (`*Server`), empty for plain functions.
    recv: String,
    name: String,
    signature: String,
    /// sha256 of the embed text (doc + signature + body, receiver
    /// genericized, truncated). The summary-cache key.
    source_hash: String,
    embed_text: String,
    /// One-line LLM behavior summary; the text that actually gets embedded.
    summary: String,
    /// sha256 of `summary`. The embedding-cache key.
    embed_key: String,
}

impl GoFn {
    /// Stable identity used in reports and the exclusions file:
    /// `dir/path.(Recv).Name`. Survives line churn and body edits; renames
    /// and moves change it, which correctly re-surfaces old verdicts.
    pub fn identity(&self) -> String {
        let dir = Path::new(&self.path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let scope = if dir.is_empty() || dir == "." {
            self.pkg.clone()
        } else {
            dir
        };
        if self.recv.is_empty() {
            format!("{scope}.{}", self.name)
        } else {
            format!("{scope}.({}).{}", self.recv, self.name)
        }
    }
}

fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "vendor"
            | "testdata"
            | ".devenv"
            | ".direnv"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | ".agents"
            | ".funcsim-cache"
    )
}

fn is_generated_name(name: &str) -> bool {
    name.ends_with(".pb.go")
        || name.ends_with("_gen.go")
        || name.ends_with(".gen.go")
        || name.ends_with("_generated.go")
}

fn is_extractable_go_file(name: &str) -> bool {
    name.ends_with(".go") && !name.ends_with("_test.go") && !is_generated_name(name)
}

fn is_generated_header(src: &[u8]) -> bool {
    let head = &src[..src.len().min(2048)];
    for line in String::from_utf8_lossy(head).lines() {
        let line = line.trim();
        if line.starts_with("package ") {
            return false;
        }
        if line.starts_with("// Code generated ") && line.contains("DO NOT EDIT") {
            return true;
        }
    }
    false
}

fn collect_go_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if !skip_dir(&name) {
                    stack.push(path);
                }
            } else if ft.is_file() && is_extractable_go_file(&name) {
                out.push(path);
            }
        }
    }
    out
}

/// Extract every function in the repo. Parses files across threads; each
/// thread owns its own tree-sitter parser (parsers are not shareable).
pub fn extract_repo(root: &Path) -> Vec<GoFn> {
    let files = collect_go_files(root);
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len().max(1));
    let chunk = files.len().div_ceil(n_threads.max(1));
    let mut out = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk.max(1))
            .map(|files| {
                scope.spawn(move || {
                    let mut parser = new_parser();
                    let mut fns = Vec::new();
                    for path in files {
                        let Ok(src) = std::fs::read(path) else {
                            continue;
                        };
                        if is_generated_header(&src) {
                            continue;
                        }
                        let rel = path
                            .strip_prefix(root)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .to_string();
                        fns.extend(extract_source(&mut parser, &rel, &src));
                    }
                    fns
                })
            })
            .collect();
        for h in handles {
            out.extend(h.join().unwrap_or_default());
        }
    });
    out
}

fn new_parser() -> tree_sitter::Parser {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .expect("tree-sitter-go grammar incompatible with tree-sitter runtime");
    parser
}

/// Extract functions from one file's source. A parse failure yields no
/// functions rather than an error — one unparseable file must not abort a
/// whole-repo scan.
fn extract_source(parser: &mut tree_sitter::Parser, rel_path: &str, src: &[u8]) -> Vec<GoFn> {
    let Some(tree) = parser.parse(src, None) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    let children: Vec<tree_sitter::Node> = root.children(&mut cursor).collect();

    let pkg = children
        .iter()
        .find(|n| n.kind() == "package_clause")
        .and_then(|n| n.named_child(0))
        .map(|n| node_text(n, src))
        .unwrap_or_default();

    let mut out = Vec::new();
    for (i, node) in children.iter().enumerate() {
        if node.kind() != "function_declaration" && node.kind() != "method_declaration" {
            continue;
        }
        let Some(body) = node.child_by_field_name("body") else {
            continue; // assembly stubs and extern declarations have no body
        };
        let Some(name_node) = node.child_by_field_name("name") else {
            continue;
        };

        // Attach the contiguous doc-comment block: walk back over comment
        // siblings separated by exactly one newline (a blank line detaches).
        let mut start = node.start_byte();
        let mut j = i;
        while j > 0 {
            let prev = &children[j - 1];
            if prev.kind() != "comment" {
                break;
            }
            let gap = &src[prev.end_byte()..start];
            if gap.iter().filter(|&&b| b == b'\n').count() != 1
                || gap.iter().any(|&b| !b.is_ascii_whitespace())
            {
                break;
            }
            start = prev.start_byte();
            j -= 1;
        }

        let end = node.end_byte();
        let source = &src[start..end];
        if source.len() < MIN_CHARS {
            continue;
        }

        let recv_node = node.child_by_field_name("receiver");
        let recv = recv_node
            .and_then(|r| r.named_child(0))
            .and_then(|p| p.child_by_field_name("type"))
            .map(|t| node_text(t, src).split_whitespace().collect::<String>())
            .unwrap_or_default();

        let mut embed = String::from_utf8_lossy(source).to_string();
        if let Some(r) = recv_node {
            let ro = r.start_byte() - start;
            let rc = r.end_byte() - start;
            let head = String::from_utf8_lossy(&source[..ro]);
            let tail = String::from_utf8_lossy(&source[rc..]);
            embed = format!("{head}{GENERIC_RECV}{tail}");
        }
        if embed.len() > MAX_CHARS {
            let mut cut = MAX_CHARS;
            while !embed.is_char_boundary(cut) {
                cut -= 1;
            }
            embed.truncate(cut);
        }

        let signature = String::from_utf8_lossy(&src[node.start_byte()..body.start_byte()])
            .trim()
            .to_string();
        let source_hash = sha256_hex(embed.as_bytes());
        out.push(GoFn {
            path: rel_path.to_string(),
            line: node.start_position().row + 1,
            pkg: pkg.clone(),
            recv,
            name: node_text(name_node, src),
            signature,
            source_hash,
            embed_text: embed,
            summary: String::new(),
            embed_key: String::new(),
        });
    }
    out
}

fn node_text(node: tree_sitter::Node, src: &[u8]) -> String {
    String::from_utf8_lossy(&src[node.start_byte()..node.end_byte()]).to_string()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

// ── Caches ───────────────────────────────────────────────────────────────────

fn dedup_dir() -> PathBuf {
    home_dir().join(".dragonfly/dedup")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '/' | ':' | ' ') { '_' } else { c })
        .collect()
}

fn summaries_cache_path() -> PathBuf {
    dedup_dir().join(format!("summaries-{}.json", sanitize(SUMMARY_MODEL)))
}

fn embeddings_cache_path() -> PathBuf {
    dedup_dir().join(format!("embeddings-{EMBED_MODEL}-{EMBED_DIM}.bin"))
}

fn load_summaries() -> HashMap<String, String> {
    std::fs::read(summaries_cache_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

fn save_summaries(cache: &HashMap<String, String>) {
    if let Ok(data) = serde_json::to_vec(cache)
        && let Err(e) = save_atomic(&summaries_cache_path(), &data)
    {
        eprintln!("   Warning: failed to write summary cache: {e}");
    }
}

// Embeddings are too many floats for JSON. Flat records after an 8-byte
// magic: 64-byte hex hash, u32 LE dim, dim × f32 LE. A truncated tail
// (killed mid-write of the pre-rename tmp never reaches here, but be safe)
// is ignored.
const EMB_MAGIC: &[u8; 8] = b"DFEMB01\n";

fn load_embeddings() -> HashMap<String, Vec<f32>> {
    let mut out = HashMap::new();
    let Ok(data) = std::fs::read(embeddings_cache_path()) else {
        return out;
    };
    if data.len() < 8 || &data[..8] != EMB_MAGIC {
        return out;
    }
    let mut pos = 8;
    while pos + 68 <= data.len() {
        let hash = match std::str::from_utf8(&data[pos..pos + 64]) {
            Ok(h) => h.to_string(),
            Err(_) => return out,
        };
        let dim = u32::from_le_bytes(data[pos + 64..pos + 68].try_into().unwrap()) as usize;
        pos += 68;
        if pos + dim * 4 > data.len() {
            break;
        }
        let vec: Vec<f32> = data[pos..pos + dim * 4]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        pos += dim * 4;
        out.insert(hash, vec);
    }
    out
}

fn save_embeddings(cache: &HashMap<String, Vec<f32>>) {
    let mut data = Vec::with_capacity(8 + cache.len() * (68 + EMBED_DIM * 4));
    data.extend_from_slice(EMB_MAGIC);
    for (hash, vec) in cache {
        if hash.len() != 64 {
            continue;
        }
        data.extend_from_slice(hash.as_bytes());
        data.extend_from_slice(&(vec.len() as u32).to_le_bytes());
        for f in vec {
            data.extend_from_slice(&f.to_le_bytes());
        }
    }
    if let Err(e) = save_atomic(&embeddings_cache_path(), &data) {
        eprintln!("   Warning: failed to write embedding cache: {e}");
    }
}

// ── Summaries (kit llm) ──────────────────────────────────────────────────────

#[derive(Clone)]
struct Kit {
    bin: String,
    /// kit loads inference credentials relative to its cwd (`go/api/.env.dev`),
    /// so it must run from its own repo, not from wherever dragonfly runs.
    cwd: Option<PathBuf>,
}

/// Resolve the kit binary: PATH first, then the repo-local `bin/kit`
/// wrapper (lovable ships kit inside the repo, not globally installed).
async fn resolve_kit(toplevel: &str) -> Result<Kit, String> {
    if let Some(p) = crate::sh("command -v kit").await.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(&p);
        let cwd = path
            .parent()
            .filter(|d| d.file_name().is_some_and(|n| n == "bin"))
            .and_then(|d| d.parent())
            .map(|d| d.to_path_buf());
        return Ok(Kit { bin: p, cwd });
    }
    let local = Path::new(toplevel).join("bin/kit");
    if local.is_file() {
        return Ok(Kit {
            bin: local.to_string_lossy().to_string(),
            cwd: Some(PathBuf::from(toplevel)),
        });
    }
    Err("kit not found (needed for behavior summaries): not on PATH and no bin/kit in repo".into())
}

/// Fill each function's `summary` and `embed_key`, generating summaries for
/// cache misses via `kit llm`. A function that never gets a summary falls
/// back to its signature — still better than dropping it from the index.
async fn summarize_all(fns: &mut [GoFn], kit: &Kit, quiet: bool) -> Result<(), String> {
    let mut code: HashMap<String, String> = HashMap::new();
    for f in fns.iter() {
        code.entry(f.source_hash.clone())
            .or_insert_with(|| f.embed_text.clone());
    }
    let cache = Arc::new(Mutex::new(load_summaries()));
    let mut missing: Vec<String> = {
        let c = cache.lock().unwrap();
        code.keys().filter(|h| !c.contains_key(*h)).cloned().collect()
    };
    missing.sort();
    if !quiet {
        eprintln!(
            "   Dedup summaries: {} distinct, {} cached, {} to generate.",
            code.len(),
            code.len() - missing.len(),
            missing.len()
        );
    }

    if !missing.is_empty() {
        let code = Arc::new(code);
        let sem = Arc::new(Semaphore::new(SUMMARY_CONCURRENCY));
        let done = Arc::new(Mutex::new(0usize));
        let total = missing.len();
        let mut tasks = tokio::task::JoinSet::new();
        for batch in missing.chunks(SUMMARY_BATCH) {
            let batch = batch.to_vec();
            let code = code.clone();
            let cache = cache.clone();
            let sem = sem.clone();
            let done = done.clone();
            let kit = kit.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let got = summarize_batch(&batch, &code, &kit).await?;
                let n = got.len();
                let mut c = cache.lock().unwrap();
                c.extend(got);
                let mut d = done.lock().unwrap();
                *d += n;
                // Checkpoint so a killed 15-minute cold build resumes.
                if *d % 500 < SUMMARY_BATCH {
                    save_summaries(&c);
                    if !quiet {
                        eprintln!("   ...{}/{} summarized", *d, total);
                    }
                }
                Ok::<(), String>(())
            });
        }
        let mut first_err = None;
        while let Some(r) = tasks.join_next().await {
            if let Ok(Err(e)) = r
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        save_summaries(&cache.lock().unwrap());
        if let Some(e) = first_err {
            return Err(e);
        }
    }

    let c = cache.lock().unwrap();
    for f in fns.iter_mut() {
        let s = c
            .get(&f.source_hash)
            .map(String::as_str)
            .unwrap_or_default()
            .trim();
        f.summary = if s.is_empty() {
            f.signature.clone()
        } else {
            s.to_string()
        };
        f.embed_key = sha256_hex(f.summary.as_bytes());
    }
    Ok(())
}

/// Summarize one batch, retrying transient failures and splitting to
/// one-function calls when the model returns fewer lines than asked — a
/// single bad line must not poison the whole batch.
async fn summarize_batch(
    hashes: &[String],
    code: &HashMap<String, String>,
    kit: &Kit,
) -> Result<HashMap<String, String>, String> {
    let line_re = regex::Regex::new(r"(?m)^\s*(\d+)\s*[:.)]\s*(\S.*?)\s*$").unwrap();
    let mut out = HashMap::new();
    let mut last_err = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(attempt)).await;
        }
        let raw = match call_kit_llm(hashes, code, kit).await {
            Ok(r) => r,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        let mut by_id: HashMap<usize, String> = HashMap::new();
        for m in line_re.captures_iter(&raw) {
            if let Ok(id) = m[1].parse::<usize>() {
                by_id.insert(id, m[2].to_string());
            }
        }
        let mut complete = true;
        for (i, h) in hashes.iter().enumerate() {
            match by_id.get(&i) {
                Some(s) => {
                    out.insert(h.clone(), s.clone());
                }
                None => complete = false,
            }
        }
        if complete {
            return Ok(out);
        }
        last_err = format!("parsed {}/{} summaries", out.len(), hashes.len());
    }
    if hashes.len() > 1 {
        for h in hashes {
            if out.contains_key(h) {
                continue;
            }
            if let Ok(single) =
                Box::pin(summarize_batch(std::slice::from_ref(h), code, kit)).await
            {
                out.extend(single);
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    Err(format!("kit llm summaries failed: {last_err}"))
}

async fn call_kit_llm(
    hashes: &[String],
    code: &HashMap<String, String>,
    kit: &Kit,
) -> Result<String, String> {
    let mut prompt = String::new();
    for (i, h) in hashes.iter().enumerate() {
        let mut src = code.get(h).map(String::as_str).unwrap_or_default();
        if src.len() > SUMMARY_MAX_FUNC_CHARS {
            let mut cut = SUMMARY_MAX_FUNC_CHARS;
            while !src.is_char_boundary(cut) {
                cut -= 1;
            }
            src = &src[..cut];
        }
        prompt.push_str(&format!("### {i}\n{src}\n"));
    }
    let mut cmd = Command::new(&kit.bin);
    cmd.args(["llm", "-m", SUMMARY_MODEL, "--system", SUMMARY_SYSTEM_PROMPT]);
    if let Some(cwd) = &kit.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("kit not runnable: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(prompt.as_bytes())
        .await
        .map_err(|e| format!("kit stdin: {e}"))?;
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("kit llm: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "kit llm exited {}: {}",
            out.status.code().unwrap_or(1),
            &stderr[..stderr.len().min(200)]
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ── Embeddings (Vertex via curl) ─────────────────────────────────────────────

struct VertexClient {
    url: String,
    /// Bearer header stored in a 0600 file passed as `curl -H @file`, so the
    /// token never shows up in `ps` output.
    header_file: PathBuf,
}

impl Drop for VertexClient {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.header_file);
    }
}

impl VertexClient {
    async fn new() -> Result<Self, String> {
        let token = crate::sh("gcloud auth application-default print-access-token")
            .await
            .ok_or("no gcloud ADC (run `gcloud auth application-default login`)")?;
        let project = std::env::var("GOOGLE_CLOUD_PROJECT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_GCP_PROJECT.to_string());
        let dir = dedup_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let header_file = dir.join(format!(".authhdr.{}", std::process::id()));
        std::fs::write(&header_file, format!("Authorization: Bearer {token}\n"))
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&header_file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Self {
            url: format!(
                "https://{EMBED_LOCATION}-aiplatform.googleapis.com/v1/projects/{project}/locations/{EMBED_LOCATION}/publishers/google/models/{EMBED_MODEL}:predict"
            ),
            header_file,
        })
    }

    async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let instances: Vec<serde_json::Value> = inputs
            .iter()
            .map(|s| serde_json::json!({"content": s, "task_type": EMBED_TASK}))
            .collect();
        let body = serde_json::json!({
            "instances": instances,
            "parameters": {"outputDimensionality": EMBED_DIM, "autoTruncate": true},
        })
        .to_string();

        let mut last_err = String::new();
        for attempt in 0..6u32 {
            if attempt > 0 {
                let backoff = std::time::Duration::from_millis(500 * (1 << (attempt - 1)));
                tokio::time::sleep(backoff).await;
            }
            let mut child = Command::new("curl")
                .args([
                    "-sS",
                    "-X",
                    "POST",
                    "-H",
                    "Content-Type: application/json",
                    "-H",
                    &format!("@{}", self.header_file.display()),
                    "--data-binary",
                    "@-",
                    "-w",
                    "\n%{http_code}",
                    &self.url,
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("curl not runnable: {e}"))?;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(body.as_bytes())
                .await
                .map_err(|e| format!("curl stdin: {e}"))?;
            let out = child
                .wait_with_output()
                .await
                .map_err(|e| format!("curl: {e}"))?;
            if !out.status.success() {
                last_err = format!("curl: {}", String::from_utf8_lossy(&out.stderr));
                continue;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let (payload, status) = stdout.rsplit_once('\n').unwrap_or((&stdout, ""));
            let status: u16 = status.trim().parse().unwrap_or(0);
            if status == 429 || status >= 500 || status == 0 {
                last_err = format!("vertex {status}: {}", &payload[..payload.len().min(300)]);
                continue;
            }
            if status != 200 {
                return Err(format!(
                    "vertex {status}: {}",
                    &payload[..payload.len().min(600)]
                ));
            }
            #[derive(Deserialize)]
            struct Pred {
                embeddings: Emb,
            }
            #[derive(Deserialize)]
            struct Emb {
                values: Vec<f32>,
            }
            #[derive(Deserialize)]
            struct Resp {
                predictions: Vec<Pred>,
            }
            let resp: Resp = serde_json::from_str(payload)
                .map_err(|e| format!("decode vertex response: {e}"))?;
            if resp.predictions.len() != inputs.len() {
                return Err(format!(
                    "vertex returned {} predictions for {} inputs",
                    resp.predictions.len(),
                    inputs.len()
                ));
            }
            return Ok(resp
                .predictions
                .into_iter()
                .map(|p| normalize(p.embeddings.values))
                .collect());
        }
        Err(format!("vertex request failed after 6 attempts: {last_err}"))
    }
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// Vertex's ~2.4 chars/token for code, over-estimated slightly so
/// token-budgeted batches stay under the hard 20k per-request cap.
fn est_tokens(s: &str) -> usize {
    s.len() * 2 / 5 + 2
}

/// Return an `embed_key -> unit vector` map covering every distinct key in
/// `fns`, fetching cache misses from Vertex.
async fn embed_all(fns: &[GoFn], quiet: bool) -> Result<HashMap<String, Vec<f32>>, String> {
    let mut texts: HashMap<String, String> = HashMap::new();
    for f in fns {
        texts
            .entry(f.embed_key.clone())
            .or_insert_with(|| f.summary.clone());
    }
    let mut cache = load_embeddings();
    let mut missing: Vec<String> = texts.keys().filter(|h| !cache.contains_key(*h)).cloned().collect();
    missing.sort();
    if !quiet {
        eprintln!(
            "   Dedup embeddings: {} distinct, {} cached, {} to fetch.",
            texts.len(),
            texts.len() - missing.len(),
            missing.len()
        );
    }

    if !missing.is_empty() {
        let client = Arc::new(VertexClient::new().await?);

        // Batch by instance count and estimated tokens, like funcsim.
        let mut batches: Vec<Vec<String>> = Vec::new();
        let mut cur: Vec<String> = Vec::new();
        let mut cur_tok = 0usize;
        for h in missing {
            let t = est_tokens(&texts[&h]);
            if !cur.is_empty() && (cur.len() >= EMBED_BATCH_INSTANCES || cur_tok + t > EMBED_BATCH_TOKENS)
            {
                batches.push(std::mem::take(&mut cur));
                cur_tok = 0;
            }
            cur.push(h);
            cur_tok += t;
        }
        if !cur.is_empty() {
            batches.push(cur);
        }

        let texts = Arc::new(texts.clone());
        let sem = Arc::new(Semaphore::new(EMBED_CONCURRENCY));
        let fetched = Arc::new(Mutex::new(HashMap::<String, Vec<f32>>::new()));
        let mut tasks = tokio::task::JoinSet::new();
        for batch in batches {
            let client = client.clone();
            let texts = texts.clone();
            let sem = sem.clone();
            let fetched = fetched.clone();
            tasks.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let inputs: Vec<&str> = batch.iter().map(|h| texts[h].as_str()).collect();
                let vecs = client.embed(&inputs).await?;
                let mut f = fetched.lock().unwrap();
                for (h, v) in batch.into_iter().zip(vecs) {
                    f.insert(h, v);
                }
                Ok::<(), String>(())
            });
        }
        let mut first_err = None;
        while let Some(r) = tasks.join_next().await {
            if let Ok(Err(e)) = r
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        // Persist whatever arrived, even on partial failure — reruns resume.
        let fetched = Arc::try_unwrap(fetched)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_default();
        if !fetched.is_empty() {
            cache.extend(fetched);
            save_embeddings(&cache);
        }
        if let Some(e) = first_err {
            return Err(e);
        }
    }
    Ok(cache)
}

// ── Exclusions ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct ExclusionPair {
    pub a: String,
    pub b: String,
    pub noted_at: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Exclusions {
    pairs: Vec<ExclusionPair>,
}

/// Key exclusions by normalized origin URL so every worktree of the same
/// repo shares one file. Falls back to the toplevel path for remoteless
/// repos.
async fn repo_key() -> String {
    let raw = match crate::sh("git remote get-url origin").await {
        Some(url) if !url.is_empty() => url,
        _ => crate::sh("git rev-parse --show-toplevel")
            .await
            .unwrap_or_else(|| "unknown".into()),
    };
    let mut s = raw.to_lowercase();
    for prefix in ["https://", "http://", "ssh://", "git@"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
        }
    }
    let s = s.strip_suffix(".git").unwrap_or(&s);
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

async fn exclusions_path() -> PathBuf {
    dedup_dir().join("repos").join(repo_key().await).join("exclusions.json")
}

async fn load_exclusions() -> Exclusions {
    std::fs::read(exclusions_path().await)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

async fn save_exclusions(e: &Exclusions) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(e).map_err(|e| e.to_string())?;
    save_atomic(&exclusions_path().await, &data).map_err(|e| e.to_string())
}

fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

// ── Candidate computation ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DupMatch {
    pub function: String,
    pub path: String,
    pub line: usize,
    pub cosine: f64,
    pub summary: String,
}

#[derive(Serialize)]
pub struct FnCandidates {
    pub function: String,
    pub path: String,
    pub line: usize,
    pub summary: String,
    pub matches: Vec<DupMatch>,
}

pub struct DedupReport {
    pub base_ref: String,
    pub threshold: f64,
    pub changed_total: usize,
    pub candidates: Vec<FnCandidates>,
}

async fn git_show(toplevel: &str, base_ref: &str, path: &str) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .args(["-C", toplevel, "show", &format!("{base_ref}:{path}")])
        .output()
        .await
        .ok()?;
    if out.status.success() { Some(out.stdout) } else { None }
}

/// Run the full pipeline: extract the repo, diff the changed set against
/// `base_ref`, summarize + embed (cache misses only), and neighbor-query
/// each changed function against the whole index.
pub async fn compute_report(
    base_ref: Option<String>,
    threshold: f64,
    limit: usize,
    quiet: bool,
) -> Result<DedupReport, String> {
    let toplevel = crate::sh("git rev-parse --show-toplevel")
        .await
        .ok_or("not inside a git repository")?;
    let base_ref = match base_ref {
        Some(b) => b,
        None => crate::pr_base_ref().await,
    };

    let changed_files: Vec<String> = crate::get_changed_files(&base_ref)
        .await
        .into_iter()
        .filter(|p| {
            Path::new(p)
                .file_name()
                .map(|n| is_extractable_go_file(&n.to_string_lossy()))
                .unwrap_or(false)
        })
        .collect();
    if changed_files.is_empty() {
        return Ok(DedupReport {
            base_ref,
            threshold,
            changed_total: 0,
            candidates: Vec::new(),
        });
    }

    let root = PathBuf::from(&toplevel);
    let mut fns = tokio::task::spawn_blocking(move || extract_repo(&root))
        .await
        .map_err(|e| e.to_string())?;
    if fns.is_empty() {
        return Ok(DedupReport {
            base_ref,
            threshold,
            changed_total: 0,
            candidates: Vec::new(),
        });
    }

    // A function is changed when its identity is new vs base or its embed
    // text hash differs. Only base versions of changed files are needed.
    let mut base_hashes: HashMap<String, String> = HashMap::new();
    {
        let mut parser = new_parser();
        for path in &changed_files {
            let Some(src) = git_show(&toplevel, &base_ref, path).await else {
                continue; // added file: every function in it is changed
            };
            if is_generated_header(&src) {
                continue;
            }
            for f in extract_source(&mut parser, path, &src) {
                base_hashes.insert(f.identity(), f.source_hash);
            }
        }
    }
    let changed_paths: HashSet<&str> = changed_files.iter().map(String::as_str).collect();
    let changed_idx: Vec<usize> = fns
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            changed_paths.contains(f.path.as_str())
                && base_hashes.get(&f.identity()) != Some(&f.source_hash)
        })
        .map(|(i, _)| i)
        .collect();
    let changed_total = changed_idx.len();
    if changed_idx.is_empty() {
        return Ok(DedupReport {
            base_ref,
            threshold,
            changed_total,
            candidates: Vec::new(),
        });
    }
    if !quiet {
        eprintln!(
            "   Dedup: {} functions in repo, {} changed vs {base_ref}.",
            fns.len(),
            changed_total
        );
    }

    let kit = resolve_kit(&toplevel).await?;
    summarize_all(&mut fns, &kit, quiet).await?;
    let vecs = embed_all(&fns, quiet).await?;

    let excluded: HashSet<(String, String)> = load_exclusions()
        .await
        .pairs
        .iter()
        .map(|p| pair_key(&p.a, &p.b))
        .collect();

    let mut candidates = Vec::new();
    for &ci in &changed_idx {
        let cf = &fns[ci];
        let Some(cv) = vecs.get(&cf.embed_key) else {
            continue;
        };
        let cid = cf.identity();
        let mut matches: Vec<DupMatch> = Vec::new();
        for (i, f) in fns.iter().enumerate() {
            if i == ci {
                continue;
            }
            let fid = f.identity();
            // Same identity elsewhere = build-tag variants of one function,
            // not duplication.
            if fid == cid || excluded.contains(&pair_key(&cid, &fid)) {
                continue;
            }
            let Some(v) = vecs.get(&f.embed_key) else {
                continue;
            };
            let cos = dot(cv, v) as f64;
            if cos >= threshold {
                matches.push(DupMatch {
                    function: fid,
                    path: f.path.clone(),
                    line: f.line,
                    cosine: cos,
                    summary: f.summary.clone(),
                });
            }
        }
        matches.sort_by(|a, b| b.cosine.total_cmp(&a.cosine));
        // A changed function can match several near-identical copies of the
        // same helper; keep one entry per identity.
        let mut seen = HashSet::new();
        matches.retain(|m| seen.insert(m.function.clone()));
        matches.truncate(limit);
        if !matches.is_empty() {
            candidates.push(FnCandidates {
                function: cid,
                path: cf.path.clone(),
                line: cf.line,
                summary: cf.summary.clone(),
                matches,
            });
        }
    }
    candidates.sort_by(|a, b| {
        b.matches[0]
            .cosine
            .total_cmp(&a.matches[0].cosine)
    });
    Ok(DedupReport {
        base_ref,
        threshold,
        changed_total,
        candidates,
    })
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ── Prompt block ─────────────────────────────────────────────────────────────

/// Render the `<potential-duplicates>` block for agent prompts, or None when
/// there is nothing to show or the pipeline can't run (no ADC, no kit) —
/// dedup hints are best-effort and must never block a dragonfly run.
pub async fn build_block(base_ref: &str) -> Option<String> {
    match compute_report(Some(base_ref.to_string()), DEFAULT_THRESHOLD, DEFAULT_LIMIT, false).await
    {
        Ok(report) if !report.candidates.is_empty() => Some(render_block(&report)),
        Ok(_) => None,
        Err(e) => {
            eprintln!("   Skipping <potential-duplicates> ({e}).");
            None
        }
    }
}

fn render_block(report: &DedupReport) -> String {
    let mut out = String::from("<potential-duplicates>\n");
    out.push_str(
        "Changed functions that may duplicate existing ones (hints, not verdicts \u{2014} \
         read both first):\n\n",
    );
    for c in report.candidates.iter().take(BLOCK_MAX_FUNCS) {
        out.push_str(&format!(
            "- `{}` ({}:{}) \u{2014} {}\n",
            c.function, c.path, c.line, c.summary
        ));
        for m in &c.matches {
            out.push_str(&format!(
                "  - {:.2} `{}` ({}:{}) \u{2014} {}\n",
                m.cosine, m.function, m.path, m.line, m.summary
            ));
        }
    }
    if report.candidates.len() > BLOCK_MAX_FUNCS {
        out.push_str(&format!(
            "\n({} more \u{2014} run `dragonfly dedup` for the full list.)\n",
            report.candidates.len() - BLOCK_MAX_FUNCS
        ));
    }
    out.push_str(
        "\nReal duplication \u{2192} mention in the final summary (don't refactor unasked). \
         False positive \u{2192} `dragonfly dedup dismiss '<changed-func>' ['<match>'...]`.\n\
         </potential-duplicates>\n",
    );
    out
}

// ── CLI commands ─────────────────────────────────────────────────────────────

pub async fn cmd_list(
    base: Option<String>,
    threshold: f64,
    limit: usize,
    json: bool,
) -> i32 {
    let report = match compute_report(base, threshold, limit, json).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dedup: {e}");
            return 1;
        }
    };
    if json {
        let v = serde_json::json!({
            "base": report.base_ref,
            "threshold": report.threshold,
            "changed_functions": report.changed_total,
            "candidates": report.candidates,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return 0;
    }
    if report.changed_total == 0 {
        println!("No changed Go functions vs {}.", report.base_ref);
        return 0;
    }
    if report.candidates.is_empty() {
        println!(
            "No dedup candidates: {} changed function{} vs {}, none similar to existing code (\u{2265} {:.2}).",
            report.changed_total,
            if report.changed_total == 1 { "" } else { "s" },
            report.base_ref,
            report.threshold
        );
        return 0;
    }
    println!(
        "Dedup candidates ({} of {} changed functions, vs {}, threshold {:.2}):\n",
        report.candidates.len(),
        report.changed_total,
        report.base_ref,
        report.threshold
    );
    for c in &report.candidates {
        println!("{}  ({}:{})", c.function, c.path, c.line);
        println!("    {}", c.summary);
        for m in &c.matches {
            println!("  {:.2}  {}  ({}:{})", m.cosine, m.function, m.path, m.line);
            println!("        {}", m.summary);
        }
        println!();
    }
    println!(
        "Dismiss false positives: dragonfly dedup dismiss '<changed-func>' ['<match>'...]"
    );
    0
}

/// Resolve a user-supplied function reference against a set of identities:
/// exact match first, then unique bare-name / suffix match.
fn resolve_ref<'a>(arg: &str, identities: &[&'a str]) -> Result<&'a str, String> {
    if let Some(id) = identities.iter().find(|id| **id == arg) {
        return Ok(id);
    }
    let matches: Vec<&str> = identities
        .iter()
        .filter(|id| {
            id.ends_with(&format!(".{arg}")) || id.ends_with(&format!("){arg}")) || **id == arg
        })
        .copied()
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!(
            "no function matching '{arg}' among: {}",
            identities.join(", ")
        )),
        _ => Err(format!(
            "'{arg}' is ambiguous; candidates: {}",
            matches.join(", ")
        )),
    }
}

pub async fn cmd_dismiss(
    func: String,
    match_refs: Vec<String>,
    base: Option<String>,
    threshold: f64,
    limit: usize,
) -> i32 {
    // Recompute the current candidate set (warm caches make this cheap) so
    // "dismiss X" can mean "X is not a dup of anything currently listed".
    let report = match compute_report(base, threshold, limit, true).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dedup dismiss: {e}");
            return 1;
        }
    };
    let identities: Vec<&str> = report.candidates.iter().map(|c| c.function.as_str()).collect();
    if identities.is_empty() {
        eprintln!("dedup dismiss: no changed functions currently have candidates.");
        return 1;
    }
    let target = match resolve_ref(&func, &identities) {
        Ok(t) => t.to_string(),
        Err(e) => {
            eprintln!("dedup dismiss: {e}");
            return 1;
        }
    };
    let cand = report
        .candidates
        .iter()
        .find(|c| c.function == target)
        .unwrap();
    let match_ids: Vec<&str> = cand.matches.iter().map(|m| m.function.as_str()).collect();
    let dismissed: Vec<String> = if match_refs.is_empty() {
        match_ids.iter().map(|s| s.to_string()).collect()
    } else {
        let mut out = Vec::new();
        for r in &match_refs {
            match resolve_ref(r, &match_ids) {
                Ok(m) => out.push(m.to_string()),
                Err(e) => {
                    eprintln!("dedup dismiss: {e}");
                    return 1;
                }
            }
        }
        out
    };

    let mut exclusions = load_exclusions().await;
    let existing: HashSet<(String, String)> = exclusions
        .pairs
        .iter()
        .map(|p| pair_key(&p.a, &p.b))
        .collect();
    let now = chrono::Local::now().to_rfc3339();
    let mut added = 0;
    for m in &dismissed {
        let (a, b) = pair_key(&target, m);
        if existing.contains(&(a.clone(), b.clone())) {
            continue;
        }
        exclusions.pairs.push(ExclusionPair {
            a,
            b,
            noted_at: now.clone(),
        });
        added += 1;
    }
    if let Err(e) = save_exclusions(&exclusions).await {
        eprintln!("dedup dismiss: failed to save exclusions: {e}");
        return 1;
    }
    println!(
        "Dismissed {added} pair{} for {target}{}:",
        if added == 1 { "" } else { "s" },
        if added < dismissed.len() {
            format!(" ({} already recorded)", dismissed.len() - added)
        } else {
            String::new()
        }
    );
    for m in &dismissed {
        println!("  not a duplicate of {m}");
    }
    0
}

pub async fn cmd_exclusions(json: bool) -> i32 {
    let exclusions = load_exclusions().await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"pairs": exclusions.pairs})).unwrap()
        );
        return 0;
    }
    if exclusions.pairs.is_empty() {
        println!(
            "No dedup exclusions recorded for this repo ({}).",
            exclusions_path().await.display()
        );
        return 0;
    }
    println!(
        "Dedup exclusions ({}):",
        exclusions_path().await.display()
    );
    for p in &exclusions.pairs {
        println!("  {}  \u{2260}  {}  ({})", p.a, p.b, p.noted_at);
    }
    0
}
