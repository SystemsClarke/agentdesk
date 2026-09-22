"""Hybrid search over the memory vault -- the software half of it.

This used to be two standalone scripts (`index_vault.py`, `vault_search.py`)
living loose in a folder next to the vault, on their own, in no git repo at
all. That split the tool from the app that actually needed it (agentdesk/
vault.py already mirrors wiki posts INTO the vault; nothing read them back
out) and put engineering work outside of any version control, silently.

The fix keeps a real boundary rather than erasing it: CODE lives here, in
agentdesk's own repo, versioned and reviewed like every other module. DATA --
the vault's notes, and the vectors/hashes derived from them, which are as
proprietary as the notes themselves -- stays under paths.VAULT_SEARCH_DIR,
inside the vault repo, which agentdesk never commits or pushes (see
agentdesk/vault.py's own docstring for why: that authorship convention is
John's to make). One tool, two repos, each holding what belongs to it.

Auto-bootstrapping: `ensure_index()` builds the index from nothing the first
time it is asked for one, and refreshes it (cheap: only changed notes are
re-embedded, see `_build_or_refresh`) every time after. This is what closes
the gap that motivated pulling this in-repo in the first place -- a wiki post
mirrored into the vault by vault.py did not appear in search results until
someone remembered to run the old index_vault.py by hand, and nothing ever
reminded them. A caller of `search()` here never sees a stale index.

Hybrid, not pure semantic: this vault is dense with exact identifiers (agent
UUIDs, DSR/PBI numbers, commit SHAs, hostnames, flags like -Xmx20g).
Embeddings are weakest exactly there, so lexical and semantic rankings are
fused with reciprocal rank fusion, weighted toward the lexical side when the
query contains a rare term.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Optional

import numpy as np

from . import paths

VAULT = paths.VAULT_DIR
INDEX_DIR = paths.VAULT_SEARCH_DIR
VEC_PATH = INDEX_DIR / "vault_vectors.npz"
META_PATH = INDEX_DIR / "vault_meta.json"

OLLAMA = "http://127.0.0.1:11434/api/embed"
MODEL = "nomic-embed-text"

# Off: embeddings come from Ollama, and John does not want Ollama used
# (2026-09-22). With it off, search() is lexical-only -- exact-term matching
# over notes/, which needs no index and no model. That is the half that finds
# identifiers (UUIDs, PBI numbers, hostnames); what is lost is recall on a
# question worded differently from the note. The index code stays so turning
# this back on is one line.
SEMANTIC_ENABLED = False

# Only notes/ get embedded -- MOCs are navigation, not the fact itself, and
# long enough to exceed the embedding model's practical context (the 8.3 KB
# GoCD MOC hung the endpoint outright, measured). _meta/ is boilerplate and
# state, log/ is an audit trail: neither is memory to search.
GLOBS = ("notes/*.md",)

# nomic-embed-text handles ~2048 tokens; stay well inside it. Notes cap at
# ~420 words by protocol, so this only ever bites on an outlier.
MAX_EMBED_CHARS = 6000

RRF_K = 60  # standard RRF damping: rank 1 -> 1/61, rank 10 -> 1/70


class VaultSearchUnavailable(RuntimeError):
    """Ollama is not reachable, or the vault has no notes yet.

    A distinct type rather than letting urllib's own exception surface, so a
    caller (the MCP tool, the CLI) can give a message that names the actual
    fix -- start Ollama -- instead of a raw connection-refused traceback.
    """


def _embed(text: str, model: str = MODEL, retries: int = 3) -> np.ndarray:
    payload = json.dumps({"model": model, "input": text[:MAX_EMBED_CHARS]}).encode()
    req = urllib.request.Request(
        OLLAMA, data=payload, headers={"Content-Type": "application/json"})
    last: Optional[Exception] = None
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                v = np.asarray(json.loads(r.read())["embeddings"][0],
                               dtype=np.float32)
            n = float(np.linalg.norm(v))
            return v / n if n else v
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            last = exc
            time.sleep(1 + attempt * 2)
    raise VaultSearchUnavailable(
        f"could not reach Ollama at {OLLAMA} to embed a query/note "
        f"(tried {retries}x): {last!r}. Start Ollama and try again.")


def _parse_note(path: Path) -> dict:
    """Split a note into frontmatter fields and body."""
    raw = path.read_text(encoding="utf-8", errors="replace")
    summary, ntype, tags = "", "note", ""
    body = raw
    if raw.startswith("---"):
        end = raw.find("\n---", 3)
        if end > 0:
            fm, body = raw[3:end], raw[end + 4:]
            for key, dest in (("summary", "summary"), ("type", "ntype"),
                             ("tags", "tags")):
                m = re.search(rf"^{key}:\s*(.*)$", fm, re.M)
                if m:
                    val = m.group(1).strip()
                    if dest == "summary":
                        summary = val
                    elif dest == "ntype":
                        ntype = val
                    else:
                        tags = val
    return {"title": path.stem, "summary": summary, "type": ntype,
           "tags": tags, "body": body.strip(), "raw": raw}


def _embed_text(rec: dict) -> str:
    """What actually gets embedded. Title and summary repeated ahead of the
    body deliberately: they are the most information-dense part of a note,
    and nomic-embed-text truncates long inputs, so front-loading them
    protects recall on the longest notes."""
    return f"{rec['title']}\n{rec['summary']}\n{rec['tags']}\n\n{rec['body']}"


def _build_or_refresh(rebuild: bool = False,
                      progress_cb=None) -> dict:
    """(Re)build the index, reusing every unchanged note's vector.

    Returns the same summary dict `main()` prints, so a caller (the MCP tool,
    a future GUI panel) can report it without parsing stdout.
    """
    files = sorted(f for g in GLOBS for f in VAULT.glob(g))
    if not files:
        raise VaultSearchUnavailable(
            f"no notes found under {VAULT / 'notes'} -- is the vault path "
            "right, and has anything been written to it yet?")

    old_meta: dict = {}
    old_vecs: dict[str, np.ndarray] = {}
    if not rebuild and META_PATH.exists() and VEC_PATH.exists():
        old_meta = json.loads(META_PATH.read_text(encoding="utf-8"))
        z = np.load(VEC_PATH)
        keys = list(old_meta.get("keys", []))
        arr = z["vectors"]
        old_vecs = {k: arr[i] for i, k in enumerate(keys)}

    old_hashes = old_meta.get("hashes", {})
    keys, rows, meta_rows, hashes = [], [], {}, {}
    reused = embedded = 0

    for p in files:
        rel = p.relative_to(VAULT).as_posix()
        rec = _parse_note(p)
        h = hashlib.sha256(rec["raw"].encode("utf-8", "replace")).hexdigest()[:16]
        hashes[rel] = h
        if rel in old_vecs and old_hashes.get(rel) == h:
            vec = old_vecs[rel]
            reused += 1
        else:
            vec = _embed(_embed_text(rec))
            embedded += 1
            if progress_cb is not None:
                progress_cb(embedded, len(files))
        keys.append(rel)
        rows.append(vec)
        meta_rows[rel] = {"title": rec["title"], "summary": rec["summary"],
                          "type": rec["type"], "tags": rec["tags"],
                          "words": len(rec["body"].split())}

    mat = np.vstack(rows).astype(np.float32)
    INDEX_DIR.mkdir(parents=True, exist_ok=True)
    np.savez_compressed(VEC_PATH, vectors=mat)
    META_PATH.write_text(
        json.dumps({"model": MODEL, "dims": int(mat.shape[1]), "keys": keys,
                   "meta": meta_rows, "hashes": hashes,
                   "built": time.strftime("%Y-%m-%dT%H:%M:%S")}, indent=1),
        encoding="utf-8")
    return {"docs": len(keys), "embedded": embedded, "reused": reused,
           "store_mb": round(VEC_PATH.stat().st_size / 1e6, 2)}


def ensure_index() -> None:
    """Build the index if it does not exist yet; otherwise leave it -- the
    incremental refresh in search() is what keeps it current on every call,
    so this only ever does real work on a machine that has never searched."""
    if not (VEC_PATH.exists() and META_PATH.exists()):
        _build_or_refresh(rebuild=False)


def _lexical(query: str, limit: int = 40) -> tuple[list[str], int]:
    """Rank notes by how many distinct query terms they contain.

    Pure Python on purpose: the whole vault's notes/ is under 1 MB, so
    scanning it costs milliseconds, and shelling out to ripgrep meant that if
    `rg` was missing from PATH this returned nothing and the hybrid silently
    degraded to semantic-only -- exactly where exact identifiers get lost.
    """
    terms = {t.lower() for t in re.findall(r"[A-Za-z0-9_.\-]{3,}", query)}
    if not terms:
        return [], 0
    per_file: dict[str, int] = {}
    totals: dict[str, int] = {}
    term_docs: dict[str, int] = {}
    # Word-boundary patterns, not substring counts: matching "name" inside
    # "filename" or "renames" buried the note that answered the query. A \b
    # pattern still matches an id inside a UUID, because "-" is a boundary.
    pats = {t: re.compile(r"\b" + re.escape(t)) for t in terms}
    for p in VAULT.glob("notes/*.md"):
        text = p.read_text(encoding="utf-8", errors="replace").lower()
        hit = tot = 0
        for t in terms:
            c = len(pats[t].findall(text))
            if c:
                hit += 1
                tot += c
                term_docs[t] = term_docs.get(t, 0) + 1
        if hit:
            rel = p.relative_to(VAULT).as_posix()
            per_file[rel] = hit
            totals[rel] = tot
    ranked = sorted(per_file, key=lambda f: (-per_file[f], -totals[f], f))
    # Rarest matched term's document frequency. A term in <=3 notes is almost
    # certainly an identifier (UUID, DSR number, SHA, flag) -- exactly where
    # embeddings are useless and an exact match is authoritative.
    rarest = min((term_docs[t] for t in terms if term_docs.get(t)), default=0)
    return ranked[:limit], rarest


def _rrf(weighted: list[tuple[list[str], float]]) -> dict[str, float]:
    fused: dict[str, float] = {}
    for ranking, w in weighted:
        for rank, key in enumerate(ranking, start=1):
            fused[key] = fused.get(key, 0.0) + w / (RRF_K + rank)
    return fused


def search(query: str, k: int = 10, *, semantic_only: bool = False,
          lexical_only: bool = False, refresh: bool = True) -> list[dict]:
    """The one entry point everything else (CLI, MCP tool) calls.

    `refresh=True` (the default) runs the same incremental pass `main()`'s
    `--rebuild` triggers in full, but cheap: only notes changed since the
    last call are re-embedded. This is what guarantees a hit list never
    reflects a stale index -- see the module docstring. Pass False only for
    a caller that has already refreshed in the same batch and wants to avoid
    re-hashing every note per query.
    """
    if not SEMANTIC_ENABLED:
        lex, _rarest = _lexical(query)
        return [_hit(key, 1.0 / (RRF_K + i), None)
                for i, key in enumerate(lex[:k], 1)]

    if refresh:
        _build_or_refresh(rebuild=False)
    elif not (VEC_PATH.exists() and META_PATH.exists()):
        raise VaultSearchUnavailable(
            "no index yet -- call ensure_index() or search(refresh=True) "
            "once before refresh=False can be used")

    meta = json.loads(META_PATH.read_text(encoding="utf-8"))
    keys: list[str] = meta["keys"]
    mat = np.load(VEC_PATH)["vectors"]
    model = meta.get("model", MODEL)

    lex, rarest = ([], 0) if semantic_only else _lexical(query)
    sem: list[str] = []
    if not lexical_only:
        q = _embed(query, model)
        scores = mat @ q  # vectors are pre-normalised: dot product == cosine
        sem = [keys[i] for i in np.argsort(-scores)]

    # A rare matched term is almost certainly an identifier: trust the exact
    # match over the embedding.
    lex_w = 4.0 if 0 < rarest <= 3 else (2.0 if 0 < rarest <= 10 else 1.0)

    if lexical_only:
        fused = {kk: 1.0 / (RRF_K + i) for i, kk in enumerate(lex, 1)}
    elif semantic_only:
        fused = {kk: 1.0 / (RRF_K + i) for i, kk in enumerate(sem[:40], 1)}
    else:
        fused = _rrf([(sem[:40], 1.0), (lex, lex_w)])

    top = sorted(fused, key=lambda kk: -fused[kk])[:k]
    return [_hit(key, fused[key], meta["meta"].get(key, {})) for key in top]


def _hit(key: str, score: float, meta: Optional[dict]) -> dict:
    """One result row. `meta` is the index's entry for the note; None means
    there is no index in play (lexical-only), so read the frontmatter directly."""
    if meta is None:
        rec = _parse_note(VAULT / key)
        meta = {"type": rec["type"], "summary": rec["summary"]}
    return {"path": key, "score": round(score, 4),
            "type": meta.get("type", "?"),
            "summary": meta.get("summary", "") or "(no summary)"}


def read_note(rel_path: str) -> str:
    """The body of one hit, by the `path` search() returned. Kept separate
    from search() itself so a caller only pays for the notes it actually
    opens -- the same reasoning the old script's --full N flag was for."""
    p = VAULT / rel_path
    if not p.is_relative_to(VAULT):  # defence, not expected: rel_path is
        raise ValueError(f"{rel_path!r} escapes the vault")
    return p.read_text(encoding="utf-8", errors="replace").strip()


def main(argv: Optional[list[str]] = None) -> int:
    if hasattr(sys.stdout, "reconfigure"):
        # Windows consoles default to cp1252 and blow up on the em-dashes and
        # arrows that are all over the vault's summaries.
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    ap = argparse.ArgumentParser(prog="agentdesk vault-search")
    ap.add_argument("query", nargs="?")
    ap.add_argument("-k", type=int, default=10, help="hits to print (default 10)")
    ap.add_argument("--semantic-only", action="store_true")
    ap.add_argument("--lexical-only", action="store_true")
    ap.add_argument("--full", type=int, default=0,
                    help="also print the body of the top N hits")
    ap.add_argument("--rebuild", action="store_true",
                    help="discard the index and re-embed every note")
    args = ap.parse_args(argv)

    try:
        if args.rebuild:
            summary = _build_or_refresh(rebuild=True)
            print(f"indexed {summary['docs']} docs ({summary['embedded']} "
                  f"embedded, {summary['reused']} reused) "
                  f"store={summary['store_mb']} MB")
            if not args.query:
                return 0
        if not args.query:
            ap.error("query is required unless --rebuild is the only thing asked for")
        hits = search(args.query, k=args.k, semantic_only=args.semantic_only,
                     lexical_only=args.lexical_only)
    except VaultSearchUnavailable as exc:
        print(str(exc), file=sys.stderr)
        return 1

    if not hits:
        print("(no hits)")
        return 0
    for hit in hits:
        print(f"{hit['score']:.4f}  [{hit['type']}] {hit['path']}\n"
              f"         {hit['summary']}")
    for hit in hits[:args.full]:
        print(f"\n----- {hit['path']} -----")
        print(read_note(hit["path"]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
