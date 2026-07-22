//! Team-shared dedup cache via GCS delta packs.
//!
//! GCS holds the union of everyone's caches as immutable "packs". A run that
//! generated new entries uploads just those as one pack named by its own
//! content hash, so concurrent writers can never conflict — two racing
//! uploads simply become two packs. Readers list the prefix, download only
//! packs they have not merged yet (tracked in
//! `~/.dragonfly/dedup/synced-packs.json`), and union them into the local
//! caches. A fresh machine therefore downloads the whole index instead of
//! re-summarizing and re-embedding 50k functions. When the pack count passes
//! [COMPACT_THRESHOLD], the next uploader folds every pack it has already
//! merged into one and deletes the originals.
//!
//! Guarantee: no source code ever leaves the machine. A pack carries the
//! one-line LLM behavior summaries (keyed by source hash), their embedding
//! vectors, and dismissed not-a-duplicate pairs (function identities, keyed
//! by repo) — the worst-case exposure of a leaked pack is terse
//! natural-language descriptions of function behavior plus function names,
//! never code.
//!
//! All sync is best-effort: any failure is a warning and the dedup pipeline
//! continues on the local caches alone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::{
    ExclusionPair, append_exclusions_in, dedup_dir, load_embeddings, load_exclusions_in,
    load_summaries, pair_key, save_atomic, save_embeddings, save_summaries, sha256_hex,
};
use crate::status::status_line;

type ExclMap = HashMap<String, Vec<ExclusionPair>>;

/// `gs://bucket/prefix` for the shared cache. Override with
/// `DRAGONFLY_DEDUP_GCS` (`off` disables sync entirely).
const DEFAULT_REMOTE: &str = "gs://lovable-dragonfly-dedup/dedup/v1";
const REMOTE_ENV: &str = "DRAGONFLY_DEDUP_GCS";

// ~20-100 packs land per day across the team, so 512 compacts roughly
// weekly. Compaction forces every other machine to re-download the folded
// base pack (~250 MB), which is why the threshold errs high.
const COMPACT_THRESHOLD: usize = 512;
const DOWNLOAD_CONCURRENCY: usize = 8;

// ── Pack format ──────────────────────────────────────────────────────────────
//
// Pack layout (all integers LE):
//   8   magic "DFPACK3\n" (older magics carried other layouts and are
//       skipped by the magic check)
//   4   u32 summaries-JSON length
//   n   summaries JSON: {"<source_hash>": "<summary>", ...}
//   4   u32 exclusions-JSON length
//   m   exclusions JSON: {"<repo_key>": [{a, b, noted_at}, ...], ...}
//   *   embedding records to EOF: 64-byte hex embed_key, u32 dim, dim × f32
//
// Packs are immutable and merged by key union, so parse order is irrelevant.

const PACK_MAGIC: &[u8; 8] = b"DFPACK3\n";

struct Pack {
    summaries: HashMap<String, String>,
    exclusions: ExclMap,
    embeddings: HashMap<String, Vec<f32>>,
}

fn push_json_section(data: &mut Vec<u8>, json: Vec<u8>) {
    data.extend_from_slice(&(json.len() as u32).to_le_bytes());
    data.extend_from_slice(&json);
}

fn read_json_section<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    if *pos + 4 > data.len() {
        return None;
    }
    let len = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap()) as usize;
    *pos += 4;
    let section = data.get(*pos..*pos + len)?;
    *pos += len;
    Some(section)
}

fn build_pack(
    summaries: &HashMap<String, String>,
    exclusions: &ExclMap,
    embeddings: &HashMap<String, Vec<f32>>,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + embeddings.len() * (68 + 768 * 4));
    data.extend_from_slice(PACK_MAGIC);
    push_json_section(
        &mut data,
        serde_json::to_vec(summaries).unwrap_or_else(|_| b"{}".to_vec()),
    );
    push_json_section(
        &mut data,
        serde_json::to_vec(exclusions).unwrap_or_else(|_| b"{}".to_vec()),
    );
    for (hash, vec) in embeddings {
        if hash.len() != 64 {
            continue;
        }
        data.extend_from_slice(hash.as_bytes());
        data.extend_from_slice(&(vec.len() as u32).to_le_bytes());
        for f in vec {
            data.extend_from_slice(&f.to_le_bytes());
        }
    }
    data
}

/// Parse a pack, tolerating a truncated embedding tail the same way
/// [super::load_embeddings] does: whole records parsed so far are kept.
fn parse_pack(data: &[u8]) -> Option<Pack> {
    if data.len() < 8 || &data[..8] != PACK_MAGIC {
        return None;
    }
    let mut pos = 8;
    let summaries: HashMap<String, String> =
        serde_json::from_slice(read_json_section(data, &mut pos)?).ok()?;
    let exclusions: ExclMap = serde_json::from_slice(read_json_section(data, &mut pos)?).ok()?;
    let mut embeddings = HashMap::new();
    while pos + 68 <= data.len() {
        let Ok(hash) = std::str::from_utf8(&data[pos..pos + 64]) else {
            break;
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
        embeddings.insert(hash.to_string(), vec);
    }
    Some(Pack {
        summaries,
        exclusions,
        embeddings,
    })
}

// ── Synced-pack state ────────────────────────────────────────────────────────

fn synced_path() -> PathBuf {
    dedup_dir().join("synced-packs.json")
}

fn load_synced() -> HashSet<String> {
    std::fs::read(synced_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn save_synced(set: &HashSet<String>) {
    let mut names: Vec<&String> = set.iter().collect();
    names.sort();
    if let Ok(data) = serde_json::to_vec(&names) {
        let _ = save_atomic(&synced_path(), &data);
    }
}

// ── Exclusion sharing ────────────────────────────────────────────────────────
//
// Dismissals live per repo (repos/<repo_key>/exclusions.jsonl). Packs carry
// them keyed by repo_key so one shared cache serves every repo. A per-repo
// marker file records which pair keys are already remote; the delta a run
// uploads is everything the marker doesn't cover, and the marker advances on
// both push and pull.

fn repos_root() -> PathBuf {
    dedup_dir().join("repos")
}

/// Repo keys come from remote packs and become path components; only the
/// charset [super::repo_key] emits is accepted, and never dot-only names —
/// `..` would escape the repos directory.
fn valid_repo_key(k: &str) -> bool {
    k.chars().any(char::is_alphanumeric)
        && k.chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-')
}

/// `repo_key -> all locally recorded dismissals`.
fn all_local_exclusions() -> ExclMap {
    let mut out = ExclMap::new();
    let Ok(entries) = std::fs::read_dir(repos_root()) else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let pairs = load_exclusions_in(&e.path()).pairs;
        if !pairs.is_empty() {
            out.insert(name, pairs);
        }
    }
    out
}

fn remote_marker_path(repo: &str) -> PathBuf {
    repos_root().join(repo).join("exclusions-remote.json")
}

fn load_remote_marker(repo: &str) -> HashSet<(String, String)> {
    std::fs::read(remote_marker_path(repo))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<(String, String)>>(&b).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn save_remote_marker(repo: &str, set: &HashSet<(String, String)>) {
    let mut v: Vec<&(String, String)> = set.iter().collect();
    v.sort();
    if let Ok(data) = serde_json::to_vec(&v) {
        let _ = save_atomic(&remote_marker_path(repo), &data);
    }
}

/// Dismissals not yet known to be in the shared cache.
fn exclusion_delta() -> ExclMap {
    let mut out = ExclMap::new();
    for (repo, pairs) in all_local_exclusions() {
        let marker = load_remote_marker(&repo);
        let fresh: Vec<ExclusionPair> = pairs
            .into_iter()
            .filter(|p| !marker.contains(&pair_key(&p.a, &p.b)))
            .collect();
        if !fresh.is_empty() {
            out.insert(repo, fresh);
        }
    }
    out
}

fn mark_remote(excl: &ExclMap) {
    for (repo, pairs) in excl {
        let mut marker = load_remote_marker(repo);
        for p in pairs {
            marker.insert(pair_key(&p.a, &p.b));
        }
        save_remote_marker(repo, &marker);
    }
}

/// Union a pack's dismissals into the per-repo files; returns the number of
/// pairs that were new locally.
fn merge_exclusions(incoming: &ExclMap) -> usize {
    let mut added = 0;
    for (repo, pairs) in incoming {
        if !valid_repo_key(repo) {
            continue;
        }
        let dir = repos_root().join(repo);
        let known: HashSet<(String, String)> = load_exclusions_in(&dir)
            .pairs
            .iter()
            .map(|p| pair_key(&p.a, &p.b))
            .collect();
        let fresh: Vec<ExclusionPair> = pairs
            .iter()
            .filter(|p| !known.contains(&pair_key(&p.a, &p.b)))
            .cloned()
            .collect();
        if !fresh.is_empty() && append_exclusions_in(&dir, &fresh).is_ok() {
            added += fresh.len();
        }
        let mut marker = load_remote_marker(repo);
        for p in pairs {
            marker.insert(pair_key(&p.a, &p.b));
        }
        save_remote_marker(repo, &marker);
    }
    added
}

// ── GCS client (JSON API via curl, ADC token) ────────────────────────────────

/// Bearer header in a 0600 file passed as `curl -H @file`, so the token
/// never shows up in `ps` output (same pattern as the Vertex client).
/// Removed when the last [Gcs] clone drops.
struct TokenFile(PathBuf);

impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Clone)]
struct Gcs {
    bucket: String,
    /// Object-name prefix inside the bucket, without leading/trailing `/`.
    prefix: String,
    header_file: std::sync::Arc<TokenFile>,
}

/// RFC 3986 unreserved-only percent-encoding; safe for both object names in
/// the URL path (where `/` must become `%2F`) and query parameter values.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Gcs {
    /// None when sync is disabled via `DRAGONFLY_DEDUP_GCS=off`.
    async fn new() -> Result<Option<Self>, String> {
        let raw = std::env::var(REMOTE_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_REMOTE.to_string());
        if matches!(raw.as_str(), "off" | "0" | "none" | "disabled") {
            return Ok(None);
        }
        let rest = raw
            .strip_prefix("gs://")
            .ok_or_else(|| format!("{REMOTE_ENV} must be gs://bucket/prefix or `off`: {raw}"))?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        let token = crate::sh("gcloud auth application-default print-access-token")
            .await
            .ok_or("no gcloud ADC (run `gcloud auth application-default login`)")?;
        let dir = dedup_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let header_file = dir.join(format!(".authhdr-gcs.{}", std::process::id()));
        std::fs::write(&header_file, format!("Authorization: Bearer {token}\n"))
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&header_file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Some(Self {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            header_file: std::sync::Arc::new(TokenFile(header_file)),
        }))
    }

    fn object_name(&self, name: &str) -> String {
        if self.prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.prefix)
        }
    }

    /// One HTTP call. Body lands in `out` when given (binary-safe), else in
    /// the returned bytes. Retries transient failures (curl error, 429, 5xx).
    async fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&Path>,
        out: Option<&Path>,
    ) -> Result<(u16, Vec<u8>), String> {
        let mut last_err = String::new();
        for attempt in 0..4u32 {
            if attempt > 0 {
                let backoff = std::time::Duration::from_millis(500 * (1 << (attempt - 1)));
                tokio::time::sleep(backoff).await;
            }
            let mut cmd = Command::new("curl");
            cmd.args(["-sS", "--connect-timeout", "10", "-X", method]);
            cmd.args(["-H", &format!("@{}", self.header_file.0.display())]);
            if let Some(b) = body {
                cmd.args(["-H", "Content-Type: application/octet-stream"]);
                cmd.args(["--data-binary", &format!("@{}", b.display())]);
            }
            if let Some(o) = out {
                cmd.args(["-o", &o.to_string_lossy()]);
            }
            cmd.args(["-w", "\n%{http_code}", url]);
            let r = cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("curl not runnable: {e}"))?;
            if !r.status.success() {
                last_err = format!("curl: {}", String::from_utf8_lossy(&r.stderr));
                continue;
            }
            let stdout = r.stdout;
            let split = stdout
                .iter()
                .rposition(|&b| b == b'\n')
                .unwrap_or(stdout.len());
            let status: u16 = std::str::from_utf8(&stdout[split..])
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            if status == 429 || status >= 500 || status == 0 {
                last_err = format!("gcs {status}");
                continue;
            }
            return Ok((status, stdout[..split].to_vec()));
        }
        Err(format!("gcs request failed after 4 attempts: {last_err}"))
    }

    /// Full object names of every pack under `<prefix>/packs/`.
    async fn list_packs(&self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        let mut page_token = String::new();
        loop {
            let mut url = format!(
                "https://storage.googleapis.com/storage/v1/b/{}/o?prefix={}&fields=items(name),nextPageToken&maxResults=1000",
                self.bucket,
                enc(&self.object_name("packs/")),
            );
            if !page_token.is_empty() {
                url.push_str(&format!("&pageToken={}", enc(&page_token)));
            }
            let (status, body) = self.request("GET", &url, None, None).await?;
            if status != 200 {
                return Err(format!(
                    "list failed ({status}): {}",
                    String::from_utf8_lossy(&body[..body.len().min(300)])
                ));
            }
            let v: serde_json::Value =
                serde_json::from_slice(&body).map_err(|e| format!("decode list: {e}"))?;
            if let Some(items) = v["items"].as_array() {
                names.extend(
                    items
                        .iter()
                        .filter_map(|i| i["name"].as_str().map(String::from)),
                );
            }
            match v["nextPageToken"].as_str() {
                Some(t) => page_token = t.to_string(),
                None => return Ok(names),
            }
        }
    }

    async fn download(&self, object: &str, dest: &Path) -> Result<(), String> {
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media",
            self.bucket,
            enc(object)
        );
        let (status, _) = self.request("GET", &url, None, Some(dest)).await?;
        if status != 200 {
            return Err(format!("download {object} failed ({status})"));
        }
        Ok(())
    }

    async fn upload(&self, object: &str, data: &[u8]) -> Result<(), String> {
        let mut tmp = tempfile::NamedTempFile::new_in(dedup_dir())
            .map_err(|e| format!("temp file: {e}"))?;
        {
            use std::io::Write as _;
            tmp.write_all(data).map_err(|e| format!("temp file: {e}"))?;
        }
        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.bucket,
            enc(object)
        );
        let (status, body) = self.request("POST", &url, Some(tmp.path()), None).await?;
        if status != 200 {
            return Err(format!(
                "upload {object} failed ({status}): {}",
                String::from_utf8_lossy(&body[..body.len().min(300)])
            ));
        }
        Ok(())
    }

    async fn delete(&self, object: &str) -> Result<(), String> {
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            self.bucket,
            enc(object)
        );
        let (status, _) = self.request("DELETE", &url, None, None).await?;
        // 404 = a concurrent compactor deleted it first; the union survives
        // in that compactor's folded pack, so it is not an error here.
        if status != 204 && status != 200 && status != 404 {
            return Err(format!("delete {object} failed ({status})"));
        }
        Ok(())
    }
}

// ── Sync entry points ────────────────────────────────────────────────────────

/// Merge remote packs this machine has not seen into the local caches.
/// Best-effort: every failure degrades to a warning.
pub(super) async fn sync_down(quiet: bool) {
    let gcs = match Gcs::new().await {
        Ok(Some(g)) => g,
        Ok(None) => return,
        Err(e) => {
            status_line!("   Warning: dedup cache sync skipped ({e}).");
            return;
        }
    };
    if let Err(e) = sync_down_inner(&gcs, quiet).await {
        status_line!("   Warning: dedup cache pull failed ({e}).");
    }
}

async fn sync_down_inner(gcs: &Gcs, quiet: bool) -> Result<(), String> {
    let names: HashSet<String> = gcs.list_packs().await?.into_iter().collect();
    let mut synced = load_synced();
    // Packs deleted by compaction leave the state file; their content
    // reappears inside the folded pack, which is unseen and gets merged.
    synced.retain(|n| names.contains(n));
    let mut unseen: Vec<String> = names.difference(&synced).cloned().collect();
    unseen.sort();
    if unseen.is_empty() {
        save_synced(&synced);
        return Ok(());
    }

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for name in unseen {
        let sem = sem.clone();
        let dest = dedup_dir().join(format!(
            ".pack-dl.{}.{}",
            std::process::id(),
            sha256_hex(name.as_bytes())
        ));
        let gcs = gcs.clone();
        tasks.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            gcs.download(&name, &dest).await.map(|()| (name, dest))
        });
    }

    let mut sums = load_summaries();
    let mut embs = load_embeddings();
    let (mut new_s, mut new_e, mut new_x) = (0usize, 0usize, 0usize);
    let (mut merged, mut failed) = (0usize, 0usize);
    while let Some(r) = tasks.join_next().await {
        let Ok(Ok((name, path))) = r else {
            failed += 1;
            continue;
        };
        let data = std::fs::read(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        // A malformed pack is still marked synced: it will never parse
        // better on a re-download, so retrying forever is pure waste.
        if let Some(pack) = parse_pack(&data) {
            for (k, v) in pack.summaries {
                if sums.insert(k, v).is_none() {
                    new_s += 1;
                }
            }
            for (k, v) in pack.embeddings {
                if embs.insert(k, v).is_none() {
                    new_e += 1;
                }
            }
            new_x += merge_exclusions(&pack.exclusions);
        }
        synced.insert(name);
        merged += 1;
    }
    if new_s > 0 {
        save_summaries(&sums);
    }
    if new_e > 0 {
        save_embeddings(&embs);
    }
    save_synced(&synced);
    if !quiet && merged > 0 {
        status_line!(
            "   Dedup sync: merged {merged} pack{} ({new_s} new summaries, {new_e} new vectors, {new_x} new dismissals).",
            if merged == 1 { "" } else { "s" }
        );
    }
    if failed > 0 {
        return Err(format!("{failed} pack downloads failed"));
    }
    Ok(())
}

/// Upload entries generated by this run as one content-named pack, seed an
/// empty remote with the full local index, and compact when the pack count
/// passes [COMPACT_THRESHOLD]. Best-effort like [sync_down].
pub(super) async fn sync_up(
    new_summaries: &HashMap<String, String>,
    new_embeddings: &HashMap<String, Vec<f32>>,
    quiet: bool,
) {
    let gcs = match Gcs::new().await {
        Ok(Some(g)) => g,
        Ok(None) => return,
        Err(e) => {
            // Reached without a prior sync_down from `dedup dismiss`, so a
            // silent return here would silently drop the dismissal push.
            status_line!("   Warning: dedup cache push skipped ({e}).");
            return;
        }
    };
    if let Err(e) = sync_up_inner(&gcs, new_summaries, new_embeddings, false, quiet).await {
        status_line!("   Warning: dedup cache push failed ({e}).");
    }
}

async fn sync_up_inner(
    gcs: &Gcs,
    new_summaries: &HashMap<String, String>,
    new_embeddings: &HashMap<String, Vec<f32>>,
    all_exclusions: bool,
    quiet: bool,
) -> Result<(), String> {
    let existing = gcs.list_packs().await?;
    // An empty remote is seeded with the full local index so the first
    // machine's history becomes every later machine's warm start.
    let seeding = existing.is_empty();
    let (sums, embs) = if seeding {
        (load_summaries(), load_embeddings())
    } else {
        (new_summaries.clone(), new_embeddings.clone())
    };
    let excl = if seeding || all_exclusions {
        all_local_exclusions()
    } else {
        exclusion_delta()
    };
    if sums.is_empty() && embs.is_empty() && excl.is_empty() {
        return Ok(());
    }

    let pack = build_pack(&sums, &excl, &embs);
    let object = gcs.object_name(&format!("packs/{}.pack", sha256_hex(&pack)));
    gcs.upload(&object, &pack).await?;
    mark_remote(&excl);
    let mut synced = load_synced();
    synced.insert(object.clone());
    save_synced(&synced);
    if !quiet {
        status_line!(
            "   Dedup sync: pushed {} ({} summaries, {} vectors, {} dismissals).",
            if seeding { "seed pack" } else { "delta pack" },
            sums.len(),
            embs.len(),
            excl.values().map(Vec::len).sum::<usize>()
        );
    }

    if existing.len() + 1 >= COMPACT_THRESHOLD {
        compact(gcs, &existing, quiet).await?;
    }
    Ok(())
}

/// Fold every pack this machine has already merged into one and delete the
/// originals. Only merged packs are deletable: a pack another machine
/// uploaded after our pull is not in the local caches, and deleting it
/// would drop its entries from the union.
async fn compact(gcs: &Gcs, existing: &[String], quiet: bool) -> Result<(), String> {
    let mut synced = load_synced();
    let deletable: Vec<&String> = existing.iter().filter(|n| synced.contains(*n)).collect();
    if deletable.is_empty() {
        return Ok(());
    }
    let full = build_pack(
        &load_summaries(),
        &all_local_exclusions(),
        &load_embeddings(),
    );
    let object = gcs.object_name(&format!("packs/{}.pack", sha256_hex(&full)));
    gcs.upload(&object, &full).await?;
    synced.insert(object.clone());
    for name in &deletable {
        if **name == object {
            continue;
        }
        gcs.delete(name).await?;
        synced.remove(*name);
    }
    save_synced(&synced);
    if !quiet {
        status_line!(
            "   Dedup sync: compacted {} packs into one.",
            deletable.len()
        );
    }
    Ok(())
}

/// `dragonfly dedup sync`: pull unseen packs, then push. Without `--full`
/// the push only happens when it would seed an empty remote; `--full`
/// uploads the entire local cache as one pack (recovery after entries were
/// generated but never uploaded, e.g. a run that died before its push).
pub async fn cmd_sync(full: bool) -> i32 {
    let gcs = match Gcs::new().await {
        Ok(Some(g)) => g,
        Ok(None) => {
            eprintln!("dedup sync: disabled via {REMOTE_ENV}");
            return 1;
        }
        Err(e) => {
            eprintln!("dedup sync: {e}");
            return 1;
        }
    };
    if let Err(e) = sync_down_inner(&gcs, false).await {
        eprintln!("dedup sync: pull failed: {e}");
        return 1;
    }
    let (sums, embs) = if full {
        (load_summaries(), load_embeddings())
    } else {
        (HashMap::new(), HashMap::new())
    };
    if let Err(e) = sync_up_inner(&gcs, &sums, &embs, full, false).await {
        eprintln!("dedup sync: push failed: {e}");
        return 1;
    }
    println!(
        "Synced with gs://{}/{} ({} summaries, {} embeddings local).",
        gcs.bucket,
        gcs.prefix,
        load_summaries().len(),
        load_embeddings().len()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (HashMap<String, String>, ExclMap, HashMap<String, Vec<f32>>) {
        let mut s = HashMap::new();
        s.insert("a".repeat(64), "reads a config file".to_string());
        s.insert("b".repeat(64), "writes audit rows".to_string());
        let mut x = ExclMap::new();
        x.insert(
            "github.com-lovable-labs-lovable".to_string(),
            vec![ExclusionPair {
                a: "go/api/pkg/a.Foo".to_string(),
                b: "go/api/pkg/b.Bar".to_string(),
                noted_at: "2026-07-21T00:00:00Z".to_string(),
            }],
        );
        let mut e = HashMap::new();
        e.insert("c".repeat(64), vec![0.5f32, -0.25, 1.0]);
        e.insert("d".repeat(64), vec![1.0f32; 768]);
        (s, x, e)
    }

    #[test]
    fn pack_roundtrip() {
        let (s, x, e) = sample();
        let p = parse_pack(&build_pack(&s, &x, &e)).unwrap();
        assert_eq!(s, p.summaries);
        assert_eq!(e, p.embeddings);
        assert_eq!(x.len(), p.exclusions.len());
        let (pair, want) = (
            &p.exclusions["github.com-lovable-labs-lovable"][0],
            &x["github.com-lovable-labs-lovable"][0],
        );
        assert_eq!(
            (&pair.a, &pair.b, &pair.noted_at),
            (&want.a, &want.b, &want.noted_at)
        );
    }

    #[test]
    fn pack_rejects_bad_magic() {
        let (s, x, e) = sample();
        let mut data = build_pack(&s, &x, &e);
        data[0] = b'X';
        assert!(parse_pack(&data).is_none());
    }

    #[test]
    fn pack_tolerates_truncated_embedding_tail() {
        let (s, x, e) = sample();
        let data = build_pack(&s, &x, &e);
        let cut = data.len() - 10;
        let p = parse_pack(&data[..cut]).unwrap();
        assert_eq!(s, p.summaries);
        // One record is whole, the truncated one is dropped.
        assert_eq!(p.embeddings.len(), 1);
    }

    #[test]
    fn pack_empty_maps() {
        let p = parse_pack(&build_pack(
            &HashMap::new(),
            &ExclMap::new(),
            &HashMap::new(),
        ))
        .unwrap();
        assert!(p.summaries.is_empty() && p.exclusions.is_empty() && p.embeddings.is_empty());
    }

    #[test]
    fn pack_truncated_inside_json_sections_is_rejected() {
        let (s, x, e) = sample();
        let data = build_pack(&s, &x, &e);
        // Cut inside the exclusions JSON: the summaries section still parses
        // but the pack must be rejected, not half-applied.
        let cut = 12 + serde_json::to_vec(&s).unwrap().len() + 6;
        assert!(parse_pack(&data[..cut]).is_none());
    }

    #[test]
    fn valid_repo_key_rejects_traversal() {
        assert!(valid_repo_key("github.com-lovable-labs-lovable"));
        assert!(!valid_repo_key(".."));
        assert!(!valid_repo_key("."));
        assert!(!valid_repo_key(""));
        assert!(!valid_repo_key("a/b"));
    }

    #[test]
    fn enc_escapes_path_separators() {
        assert_eq!(enc("dedup/v1/packs/ab.pack"), "dedup%2Fv1%2Fpacks%2Fab.pack");
        assert_eq!(enc("a-b._~Z9"), "a-b._~Z9");
    }
}
