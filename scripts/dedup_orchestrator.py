#!/usr/bin/env python3
"""Repo-wide duplicate-function finder: cluster, rank, then LLM-judge.

`dragonfly dedup` only scores functions a PR touched. This walks the whole repo,
groups near-identical functions into clusters (one cluster = one consolidation
opportunity, not N^2 pairs), ranks them by how much code a fix would delete, and
asks an LLM which are real opportunities.

Reuses dragonfly's caches so nothing is recomputed: summaries keyed by sha256 of
the embed text, embeddings keyed by sha256 of the summary.

Ranking is calibrated against the June 2026 dedup campaign. Clusters whose PR
landed had a median body of 205 chars; the two that never landed, 653 (p75 1303).
Those two were abandoned unreviewed, not rejected on the merits (both needed a new
go.work module, Dockerfile layers, CODEOWNERS and CI path triggers), so body size
predicts *fix cost*, not whether the duplication is real. It is scored as a
priority penalty on that basis; the judge decides realness.

Stages (each cached under --work, keyed by the inputs that produced it, so a
rerun recomputes only what the repo or the flags actually changed):
  extract -> summaries -> cluster -> cohesion -> judge -> report

Usage:
  scripts/dedup_orchestrator.py --repo ~/code/lovable/lovable
  scripts/dedup_orchestrator.py --repo . --judge 40 --model anthropic/claude-sonnet-4-6
  scripts/dedup_orchestrator.py --repo . --stage cluster        # stop before spending LLM calls
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import struct
import subprocess
import sys
import time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor

# Mirrors dedup.rs; changing these invalidates cache reuse.
MIN_CHARS, MAX_CHARS = 120, 6000
GENERIC_RECV = '(_ R)'
SUMMARY_MODEL = 'vertex/gemini-3.1-flash-lite-preview'
EMBED_MODEL, EMBED_DIM = 'text-embedding-005', 768
SKIP_DIRS = {'.git', 'node_modules', 'vendor', 'testdata', '.devenv', '.direnv',
             'dist', 'build', '.next', '.turbo', '.agents', '.funcsim-cache'}

DEDUP_DIR = os.path.expanduser('~/.dragonfly/dedup')

# Largest same-name family the pairwise pass will join. At 60 the biggest fan-outs
# were skipped entirely and reported only via their byte-identical subset: 88
# connector fetchUntraced methods surfaced as a cluster of 24, and the count moved
# with which copies happened to be identical that week. Cost is O(n^2) Jaccard
# within one name.
SAME_NAME_CAP = 300


def log(msg):
    print(f'   {msg}', file=sys.stderr, flush=True)


# ── Stage cache ─────────────────────────────────────────────────────────
# Every snapshot under --work carries a `.key` file naming the inputs that
# produced it. Existence alone is not reuse-worthy: without the key a rerun
# serves the first campaign's functions.json forever and silently ignores
# --fill-summaries and --jac-*, so the report looks current while describing a
# repo state weeks old. Unlike dragonfly's summary and embedding caches, which
# are content-addressed and safe to keep indefinitely, these are whole-repo
# snapshots with no natural key of their own.

def digest(*parts):
    h = hashlib.sha256()
    for part in parts:
        h.update(repr(part).encode())
        h.update(b'\0')
    return h.hexdigest()[:16]


def file_key(path):
    """Identity of an external cache file, by size and mtime."""
    try:
        st = os.stat(path)
    except OSError:
        return 'absent'
    return f'{st.st_size}:{int(st.st_mtime)}'


def repo_key(repo):
    """Key for the extract stage: commit, plus any uncommitted work.

    Tracked edits are covered by content (the `git diff HEAD` digest); untracked
    files only by name, since hashing them all costs more than the stage saves.
    """
    def git(*args):
        p = subprocess.run(['git', '-C', repo, *args], capture_output=True, text=True)
        return p.stdout if p.returncode == 0 else ''
    head = git('rev-parse', 'HEAD').strip() or 'no-git'
    return digest(head, git('status', '--porcelain'), git('diff', 'HEAD'),
                  MIN_CHARS, MAX_CHARS, sorted(SKIP_DIRS))


def cached(work, name, key, reuse_stale=False):
    """The snapshot for `name` if its key still matches, else None."""
    dst = f'{work}/{name}.json'
    if not os.path.exists(dst):
        return None
    kf = f'{work}/{name}.key'
    have = open(kf).read().strip() if os.path.exists(kf) else ''
    if have == key:
        return json.load(open(dst))
    if reuse_stale:
        log(f'{name}: stale ({have or "unkeyed"} != {key}) but --reuse-stale set')
        return json.load(open(dst))
    log(f'{name}: inputs changed ({have or "unkeyed"} -> {key}), recomputing')
    return None


def store(work, name, key, value):
    json.dump(value, open(f'{work}/{name}.json', 'w'))
    open(f'{work}/{name}.key', 'w').write(key)
    return value


# ── Extraction ───────────────────────────────────────────────────────────────

def is_extractable(name):
    return (name.endswith('.go') and not name.endswith('_test.go')
            and not re.search(r'(\.pb|_gen|\.gen|_generated)\.go$', name))


def _skip_literal(src, i):
    """Index past a string/rune/comment beginning at i, else None."""
    c = src[i]
    if c == '/' and i + 1 < len(src):
        if src[i + 1] == '/':
            j = src.find('\n', i)
            return len(src) if j < 0 else j
        if src[i + 1] == '*':
            j = src.find('*/', i + 2)
            return len(src) if j < 0 else j + 2
    if c == '`':
        j = src.find('`', i + 1)
        return len(src) if j < 0 else j + 1
    if c in '"\'':
        j = i + 1
        while j < len(src):
            if src[j] == '\\':
                j += 2
                continue
            if src[j] == c:
                return j + 1
            if src[j] == '\n' and c == '"':
                return j
            j += 1
        return len(src)
    return None


def _match_brace(src, open_idx):
    depth, i = 0, open_idx
    while i < len(src):
        nxt = _skip_literal(src, i)
        if nxt is not None and nxt > i:
            i = nxt
            continue
        if src[i] == '{':
            depth += 1
        elif src[i] == '}':
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return None


def _doc_start(src, func_idx):
    """Contiguous // block above the decl; a blank line detaches it."""
    start = func_idx
    while start > 0:
        line_start = src.rfind('\n', 0, start - 1) + 1
        if not src[line_start:start - 1].strip().startswith('//'):
            break
        start = line_start
        if line_start == 0:
            break
    return start


FUNC_RE = re.compile(r'^func\s*(\([^)]*\)\s*)?([A-Za-z_]\w*)\s*[\(\[]', re.M)


def extract_text(src, rel):
    for ln in src[:2048].splitlines():
        s = ln.strip()
        if s.startswith('package '):
            break
        if s.startswith('// Code generated ') and 'DO NOT EDIT' in s:
            return []
    out = []
    for m in FUNC_RE.finditer(src):
        recv_txt = (m.group(1) or '').strip()
        i, depth, body_open = m.end() - 1, 0, None
        while i < len(src):
            nxt = _skip_literal(src, i)
            if nxt is not None and nxt > i:
                i = nxt
                continue
            ch = src[i]
            if ch in '([':
                depth += 1
            elif ch in ')]':
                depth -= 1
            elif ch == '{' and depth <= 0:
                body_open = i
                break
            i += 1
        if body_open is None:
            continue
        end = _match_brace(src, body_open)
        if end is None:
            continue
        start = _doc_start(src, m.start())
        source = src[start:end]
        if len(source.encode()) < MIN_CHARS:
            continue
        recv, embed = '', source
        if recv_txt:
            parts = recv_txt[1:-1].strip().split()
            recv = ''.join(parts[1:]) if len(parts) > 1 else ''.join(parts)
            ro = source.find(recv_txt)
            if ro >= 0:
                embed = source[:ro] + GENERIC_RECV + source[ro + len(recv_txt):]
        embed = embed[:MAX_CHARS]
        d = os.path.dirname(rel)
        name = m.group(2)
        ident = f'{d}.({recv}).{name}' if recv else f'{d}.{name}'
        out.append(dict(id=ident, path=rel, line=src.count('\n', 0, m.start()) + 1,
                        recv=recv, name=name, body=source, embed_text=embed,
                        source_hash=hashlib.sha256(embed.encode()).hexdigest()))
    return out


def stage_extract(repo, work, key, reuse_stale=False):
    hit = cached(work, 'functions', key, reuse_stale)
    if hit is not None:
        return hit
    files = []
    for root, dirs, names in os.walk(repo):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        files += [os.path.join(root, n) for n in names if is_extractable(n)]
    fns = {}
    for p in files:
        try:
            src = open(p, encoding='utf-8', errors='replace').read()
        except OSError:
            continue
        for f in extract_text(src, os.path.relpath(p, repo)):
            fns.setdefault(f['id'], f)   # first definition wins on build-tag variants
    log(f'extracted {len(fns)} functions from {len(files)} files')
    return store(work, 'functions', key, fns)


# ── Summaries (dragonfly cache, optional top-up) ─────────────────────────────

def sanitize(s):
    return ''.join('_' if c in '/: ' else c for c in s)


def save_summary_cache(path, got):
    """Merge freshly generated summaries into dragonfly's shared cache.

    Re-read before writing: dragonfly's own dedup runs own this file too, and
    dumping our in-memory copy would drop whatever they added meanwhile. Skipping
    the write entirely means every --fill-summaries run re-pays for the same
    summaries, since the cache is what makes them a one-time cost.
    """
    if not got:
        return
    merged = json.load(open(path)) if os.path.exists(path) else {}
    merged.update(got)
    tmp = f'{path}.tmp-{os.getpid()}'
    json.dump(merged, open(tmp, 'w'))
    os.replace(tmp, path)
    log(f'wrote {len(got)} new summaries to the shared cache ({len(merged)} total)')


def stage_summaries(fns, work, kit, model, fill, key, reuse_stale=False):
    if fill:
        log('--fill-summaries set; rebuilding summaries regardless of cache')
    else:
        hit = cached(work, 'summaries', key, reuse_stale)
        if hit is not None:
            return hit
    path = f'{DEDUP_DIR}/summaries-{sanitize(SUMMARY_MODEL)}.json'
    cache = json.load(open(path)) if os.path.exists(path) else {}
    log(f'dragonfly summary cache: {len(cache)} entries')
    need = {f['source_hash'] for f in fns.values()} - set(cache)
    log(f'cache covers {100*(1-len(need)/max(len(set(f["source_hash"] for f in fns.values())),1)):.1f}% '
        f'of this repo; {len(need)} uncached')
    if fill and need:
        by_hash = {}
        for f in fns.values():
            by_hash.setdefault(f['source_hash'], f['embed_text'])
        todo = sorted(need)[:fill]
        log(f'summarizing {len(todo)} uncached functions with {SUMMARY_MODEL}')
        got = summarize(todo, by_hash, kit, SUMMARY_MODEL)
        cache.update(got)
        save_summary_cache(path, got)
        if len(got) < len(todo):
            log(f'{len(todo)-len(got)} summaries came back unparseable; they stay '
                f'uncached and will be retried next --fill-summaries run')
    out = {}
    for f in fns.values():
        s = cache.get(f['source_hash'], '').strip()
        if s:
            out[f['id']] = s
    log(f'{len(out)}/{len(fns)} functions have summaries')
    return store(work, 'summaries', key, out)


LINE_RE = re.compile(r'(?m)^\s*(\d+)\s*[:.)]\s*(\S.*?)\s*$')
SUMMARY_SYSTEM = ("You summarize what each Go function does in one terse line describing its "
                  "specific operation and effect. Ignore the receiver, the function name, "
                  "error-wrapping, logging, and generic scaffolding; focus on the distinctive "
                  "logic that sets it apart. Output exactly one line per function in the form "
                  "'<id>: <summary>', and nothing else.")


def kit_llm(kit, model, system, prompt, cwd, retries=3):
    for _ in range(retries):
        try:
            p = subprocess.run([kit, 'llm', '-m', model, '--system', system],
                               input=prompt, capture_output=True, text=True,
                               cwd=cwd, timeout=600)
            if p.returncode == 0 and p.stdout.strip():
                return p.stdout
        except subprocess.TimeoutExpired:
            pass
    return ''


def summarize(hashes, by_hash, kit, model, batch=20, conc=8):
    batches = [hashes[i:i + batch] for i in range(0, len(hashes), batch)]

    def run(b):
        prompt = ''.join(f'### {i}\n{by_hash[h][:3000]}\n' for i, h in enumerate(b))
        raw = kit_llm(kit, model, SUMMARY_SYSTEM, prompt, os.path.dirname(os.path.dirname(kit)))
        got = {int(m[1]): m[2] for m in LINE_RE.finditer(raw)}
        return {h: got[i] for i, h in enumerate(b) if i in got}

    out = {}
    with ThreadPoolExecutor(conc) as ex:
        for r in ex.map(run, batches):
            out.update(r)
    return out


# ── Candidate clusters ───────────────────────────────────────────────────────

class Union:
    def __init__(self):
        self.p = {}

    def find(self, x):
        self.p.setdefault(x, x)
        while self.p[x] != x:
            self.p[x] = self.p[self.p[x]]
            x = self.p[x]
        return x

    def join(self, a, b):
        ra, rb = self.find(a), self.find(b)
        if ra != rb:
            self.p[ra] = rb


def norm_name(n):
    return re.sub(r'[^a-z0-9]', '', n.lower())


def body_tokens(s):
    return set(re.findall(r'[A-Za-z_]\w+', s))


def jaccard(a, b):
    if not a or not b:
        return 0.0
    return len(a & b) / len(a | b)


STOP = set('the a an of to and or in for with on returns return given from into '
           'that this it its is are be by as at if then else new value values '
           'string error context bool int list map set get'.split())


def stage_cluster(fns, sums, work, jac_body, jac_summary, key, reuse_stale=False):
    hit = cached(work, 'clusters', key, reuse_stale)
    if hit is not None:
        return hit
    u = Union()
    ids = list(fns)

    # 1. identical embed text: the byte-for-byte fan-outs (mixin candidates)
    by_hash = defaultdict(list)
    for i in ids:
        by_hash[fns[i]['source_hash']].append(i)
    n1 = 0
    for g in by_hash.values():
        for x in g[1:]:
            u.join(g[0], x)
            n1 += 1

    # 2. identical summary: same behavior, cosmetically different bodies
    by_sum = defaultdict(list)
    for i, s in sums.items():
        by_sum[s.lower()].append(i)
    n2 = 0
    for g in by_sum.values():
        if len(g) > 40:            # a summary this generic carries no signal
            continue
        for x in g[1:]:
            u.join(g[0], x)
            n2 += 1

    # 3. same function name across packages + near-identical body
    by_name = defaultdict(list)
    for i in ids:
        by_name[norm_name(fns[i]['name'])].append(i)
    n3 = 0
    for g in by_name.values():
        if len(g) < 2 or len(g) > SAME_NAME_CAP:
            continue
        toks = {i: body_tokens(fns[i]['embed_text']) for i in g}
        for a in range(len(g)):
            for b in range(a + 1, len(g)):
                if jaccard(toks[g[a]], toks[g[b]]) >= jac_body:
                    u.join(g[a], g[b])
                    n3 += 1

    # 4. cross-name discovery: rare summary tokens as blocking keys
    inv = defaultdict(list)
    stoks = {}
    for i, s in sums.items():
        t = {w for w in re.findall(r'[a-z]{4,}', s.lower()) if w not in STOP}
        stoks[i] = t
        for w in t:
            inv[w].append(i)
    cand = Counter()
    for w, group in inv.items():
        if len(group) > 60:        # common word, useless as a key
            continue
        for a in range(len(group)):
            for b in range(a + 1, len(group)):
                if u.find(group[a]) != u.find(group[b]):
                    cand[tuple(sorted((group[a], group[b])))] += 1
    n4 = 0
    for (x, y), shared in cand.items():
        if shared >= 2 and jaccard(stoks[x], stoks[y]) >= jac_summary:
            if jaccard(body_tokens(fns[x]['embed_text']),
                       body_tokens(fns[y]['embed_text'])) >= 0.5:
                u.join(x, y)
                n4 += 1
    log(f'joins: identical-body {n1}, identical-summary {n2}, same-name {n3}, cross-name {n4}')

    groups = defaultdict(list)
    for i in ids:
        if i in u.p:
            groups[u.find(i)].append(i)
    clusters = [sorted(g) for g in groups.values() if len(g) > 1]

    # One pattern must be one cluster: the blocking above splits e.g. 48 copies of
    # the same Fetch delegation into 4 groups, and the judge then contradicts
    # itself across them. Re-merge same-name clusters whose bodies still agree.
    merged, before = [], len(clusters)
    by_dom = defaultdict(list)
    for c in clusters:
        names = Counter(norm_name(fns[i]['name']) for i in c)
        dom, cnt = names.most_common(1)[0]
        (by_dom[dom] if cnt == len(c) else by_dom[f'~{id(c)}']).append(c)
    for dom, cs in by_dom.items():
        if len(cs) == 1:
            merged.append(cs[0])
            continue
        reps = [(c, body_tokens(fns[c[0]]['embed_text'])) for c in cs]
        pool = []
        for c, t in reps:
            for grp in pool:
                if jaccard(t, grp[1]) >= 0.6:
                    grp[0].extend(c)
                    break
            else:
                pool.append([list(c), t])
        merged += [sorted(set(g[0])) for g in pool]
    clusters = merged
    log(f'{before} -> {len(clusters)} clusters after same-name re-merge')
    log(f'{len(clusters)} candidate clusters covering {sum(len(c) for c in clusters)} functions')
    return store(work, 'clusters', key, clusters)


# ── Cohesion from dragonfly's embeddings ─────────────────────────────────────

def load_embeddings(keys):
    path = f'{DEDUP_DIR}/embeddings-{EMBED_MODEL}-{EMBED_DIM}.bin'
    if not os.path.exists(path):
        return {}
    want, out = set(keys), {}
    with open(path, 'rb') as fh:
        data = fh.read()
    if data[:8] != b'DFEMB01\n':
        return {}
    pos = 8
    while pos + 68 <= len(data):
        h = data[pos:pos + 64].decode('ascii', 'replace')
        dim = struct.unpack('<I', data[pos + 64:pos + 68])[0]
        pos += 68
        if pos + dim * 4 > len(data):
            break
        if h in want:
            out[h] = struct.unpack(f'<{dim}f', data[pos:pos + dim * 4])
        pos += dim * 4
    return out


def cos(a, b):
    d = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(y * y for y in b))
    return d / (na * nb) if na and nb else 0.0


def stage_cohesion(clusters, sums, work, key, reuse_stale=False):
    hit = cached(work, 'cohesion', key, reuse_stale)
    if hit is not None:
        return hit
    keys = {}
    for c in clusters:
        for i in c:
            if i in sums:
                keys[i] = hashlib.sha256(sums[i].encode()).hexdigest()
    vecs = load_embeddings(set(keys.values()))
    log(f'loaded {len(vecs)} cached embeddings for {len(keys)} cluster members')
    out = {}
    for ci, c in enumerate(clusters):
        vs = [vecs[keys[i]] for i in c if i in keys and keys[i] in vecs]
        if len(vs) < 2:
            out[str(ci)] = None
            continue
        pairs = [cos(vs[a], vs[b]) for a in range(len(vs)) for b in range(a + 1, len(vs))]
        out[str(ci)] = dict(mean=sum(pairs) / len(pairs), min=min(pairs), n=len(vs))
    return store(work, 'cohesion', key, out)


# ── Ranking ──────────────────────────────────────────────────────────────────

def size_factor(median_chars):
    """Fix-cost penalty: big clusters need cross-module plumbing and historically
    stalled unreviewed (see module docstring). Not a realness signal."""
    if median_chars <= 300:
        return 1.0
    if median_chars <= 600:
        return 0.7
    if median_chars <= 1200:
        return 0.35
    return 0.15


def rank(clusters, fns, sums, coh):
    rows = []
    for ci, c in enumerate(clusters):
        bodies = [len(fns[i]['embed_text']) for i in c]
        bodies.sort()
        med = bodies[len(bodies) // 2]
        n = len(c)
        pkgs = {os.path.dirname(fns[i]['path']) for i in c}
        recvs = {fns[i]['recv'] for i in c}
        names = {norm_name(fns[i]['name']) for i in c}
        identical = len({fns[i]['source_hash'] for i in c}) == 1
        h = coh.get(str(ci)) or {}
        payoff = (n - 1) * med
        score = payoff * size_factor(med)
        if identical:
            score *= 1.3
        # Interface fan-out inside one package: the reviewer dismisses these as
        # intentionally parallel, and dropping them cost 0/943 genuine pairs.
        if len(pkgs) == 1 and len(recvs) > 1 and len(names) == 1:
            score *= 0.3
        if h.get('mean'):
            score *= max(0.3, min(1.2, (h['mean'] - 0.6) / 0.3))
        rows.append(dict(ci=ci, n=n, median_chars=med, identical=identical,
                         packages=len(pkgs), names=sorted(names)[:3],
                         cohesion=round(h.get('mean', 0), 3) if h else None,
                         removable_chars=payoff, score=round(score, 1),
                         members=c))
    rows.sort(key=lambda r: -r['score'])
    return rows


# ── LLM judge ────────────────────────────────────────────────────────────────

JUDGE_SYSTEM = """You decide whether a cluster of similar Go functions is a real
consolidation opportunity.

REAL means: one shared helper (or an embedded mixin type) could absorb every copy
with no behavior change and no new cross-module abstraction, and a future bug fix
would otherwise have to land in each copy separately.

NOT REAL means any of:
- per-service / per-provider / per-tenant wiring that is intentionally parallel and
  expected to diverge (config assembly, service bootstrap, observability setup)
- the copies already delegate to a shared helper and only the call scaffolding repeats
- consolidating would force a new dependency between otherwise independent modules,
  or an interface with more parameters than the duplication costs
- generated or vendored code
- the bodies differ in a way that matters (different defaults, error handling, ordering
  with observable effect)

Judge the code you are shown, not the names. Reply with ONLY a JSON object:
{"verdict":"REAL"|"NOT_REAL","confidence":0-100,"reason":"<one sentence>",
 "direction":"<the consolidation, or empty if NOT_REAL>"}"""


def judge_cluster(row, fns, sums, kit, model, cwd, max_members=4, max_chars=1800):
    ms = row['members'][:max_members]
    parts = [f"Cluster of {row['n']} similar functions across {row['packages']} package(s); "
             f"median body {row['median_chars']} chars; "
             f"{'byte-identical bodies' if row['identical'] else 'near-identical bodies'}."]
    if row['n'] > len(ms):
        parts.append(f'Showing {len(ms)} of {row["n"]} members.')
    for i in ms:
        f = fns[i]
        parts.append(f"\n--- {i}  ({f['path']}:{f['line']})\n"
                     f"summary: {sums.get(i, '(none)')}\n{f['body'][:max_chars]}")
    raw = kit_llm(kit, model, JUDGE_SYSTEM, '\n'.join(parts), cwd)
    m = re.search(r'\{.*\}', raw, re.S)
    if not m:
        return dict(verdict='ERROR', confidence=0, reason='no json from judge', direction='')
    try:
        d = json.loads(m.group(0))
    except json.JSONDecodeError:
        return dict(verdict='ERROR', confidence=0, reason='bad json', direction='')
    return dict(verdict=str(d.get('verdict', 'ERROR')).upper(),
                confidence=int(d.get('confidence', 0) or 0),
                reason=str(d.get('reason', ''))[:400],
                direction=str(d.get('direction', ''))[:400])


def members_sha(row):
    return digest(sorted(row['members']))


def stage_judge(rows, fns, sums, work, kit, model, limit, conc):
    """Judge the top `limit` clusters, reusing verdicts for unchanged findings.

    Verdicts are stored under [finding_key], never the cluster index: an index is
    positional, so any re-cluster silently re-points an existing verdict at a
    different set of functions. A verdict is reused only while its cluster's
    membership is also unchanged, since the reason text cites specific copies.
    """
    dst = f'{work}/verdicts.json'
    done = json.load(open(dst)) if os.path.exists(dst) else {}
    legacy = [k for k in done if k.isdigit()]
    if legacy:
        log(f'ignoring {len(legacy)} index-keyed verdicts from an older run '
            f'(migrate with --migrate-verdicts before re-clustering)')

    keyed = {r['ci']: (finding_key(r, fns), members_sha(r)) for r in rows[:limit]}
    todo = []
    for r in rows[:limit]:
        fk, sha = keyed[r['ci']]
        prev = done.get(fk)
        if prev and prev.get('members_sha') == sha:
            continue
        todo.append(r)
    if todo:
        cwd = os.path.dirname(os.path.dirname(kit))
        log(f'judging {len(todo)} clusters with {model} '
            f'({len(rows[:limit]) - len(todo)} reused)')
        t = time.time()

        def run(r):
            fk, sha = keyed[r['ci']]
            v = judge_cluster(r, fns, sums, kit, model, cwd)
            return fk, dict(v, members_sha=sha)

        with ThreadPoolExecutor(conc) as ex:
            for k, v in ex.map(run, todo):
                done[k] = v
        json.dump(done, open(dst, 'w'))
        log(f'judged in {time.time()-t:.0f}s')
    return {str(ci): done[fk] for ci, (fk, _) in keyed.items() if fk in done}


# ── Report ───────────────────────────────────────────────────────────────────

def report(rows, verdicts, fns, sums, limit, out_path):
    lines = ['# Duplicate-function opportunities', '']
    real = [r for r in rows[:limit] if verdicts.get(str(r['ci']), {}).get('verdict') == 'REAL']
    notreal = [r for r in rows[:limit] if verdicts.get(str(r['ci']), {}).get('verdict') == 'NOT_REAL']
    err = [r for r in rows[:limit] if verdicts.get(str(r['ci']), {}).get('verdict') == 'ERROR']
    lines.append(f'{len(rows)} candidate clusters ranked; top {limit} judged: '
                 f'**{len(real)} real**, {len(notreal)} rejected, {len(err)} errored.')
    lines.append('')
    for title, group in (('Real opportunities', real), ('Rejected by judge', notreal)):
        lines += [f'## {title}', '']
        if not group:
            lines += ['(none)', '']
        for r in group:
            v = verdicts[str(r['ci'])]
            lines.append(f"### {' / '.join(r['names'])} — {r['n']} copies, "
                         f"~{r['removable_chars']//1000 or r['removable_chars']}"
                         f"{'k' if r['removable_chars']>=1000 else ''} chars removable")
            lines.append(f"score {r['score']} · median body {r['median_chars']} chars · "
                         f"{r['packages']} packages · cohesion {r['cohesion']} · "
                         f"{'identical' if r['identical'] else 'near-identical'} · "
                         f"confidence {v['confidence']}")
            lines.append(f"**{v['reason']}**")
            if v.get('direction'):
                lines.append(f"Consolidation: {v['direction']}")
            lines.append('')
            for i in r['members'][:12]:
                lines.append(f"- `{i}` ({fns[i]['path']}:{fns[i]['line']})")
            if r['n'] > 12:
                lines.append(f'- …and {r["n"]-12} more')
            lines.append('')
    open(out_path, 'w').write('\n'.join(lines))
    log(f'wrote {out_path}')
    return real, notreal


# ── Linear hand-off ──────────────────────────────────────────────────────────
# Findings become Linear issues under one label; goalie-triage polls that label
# and runs a consolidation agent per issue. Credentials and team default come
# from goalie-triage's own config so there is one place to rotate the key.

GOALIE_ENV = os.path.expanduser('~/.config/goalie-triage/env')
LINEAR_API = 'https://api.linear.app/graphql'
DEFAULT_TEAM_ID = '6e939f8f-cd61-491d-89d8-376405f32a07'   # Agent team
KEY_PREFIX = 'dedup-key:'
# A finding under a Done issue that comes back means the consolidation was partial
# or the pattern regressed, which is worth saying. Under a Canceled or Duplicate
# issue it means the finding was declined and will resurface on every run forever,
# so saying anything is noise. Both still suppress re-filing.
COMPLETED_STATES = {'Done'}
DECLINED_STATES = {'Canceled', 'Duplicate'}


def goalie_config():
    cfg = {}
    if os.path.exists(GOALIE_ENV):
        for line in open(GOALIE_ENV):
            line = line.strip()
            if line and not line.startswith('#') and '=' in line:
                k, v = line.split('=', 1)
                cfg[k.strip()] = v.strip().strip('"').strip("'")
    for k in ('LINEAR_API_KEY', 'AGENT_TEAM_ID'):
        if os.environ.get(k):
            cfg[k] = os.environ[k]
    return cfg


class Linear:
    def __init__(self, key):
        self.key = key

    def call(self, query, variables=None):
        import urllib.error
        import urllib.request
        req = urllib.request.Request(
            LINEAR_API,
            data=json.dumps({'query': query, 'variables': variables or {}}).encode(),
            headers={'Authorization': self.key, 'Content-Type': 'application/json'})
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                payload = json.loads(r.read())
        except urllib.error.HTTPError as e:
            raise RuntimeError(f'Linear HTTP {e.code}: {e.read().decode()[:300]}')
        if 'errors' in payload:
            raise RuntimeError(f'Linear error: {payload["errors"]}')
        return payload['data']

    def label_id(self, team_id, name, create=True):
        d = self.call("""
          query($t: ID!, $n: String!) {
            issueLabels(filter: {name: {eq: $n}, team: {id: {eq: $t}}}, first: 1) {
              nodes { id name }
            }
          }""", {'t': team_id, 'n': name})
        nodes = d['issueLabels']['nodes']
        if nodes:
            return nodes[0]['id']
        if not create:
            return None
        d = self.call("""
          mutation($t: String!, $n: String!) {
            issueLabelCreate(input: {teamId: $t, name: $n, color: "#bb87fc"}) {
              success issueLabel { id }
            }
          }""", {'t': team_id, 'n': name})
        return d['issueLabelCreate']['issueLabel']['id']

    def existing_keys(self, label_id, limit=250):
        """Fingerprints already filed under the label, in any state, with that state.

        Closed issues count as filed: re-filing work already done is worse than
        leaving a resurfaced finding out of the report. The state rides along so
        callers can say which of the two a suppression is.
        """
        d = self.call("""
          query($l: ID!, $n: Int!) {
            issues(filter: {labels: {id: {eq: $l}}}, first: $n) {
              nodes { identifier description state { name } }
            }
          }""", {'l': label_id, 'n': limit})
        out = {}
        for n in d['issues']['nodes']:
            for line in (n.get('description') or '').splitlines():
                if line.strip().startswith(KEY_PREFIX):
                    out[line.split(KEY_PREFIX, 1)[1].strip()] = (
                        n['identifier'], (n.get('state') or {}).get('name', '?'))
        return out

    def create(self, team_id, label_id, title, description):
        d = self.call("""
          mutation($t: String!, $ti: String!, $d: String!, $l: [String!]) {
            issueCreate(input: {teamId: $t, title: $ti, description: $d, labelIds: $l}) {
              success issue { identifier url }
            }
          }""", {'t': team_id, 'ti': title, 'd': description, 'l': [label_id]})
        return d['issueCreate']['issue']


def common_prefix(paths):
    parts = [p.split('/') for p in paths]
    out = []
    for i in range(min(len(p) for p in parts)):
        seg = parts[0][i]
        if all(p[i] == seg for p in parts):
            out.append(seg)
        else:
            break
    return '/'.join(out)


def finding_key(row, fns):
    """Stable across line moves and new members joining the cluster.

    The shape suffix separates distinct implementation families that share a name
    and a scope — two RevokeToken clusters in the same connectors tree are
    different findings, and without it the second is skipped as already-filed.
    """
    name = sorted(row['names'])[0]
    scope = common_prefix([os.path.dirname(fns[i]['path']) for i in row['members']])
    modal = Counter(fns[i]['source_hash'] for i in row['members']).most_common(1)[0][0]
    rep = next(fns[i]['embed_text'] for i in row['members'] if fns[i]['source_hash'] == modal)
    shape = hashlib.sha1(' '.join(sorted(body_tokens(rep))).encode()).hexdigest()[:6]
    return f'{name}@{scope or "repo"}#{shape}'


def issue_text(row, verdict, fns, sums, model, max_members=25):
    scope = common_prefix([os.path.dirname(fns[i]['path']) for i in row['members']]) or 'repo'
    name = ' / '.join(row['names'])
    title = f'dedup: {name} duplicated across {row["packages"]} packages'
    chars = row['removable_chars']
    body = [
        f"**{verdict['reason']}**", '',
        '## Proposed consolidation', '',
        verdict.get('direction') or '(none suggested)', '',
        '## Evidence', '',
        f'- {row["n"]} copies in {row["packages"]} packages under `{scope}`',
        f'- bodies are **{"byte-identical" if row["identical"] else "near-identical"}**'
        f' (embedding cohesion {row["cohesion"]})',
        f'- median body {row["median_chars"]} chars; roughly {chars} chars removable',
        f'- judge confidence {verdict["confidence"]}/100',
        '', '## Copies', '',
    ]
    for i in row['members'][:max_members]:
        body.append(f'- `{fns[i]["path"]}:{fns[i]["line"]}` — `{i}`')
    if row['n'] > max_members:
        body.append(f'- …and {row["n"] - max_members} more')
    body += [
        '', '## Provenance', '',
        f'Machine-generated by `dragonfly/scripts/dedup_orchestrator.py`; judged by '
        f'`{model}`. Cluster rank score {row["score"]}. Verify the duplication still '
        f'exists before acting — `main` moves.', '',
        f'{KEY_PREFIX} {finding_key(row, fns)}',
    ]
    return title, '\n'.join(body)


def stage_post_linear(rows, verdicts, fns, sums, work, label, team_id,
                      limit, min_conf, model, dry_run):
    """File qualifying findings as Linear issues, or report what filing would do.

    A dry run resolves the same already-filed set as a live run and applies the
    same skip-and-defer logic, so its output names the issues that would really
    be created. It never mutates Linear: the label is looked up with create=False.
    """
    cfg = goalie_config()
    key = cfg.get('LINEAR_API_KEY', '').strip()
    if not key and not dry_run:
        sys.exit(f'LINEAR_API_KEY not set (looked in {GOALIE_ENV} and env)')
    team_id = team_id or cfg.get('AGENT_TEAM_ID') or DEFAULT_TEAM_ID

    picked = []
    for r in rows:
        v = verdicts.get(str(r['ci']))
        if not v or v['verdict'] != 'REAL' or v['confidence'] < min_conf:
            continue
        picked.append((r, v))

    lin, already, blind = None, {}, False
    if key:
        lin = Linear(key)
        label_id = lin.label_id(team_id, label, create=not dry_run)
        already = lin.existing_keys(label_id) if label_id else {}
        log(f'label "{label}" has {len(already)} findings already filed')
    else:
        blind = True
        log(f'no LINEAR_API_KEY in {GOALIE_ENV}; cannot tell which findings are '
            f'already filed, so every qualifier below is shown as new')

    filed, skipped, deferred, declined, resurfaced = [], 0, 0, 0, []
    for r, v in picked:
        k = finding_key(r, fns)
        if k in already:
            skipped += 1
            ident, state = already[k]
            if state in COMPLETED_STATES:
                resurfaced.append((ident, state, k))
            elif state in DECLINED_STATES:
                declined += 1
            if dry_run:
                tag = f'{state}, declined' if state in DECLINED_STATES else state
                print(f'  {ident} ({tag}){"":<{max(1, 14 - len(tag))}}'
                      f'{issue_text(r, v, fns, sums, model)[0]}   [{k}]')
            continue
        if len(filed) >= limit:
            deferred += 1
            if dry_run:
                print(f'  deferred{"":13}{issue_text(r, v, fns, sums, model)[0]}   [{k}]')
            continue
        title, body = issue_text(r, v, fns, sums, model)
        if dry_run:
            print(f'  would file{"":11}{title}   [{k}]')
            filed.append(dict(key=k, title=title, ci=r['ci']))
            continue
        issue = lin.create(team_id, label_id, title, body)
        already[k] = (issue['identifier'], 'Triage')
        filed.append(dict(key=k, identifier=issue['identifier'], url=issue['url'],
                          title=title, ci=r['ci']))
        log(f'filed {issue["identifier"]}  {title}')
    for ident, state, k in resurfaced:
        log(f'{ident} is {state} but its finding is back: the consolidation was '
            f'partial, or the pattern regressed [{k}]')
    verb = 'would be filed' if dry_run else 'filed'
    log(f'{len(picked)} qualified: {len(filed)} {verb}, {skipped} already present'
        f'{f" ({declined} declined)" if declined else ""}'
        f'{" (unknown, no API key)" if blind else ""}, '
        f'{deferred} deferred by --post-limit {limit}')
    if dry_run:
        return []
    path = f'{work}/filed.json'
    prev = json.load(open(path)) if os.path.exists(path) else []
    json.dump(prev + filed, open(path, 'w'))
    return filed


STAGES = ['extract', 'summaries', 'cluster', 'cohesion', 'rank', 'judge', 'report']


def migrate_verdicts(repo, work, kit, a):
    """Rekey index-keyed verdicts onto [finding_key], using the caches that produced them.

    Only meaningful before the snapshots are recomputed: cluster indices are
    resolved against the clusters.json those verdicts were judged from.
    """
    dst = f'{work}/verdicts.json'
    if not os.path.exists(dst):
        sys.exit(f'no verdicts at {dst}')
    done = json.load(open(dst))
    legacy = {k: v for k, v in done.items() if k.isdigit()}
    if not legacy:
        log('no index-keyed verdicts to migrate')
        return
    for name in ('functions', 'clusters'):
        if not os.path.exists(f'{work}/{name}.json'):
            sys.exit(f'{name}.json missing; cannot resolve cluster indices')
    fns = json.load(open(f'{work}/functions.json'))
    sums = json.load(open(f'{work}/summaries.json'))
    clusters = json.load(open(f'{work}/clusters.json'))
    coh = json.load(open(f'{work}/cohesion.json'))
    rows = {str(r['ci']): r for r in rank(clusters, fns, sums, coh)}
    out = {k: v for k, v in done.items() if not k.isdigit()}
    moved = 0
    for ci, v in legacy.items():
        r = rows.get(ci)
        if not r:
            continue
        out[finding_key(r, fns)] = dict(v, members_sha=members_sha(r))
        moved += 1
    open(f'{dst}.index-keyed.bak', 'w').write(json.dumps(done))
    json.dump(out, open(dst, 'w'))
    log(f'rekeyed {moved}/{len(legacy)} verdicts; backup at {dst}.index-keyed.bak')


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--repo', default='.')
    ap.add_argument('--work', default=None, help='cache dir (default ~/.dragonfly/dedup/orchestrator/<repo>)')
    ap.add_argument('--stage', choices=STAGES, default='report', help='stop after this stage')
    ap.add_argument('--judge', type=int, default=40, help='clusters to send to the judge')
    ap.add_argument('--model', default='anthropic/claude-sonnet-4-6')
    ap.add_argument('--conc', type=int, default=6)
    ap.add_argument('--kit', default=None, help='path to kit (default <repo>/.devenv/state/go/bin/kit)')
    ap.add_argument('--fill-summaries', type=int, default=0,
                    help='summarize up to N functions missing from the dragonfly cache')
    ap.add_argument('--jac-body', type=float, default=0.85)
    ap.add_argument('--jac-summary', type=float, default=0.6)
    ap.add_argument('--out', default=None)
    ap.add_argument('--post-linear', action='store_true',
                    help='file REAL findings as Linear issues for goalie-triage to pick up')
    ap.add_argument('--linear-label', default='dedup')
    ap.add_argument('--linear-team', default=None)
    ap.add_argument('--post-limit', type=int, default=3,
                    help='max issues per run; goalie-triage only runs 2 triages at a time')
    ap.add_argument('--min-confidence', type=int, default=85)
    ap.add_argument('--dry-run-linear', action='store_true')
    ap.add_argument('--reuse-stale', action='store_true',
                    help='serve stage snapshots whose inputs have changed')
    ap.add_argument('--migrate-verdicts', action='store_true',
                    help='rekey index-keyed verdicts from an older run, then exit')
    a = ap.parse_args()

    repo = os.path.abspath(os.path.expanduser(a.repo))
    work = a.work or f'{DEDUP_DIR}/orchestrator/{os.path.basename(repo)}'
    os.makedirs(work, exist_ok=True)
    kit = a.kit or f'{repo}/.devenv/state/go/bin/kit'
    stop = STAGES.index(a.stage)

    if a.migrate_verdicts:
        migrate_verdicts(repo, work, kit, a)
        return

    k_fns = repo_key(repo)
    fns = stage_extract(repo, work, k_fns, a.reuse_stale)
    if stop == 0:
        return
    k_sums = digest(k_fns, SUMMARY_MODEL,
                    file_key(f'{DEDUP_DIR}/summaries-{sanitize(SUMMARY_MODEL)}.json'))
    sums = stage_summaries(fns, work, kit, a.model, a.fill_summaries, k_sums, a.reuse_stale)
    if stop == 1:
        return
    # Content, not k_sums: --fill-summaries rebuilds summaries without changing
    # its key, and those new summaries are what clustering joins on.
    k_clusters = digest(digest(json.dumps(sums, sort_keys=True)),
                        a.jac_body, a.jac_summary, SAME_NAME_CAP)
    clusters = stage_cluster(fns, sums, work, a.jac_body, a.jac_summary,
                             k_clusters, a.reuse_stale)
    if stop == 2:
        return
    k_coh = digest(k_clusters,
                   file_key(f'{DEDUP_DIR}/embeddings-{EMBED_MODEL}-{EMBED_DIM}.bin'))
    coh = stage_cohesion(clusters, sums, work, k_coh, a.reuse_stale)
    if stop == 3:
        return
    rows = rank(clusters, fns, sums, coh)
    json.dump(rows, open(f'{work}/ranked.json', 'w'))
    log(f'top score {rows[0]["score"] if rows else 0}')
    if stop == 4:
        for r in rows[:20]:
            print(f"{r['score']:>9}  n={r['n']:<3} med={r['median_chars']:<5} "
                  f"pkgs={r['packages']:<3} {'/'.join(r['names'])}")
        return
    if not os.path.exists(kit):
        sys.exit(f'kit not found at {kit}; pass --kit')
    verdicts = stage_judge(rows, fns, sums, work, kit, a.model, a.judge, a.conc)
    if stop == 5:
        return
    out = a.out or f'{work}/report.md'
    real, notreal = report(rows, verdicts, fns, sums, a.judge, out)
    print(f'{len(real)} real opportunities, {len(notreal)} rejected -> {out}')

    if a.post_linear or a.dry_run_linear:
        filed = stage_post_linear(rows, verdicts, fns, sums, work, a.linear_label,
                                  a.linear_team, a.post_limit, a.min_confidence,
                                  a.model, a.dry_run_linear)
        for f in filed:
            print(f'{f["identifier"]}  {f["url"]}')


if __name__ == '__main__':
    main()
