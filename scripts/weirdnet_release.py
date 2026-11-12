#!/usr/bin/env python3
"""The shared WeirdBrains release script (weirdnet-project-helpers).

    release.py [cut] [--major|--minor|--patch|--version X.Y.Z] [--dry-run] [--yes]
               [--no-push] [--no-fetch] [--skip-checks] [--skip-hooks] [--skip-crates]
               [--expect-head SHA] [--date YYYY-MM-DD] [--section NAME]
               [--allow-empty-changelog] [MESSAGE ...]
    release.py validate                    read release.toml and plan the next bump; change nothing
    release.py crates [--tag vX.Y.Z] [--dry-run] [--yes]
    release.py taur VERSION [--package NAME] [--dry-run] [--yes] [--no-verify-sources]
    release.py taur-srcinfo [DIR]          write DIR/.SRCINFO from DIR/PKGBUILD
    release.py taur-check VERSION [DIR]    is the recipe ready for TAUR
    release.py assets VERSION              check the GitHub release has every [[platforms]] asset
    release.py version                     print this script's version

`cut` (the default) follows the collective AGENTS.md order: preflight, bump every version
file in release.toml, move the changelog, run hooks and checks, commit `release: vX.Y.Z`,
annotated tag `vX.Y.Z`, one atomic push of main and that tag (never forced), then publish
affected public crates to crates.io and print the project's next steps (TAUR and so on).
MINOR is the default bump. Python 3.9+, standard library only (3.11+ reads release.toml with
tomllib; older Pythons use a small built-in parser for the subset it needs). See README.md.
"""
from __future__ import annotations

import argparse
import datetime
import difflib
import fnmatch
import glob as globmod
import gzip
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

try:
    import tomllib
except ImportError:  # Python < 3.11 (e.g. Ubuntu 22.04 runners): mini_toml() below
    tomllib = None

__version__ = "0.1.1"

CONFIG = "release.toml"
ORDER = ["Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"]
SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$")
PLACEHOLDER = re.compile(r"\{(version|old|tag|old_tag|date|title|title_cell|build|project|work)\}")
HEX64 = re.compile(r"^[0-9a-f]{64}$")


class ReleaseError(Exception):
    pass


def out(msg=""):
    print(msg, flush=True)


# ---- versions ------------------------------------------------------------------------------

def parse_version(v):
    m = SEMVER.match(str(v).strip())
    if not m:
        raise ReleaseError(f"version {v!r} is not MAJOR.MINOR.PATCH (no v prefix, prerelease or build)")
    return tuple(int(x) for x in m.groups())


def bump(version, part):
    ma, mi, pa = parse_version(version)
    if part == "major":
        return f"{ma + 1}.0.0"
    if part == "minor":
        return f"{ma}.{mi + 1}.0"
    if part == "patch":
        return f"{ma}.{mi}.{pa + 1}"
    raise ReleaseError(f"unknown bump {part!r}")


def today(tz="America/New_York"):
    from zoneinfo import ZoneInfo
    return datetime.datetime.now(ZoneInfo(tz)).date().isoformat()


def fill(template, ctx, regex=False):
    """Replace {version}, {old}, {tag}, {date}, {title}, ... and nothing else, so regex
    quantifiers like \\d{4} survive. With regex=True the values are re.escape()d."""
    def one(m):
        v = str(ctx.get(m.group(1), m.group(0)))
        return re.escape(v) if regex else v
    return PLACEHOLDER.sub(one, template)


# ---- config --------------------------------------------------------------------------------

def mini_toml(text):
    """The TOML subset release.toml uses, for Python < 3.11: [table], [[array.of.tables]],
    key = "basic" / 'literal' strings, integers, booleans and (multi-line) arrays of those."""
    root = {}
    cur = root
    toks = _toml_tokens(text)
    i = 0

    def value():
        nonlocal i
        kind, v = toks[i]
        i += 1
        if kind == "[":
            arr = []
            while toks[i][0] != "]":
                if toks[i][0] == ",":
                    i += 1
                    continue
                arr.append(value())
            i += 1
            return arr
        if kind in ("str", "int", "bool"):
            return v
        raise ReleaseError(f"release.toml: unsupported value near {v!r} (use Python 3.11+ for full TOML)")

    def table(path, array):
        t = root
        for k in path[:-1]:
            t = t.setdefault(k, {})
            if isinstance(t, list):
                t = t[-1]
        last = path[-1]
        if array:
            t.setdefault(last, []).append({})
            return t[last][-1]
        return t.setdefault(last, {})

    while i < len(toks):
        kind, v = toks[i]
        if kind == "hdr":
            cur = table(v[0], v[1])
            i += 1
        elif kind == "key":
            if i + 1 >= len(toks) or toks[i + 1][0] != "=":
                raise ReleaseError(f"release.toml: expected = after {v!r}")
            i += 2
            cur[v] = value()
        else:
            raise ReleaseError(f"release.toml: unexpected {v!r}")
    return root


def _toml_tokens(text):
    toks, i, n, line_start, depth = [], 0, len(text), True, 0
    q3s, q3d = "'" * 3, '"' * 3
    while i < n:
        c = text[i]
        if c == "\n":
            line_start = True
            i += 1
        elif c in " \t\r":
            i += 1
        elif c == "#":
            while i < n and text[i] != "\n":
                i += 1
        elif line_start and depth == 0 and c == "[":
            arr = text.startswith("[[", i)
            j = text.index("]]" if arr else "]", i)
            toks.append(("hdr", ([k.strip().strip('"') for k in text[i + (2 if arr else 1):j].split(".")], arr)))
            i = j + (2 if arr else 1)
            line_start = False
        elif line_start and depth == 0 and (c.isalnum() or c in '_-"'):
            m = re.match(r'"([^"]+)"|([A-Za-z0-9_\-]+)', text[i:])
            toks.append(("key", m.group(1) or m.group(2)))
            i += m.end()
            line_start = False
        elif c == "=":
            toks.append(("=", "="))
            line_start = False
            i += 1
        elif c in "[],":
            depth += {"[": 1, "]": -1}.get(c, 0)
            toks.append((c, c))
            line_start = False
            i += 1
        elif text.startswith(q3s, i) or text.startswith(q3d, i):
            q = text[i:i + 3]
            j = text.index(q, i + 3)
            body = text[i + 3:j]
            toks.append(("str", body[1:] if body.startswith("\n") else body))
            i = j + 3
        elif c == "'":
            j = text.index("'", i + 1)
            toks.append(("str", text[i + 1:j]))
            i = j + 1
        elif c == '"':
            j, buf = i + 1, []
            while text[j] != '"':
                if text[j] == "\\":
                    e = text[j + 1]
                    buf.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\"}.get(e, "\\" + e))
                    j += 2
                else:
                    buf.append(text[j])
                    j += 1
            toks.append(("str", "".join(buf)))
            i = j + 1
        elif text.startswith("true", i) or text.startswith("false", i):
            b = text.startswith("true", i)
            toks.append(("bool", b))
            i += 4 if b else 5
        else:
            m = re.match(r"[+-]?\d+", text[i:])
            if not m:
                raise ReleaseError(f"release.toml: can't parse near {text[i:i + 30]!r}")
            toks.append(("int", int(m.group(0))))
            i += m.end()
    return toks


def parse_toml(text):
    if tomllib is not None and not os.environ.get("WEIRDNET_RELEASE_MINI_TOML"):
        try:
            return tomllib.loads(text)
        except tomllib.TOMLDecodeError as e:
            raise ReleaseError(f"release.toml: {e}")
    return mini_toml(text)


def load_config(root, path=None):
    p = Path(path) if path else Path(root) / CONFIG
    if not p.is_file():
        raise ReleaseError(f"no {p.name} in {p.parent} (see weirdnet-project-helpers/README.md)")
    cfg = parse_toml(p.read_text(encoding="utf-8"))
    want = cfg.get("helpers")
    if want and want != __version__:
        raise ReleaseError(f"{p.name} wants weirdnet-project-helpers {want}, this script is {__version__}; "
                           "re-vendor it with weirdnet-project-helpers/vendor.py")
    if not cfg.get("files"):
        raise ReleaseError(f"{p.name} has no [[files]] entries (the version files)")
    cfg.setdefault("project", Path(root).name)
    cfg.setdefault("branch", "main")
    cfg.setdefault("remote", "origin")
    cfg.setdefault("commit_message", "release: {tag}")
    cfg.setdefault("tag_message", "{tag}")
    cfg.setdefault("timezone", "America/New_York")
    return cfg


def apply_env(cfg):
    """[env] sets variables only when they are not set already (e.g. CARGO_TARGET_DIR)."""
    env = dict(os.environ)
    for k, v in (cfg.get("env") or {}).items():
        env.setdefault(k, str(v))
    return env


# ---- version files -------------------------------------------------------------------------

def _table(text, header):
    """(start, end) of the body of `[header]` in a TOML file, or None."""
    m = re.search(r"^\[" + re.escape(header) + r"\][ \t]*\n", text, re.M)
    if not m:
        return None
    n = re.search(r"^\[", text[m.end():], re.M)
    return m.end(), (m.end() + n.start() if n else len(text))


def _table_version(text, header):
    span = _table(text, header)
    if not span:
        return None
    body = text[span[0]:span[1]]
    if re.search(r"^version(\.workspace\s*=\s*true|\s*=\s*\{\s*workspace\s*=\s*true\s*\})", body, re.M):
        return "workspace"
    v = re.search(r"^version\s*=\s*\"([^\"]+)\"", body, re.M)
    return v.group(1) if v else None


def _set_table_version(text, header, old, new, what):
    span = _table(text, header)
    if not span:
        raise ReleaseError(f"{what}: no [{header}] table")
    body = text[span[0]:span[1]]
    nb, n = re.subn(r"^(version\s*=\s*\")" + re.escape(old) + r"\"", lambda m: m.group(1) + new + '"', body, count=1, flags=re.M)
    if n != 1:
        raise ReleaseError(f"{what}: [{header}] version is not {old}")
    return text[:span[0]] + nb + text[span[1]:]


def package_name(text):
    span = _table(text, "package")
    if not span:
        return None
    m = re.search(r"^name\s*=\s*\"([^\"]+)\"", text[span[0]:span[1]], re.M)
    return m.group(1) if m else None


def publish_flag(text):
    span = _table(text, "package")
    if not span:
        return True
    m = re.search(r"^publish\s*=\s*(false|true|\[[^\]]*\])", text[span[0]:span[1]], re.M)
    return not (m and m.group(1) in ("false", "[]"))


def members(cargo_toml):
    m = re.search(r"^members\s*=\s*\[(.*?)\]", cargo_toml, re.M | re.S)
    if not m:
        return []
    return re.findall(r"\"([^\"]+)\"", re.sub(r"#[^\n]*", "", m.group(1)))


def bump_lock(text, names, new):
    done = set()

    def one(m):
        if m.group(2) in names and not m.group(4):
            done.add(m.group(2))
            return m.group(1) + new + '"' + m.group(3)
        return m.group(0)
    res = re.sub(r"(\[\[package\]\]\nname = \"([^\"]+)\"\nversion = \")[^\"]+\"(\n(source = )?)", one, text)
    return res, done


class Plan:
    """Every file the release rewrites, in memory; nothing is written until apply()."""

    def __init__(self, root):
        self.root = Path(root)
        self.edits = {}      # rel -> (old text or None, new text or None)
        self.stage_all = []  # paths staged with `git add -A` (moves)
        self.lock_names = set()
        self.build = None

    def read(self, rel):
        rel = str(rel)
        if rel in self.edits:
            return self.edits[rel][1]
        return (self.root / rel).read_text(encoding="utf-8")

    def put(self, rel, new):
        rel = str(rel)
        old = self.edits[rel][0] if rel in self.edits else ((self.root / rel).read_text(encoding="utf-8") if (self.root / rel).exists() else None)
        if old == new:
            self.edits.pop(rel, None)
        else:
            self.edits[rel] = (old, new)

    def paths(self, pattern, ctx, optional=False):
        pat = fill(pattern, ctx)
        if any(c in pat for c in "*?["):
            hits = sorted(Path(p).relative_to(self.root).as_posix() for p in globmod.glob(str(self.root / pat)) if Path(p).is_file())
        else:
            hits = [pat] if (self.root / pat).is_file() else []
        if not hits and not optional:
            raise ReleaseError(f"{pat}: no such file (mark it optional = true if it may be missing)")
        return hits


def read_current(root, cfg):
    """The project version from the primary [[files]] entry (primary = true, else the first)."""
    files = cfg["files"]
    e = next((f for f in files if f.get("primary")), files[0])
    kind, path = e.get("kind"), e.get("path") or (e.get("paths") or [None])[0]
    text = (Path(root) / path).read_text(encoding="utf-8")
    if kind == "cargo-workspace":
        v = _table_version(text, "workspace.package")
    elif kind == "cargo-package":
        v = _table_version(text, "package")
    elif kind == "pubspec":
        m = re.search(r"^version:\s*([0-9.]+)(\+\d+)?\s*$", text, re.M)
        v = m.group(1) if m else None
    elif kind == "regex" and e.get("read"):
        m = re.search(e["read"], text, re.M)
        v = m.group("v") if m else None
    else:
        raise ReleaseError(f"the primary [[files]] entry ({kind}) can't give the version; mark a cargo or pubspec entry primary")
    if not v or v == "workspace":
        raise ReleaseError(f"{path}: no version found for kind {kind}")
    parse_version(v)
    return v


def plan_files(root, cfg, ctx):
    plan = Plan(root)
    old, new = ctx["old"], ctx["version"]
    for e in cfg["files"]:
        kind = e.get("kind")
        opt = bool(e.get("optional"))
        pats = e.get("paths") or ([e["path"]] if "path" in e else [])
        if kind == "cargo-workspace":
            rel = pats[0] if pats else "Cargo.toml"
            text = plan.read(rel)
            if _table_version(text, "workspace.package") != old:
                raise ReleaseError(f"{rel}: [workspace.package] version is not {old}")
            text = _set_table_version(text, "workspace.package", old, new, rel)
            name = package_name(text)
            if name and _table_version(text, "package") in ("workspace", old):
                if _table_version(text, "package") == old:
                    text = _set_table_version(text, "package", old, new, rel)
                plan.lock_names.add(name)
            plan.put(rel, text)
            follow = set(e.get("follow", []))
            base = Path(rel).parent
            for mem in members(text) if e.get("members", "matching") != "none" else []:
                mrel = (base / mem / "Cargo.toml").as_posix()
                if not (plan.root / mrel).is_file():
                    continue
                mt = plan.read(mrel)
                v, nm = _table_version(mt, "package"), package_name(mt)
                if v == "workspace":
                    plan.lock_names.add(nm)
                elif v and (v == old or mem in follow):
                    plan.put(mrel, _set_table_version(mt, "package", v, new, mrel))
                    plan.lock_names.add(nm)
        elif kind == "cargo-package":
            for pat in pats:
                for rel in plan.paths(pat, ctx, opt):
                    t = plan.read(rel)
                    v = _table_version(t, "package")
                    if v != old:
                        raise ReleaseError(f"{rel} is at {v}, expected {old} (versions out of step)")
                    plan.put(rel, _set_table_version(t, "package", old, new, rel))
                    plan.lock_names.add(package_name(t))
        elif kind == "cargo-lock":
            names = e.get("packages", "auto")
            names = plan.lock_names if names == "auto" else set(names) | (plan.lock_names if e.get("include_auto") else set())
            for pat in pats or ["Cargo.lock"]:
                for rel in plan.paths(pat, ctx, opt):
                    t, done = bump_lock(plan.read(rel), names, new)
                    if not done and not opt:
                        raise ReleaseError(f"{rel}: none of {sorted(names)} found")
                    missing = set(e["packages"]) - done if isinstance(e.get("packages"), list) else set()
                    if missing:
                        raise ReleaseError(f"{rel}: missing {sorted(missing)}")
                    plan.put(rel, t)
        elif kind == "pubspec":
            mode = e.get("build", "keep")
            rels = [r for pat in pats for r in plan.paths(pat, ctx, opt)]
            builds = {}
            for rel in rels:
                m = re.search(r"^version:\s*" + re.escape(old) + r"(?:\+(\d+))?[ \t]*$", plan.read(rel), re.M)
                if not m:
                    raise ReleaseError(f"{rel}: expected 'version: {old}[+N]'")
                builds[rel] = m.group(1)
            if mode == "increment":
                got = set(builds.values())
                if None in got or len(got) != 1:
                    raise ReleaseError(f"pubspec build numbers differ or are missing: {builds}")
                plan.build = int(got.pop()) + 1
                ctx["build"] = plan.build
            for rel in rels:
                b = builds[rel]
                suffix = f"+{plan.build}" if mode == "increment" else (f"+{b}" if b and mode == "keep" else "")
                plan.put(rel, re.sub(r"^version:\s*" + re.escape(old) + r"(?:\+\d+)?[ \t]*$", f"version: {new}{suffix}", plan.read(rel), count=1, flags=re.M))
        elif kind == "regex":
            pattern, repl = e["pattern"], e["replace"]
            count = int(e.get("count", 1))
            for pat in pats:
                for rel in plan.paths(pat, ctx, opt):
                    rx = re.compile(fill(pattern, ctx, regex=True), re.M)
                    text = plan.read(rel)
                    hits = len(rx.findall(text))
                    if count and hits != count and not opt:
                        raise ReleaseError(f"{rel}: expected {count} match(es) of {pattern!r}, found {hits}")
                    if not hits and not opt:
                        raise ReleaseError(f"{rel}: no match for {pattern!r}")
                    r = fill(repl, ctx).replace("\\", "\\\\").replace("\\\\g<", "\\g<")
                    plan.put(rel, rx.sub(lambda m: m.expand(r), text, count=count if count else 0))
        elif kind == "replace":
            for pat in pats:
                for rel in plan.paths(pat, ctx, opt):
                    text = plan.read(rel)
                    cur = old
                    if e.get("detect"):
                        m = re.search(e["detect"], text, re.M)
                        if not m:
                            if opt:
                                continue
                            raise ReleaseError(f"{rel}: no match for detect {e['detect']!r}")
                        cur = m.group("v")
                    if cur == new:
                        continue
                    if cur not in text and not opt:
                        raise ReleaseError(f"{rel} does not contain {cur}")
                    plan.put(rel, text.replace(cur, new))
        elif kind == "insert-after":
            for pat in pats:
                for rel in plan.paths(pat, ctx, opt):
                    lines = plan.read(rel).split("\n")
                    rx = re.compile(fill(e["pattern"], ctx, regex=True))
                    if e.get("exists") and any(re.search(fill(e["exists"], ctx, regex=True), l) for l in lines):
                        raise ReleaseError(f"{rel}: already has a line for {new}")
                    idx = [i for i, l in enumerate(lines) if rx.search(l)]
                    if len(idx) != 1:
                        if opt:
                            continue
                        raise ReleaseError(f"{rel}: expected exactly one line matching {e['pattern']!r}, found {len(idx)}")
                    lines.insert(idx[0] + 1, fill(e["line"], ctx))
                    plan.put(rel, "\n".join(lines))
        elif kind == "move":
            src, dst = fill(e["from"], ctx), fill(e["to"], ctx)
            sd = plan.root / src
            if not sd.is_dir():
                if opt:
                    continue
                raise ReleaseError(f"{src}: no such directory to move")
            if (plan.root / dst).exists():
                raise ReleaseError(f"{dst} already exists")
            for f in sorted(p for p in sd.rglob("*") if p.is_file()):
                r_old = f.relative_to(plan.root).as_posix()
                r_new = (Path(dst) / f.relative_to(sd)).as_posix()
                t = f.read_text(encoding="utf-8")
                plan.edits[r_old] = (t, None)
                plan.edits[r_new] = (None, t.replace(old, new) if e.get("replace_contents", True) else t)
            plan.stage_all += [src, dst]
        else:
            raise ReleaseError(f"unknown [[files]] kind {kind!r}")
    return plan


# ---- changelog -----------------------------------------------------------------------------

def _blocks(text):
    pre, heads, cur, item = [], {}, None, None

    def close():
        nonlocal item
        if item is not None:
            body = "\n".join(item).rstrip()
            if body:
                (pre if cur is None else heads[cur]).append(body)
        item = None

    for line in text.splitlines():
        h = re.match(r"^###\s+(.+?)\s*$", line)
        if h:
            close()
            cur = h.group(1)
            heads.setdefault(cur, [])
        elif re.match(r"^[-*]\s+", line):
            close()
            item = ["- " + re.sub(r"^[-*]\s+", "", line)]
        elif not line.strip():
            if item is not None:
                item.append("")
        elif item is None:
            item = [line]
        else:
            item.append(line)
    close()
    return pre, heads


def message_blocks(messages, section):
    """Release messages as changelog items. `### Heading` blocks and `- ` bullets are used as
    written; `Fixed: text` / `Changed: text` pick a section; plain text is one bullet."""
    heads = {}
    for msg in messages:
        text = msg.strip()
        if not text:
            continue
        if re.search(r"^###\s", text, re.M) or re.match(r"^[-*]\s", text):
            pre, hs = _blocks(text)
            if pre:
                heads.setdefault(section, []).extend(pre)
            for h, items in hs.items():
                heads.setdefault(h, []).extend(items)
            continue
        m = re.match(r"^(" + "|".join(ORDER) + r"):\s+(.*)$", text, re.S)
        sec, body = (m.group(1), m.group(2)) if m else (section, text)
        body = " ".join(body.split())
        heads.setdefault(sec, []).append("- " + body[:1].upper() + body[1:])
    return heads


def _merge(old, new):
    res = {h: list(v) for h, v in old.items()}
    for h, items in new.items():
        dst = res.setdefault(h, [])
        dst.extend(i for i in items if i not in dst)
    rest = sorted((h for h in res if h not in old), key=lambda h: ORDER.index(h) if h in ORDER else len(ORDER))
    return {h: res[h] for h in list(old) + rest if res[h]}


def rewrite_changelog(text, cl, ctx, messages, section, allow_empty=False):
    style = cl.get("style", "keepachangelog")
    unrel = cl.get("unreleased", "## [Unreleased]" if style == "keepachangelog" else "## Unreleased")
    heading = fill(cl.get("heading", "## [{version}] - {date}" if style == "keepachangelog" else "## v{version} ({date})"), ctx)
    m = re.search(r"^" + re.escape(unrel) + r"[ \t]*\n", text, re.M)
    if not m:
        # No Unreleased heading yet: start an empty one above the first release section.
        first = re.search(r"^## ", text, re.M)
        at = first.start() if first else len(text)
        text = text[:at] + (unrel + "\n\n") + text[at:]
        m = re.search(r"^" + re.escape(unrel) + r"[ \t]*\n", text, re.M)
    if re.search(r"^" + re.escape(heading.split(" - ")[0].split(" (")[0]) + r"(\s|$)", text, re.M):
        raise ReleaseError(f"the changelog already has a {ctx['version']} section")
    nxt = re.search(r"^## ", text[m.end():], re.M)
    end = m.end() + nxt.start() if nxt else len(text)
    body = text[m.end():end]
    if style == "raw":
        notes = body.strip("\n")
        if not notes.strip() and not allow_empty:
            raise ReleaseError(f"`{unrel}` is empty: add notes, or pass --allow-empty-changelog")
        lines = [heading] + [fill(l, ctx) for l in cl.get("intro", [])]
        if notes.strip():
            lines.append(notes)
        block = f"{unrel}\n\n" + "\n".join(lines) + "\n\n"
    else:
        pre, heads = _blocks(body)
        heads = _merge(heads, message_blocks(messages, section))
        if not pre and not any(heads.values()) and not allow_empty:
            raise ReleaseError(f"no changelog notes: add them under `{unrel}` or pass a message")
        parts = [f"{unrel}\n\n{heading}\n"]
        if pre:
            parts.append("\n" + "\n\n".join(pre) + "\n")
        for h, items in heads.items():
            parts.append(f"\n### {h}\n\n" + "\n".join(items) + "\n")
        block = "".join(parts) + ("\n" if nxt else "")
    res = text[:m.start()] + block + text[end:]
    url = cl.get("links")
    if url:
        url = url.rstrip("/")
        pfx = ctx.get("tag_prefix", "v")
        lm = re.search(r"^\[Unreleased\]: " + re.escape(url) + r"/compare/(\S+)\.\.\.HEAD$", res, re.M)
        if not lm:
            raise ReleaseError(f"changelog has no `[Unreleased]: {url}/compare/...HEAD` link")
        prev = lm.group(1)
        res = res[:lm.start()] + (f"[Unreleased]: {url}/compare/{ctx['tag']}...HEAD\n"
                                  f"[{ctx['version']}]: {url}/compare/{prev}...{ctx['tag']}") + res[lm.end():]
    return res


def changelog_section(text, ctx, cl):
    style = cl.get("style", "keepachangelog")
    heading = fill(cl.get("heading", "## [{version}] - {date}" if style == "keepachangelog" else "## v{version} ({date})"), ctx)
    i = text.find(heading)
    if i < 0:
        return ""
    n = re.search(r"^## ", text[i + len(heading):], re.M)
    return text[i:i + len(heading) + n.start() if n else len(text)].rstrip() + "\n"


# ---- git -----------------------------------------------------------------------------------

def git(root, *args, check=True, input=None):
    r = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, input=input)
    if check and r.returncode != 0:
        raise ReleaseError(f"git {' '.join(args)} failed: {(r.stderr or r.stdout).strip()}")
    return r


def tags(root, prefix="v"):
    got = []
    for t in git(root, "tag", "--list", f"{prefix}*").stdout.split():
        try:
            got.append((parse_version(t[len(prefix):]), t))
        except ReleaseError:
            pass
    return sorted(got)


def preflight(root, cfg, tag, new, expect_head, fetch):
    branch, remote = cfg["branch"], cfg["remote"]
    cur = git(root, "symbolic-ref", "--quiet", "--short", "HEAD", check=False).stdout.strip()
    if cur != branch:
        raise ReleaseError(f"not on {branch} (on {cur or 'a detached HEAD'}); releases are cut from {branch}")
    flags = ["--untracked-files=no"] if cfg.get("ignore_untracked") else []
    dirty = git(root, "status", "--porcelain", *flags).stdout.strip()
    if dirty:
        raise ReleaseError("the working tree is not clean (commit or stash first):\n" + dirty)
    if git(root, "diff", "--cached", "--quiet", check=False).returncode != 0:
        raise ReleaseError("staged changes are present")
    head = git(root, "rev-parse", "HEAD").stdout.strip()
    if expect_head and not head.startswith(expect_head.lower()):
        raise ReleaseError(f"HEAD is {head[:12]}, not --expect-head {expect_head}")
    if fetch:
        git(root, "fetch", "--quiet", remote, branch)
        if git(root, "ls-remote", "--tags", remote, f"refs/tags/{tag}").stdout.strip():
            raise ReleaseError(f"tag {tag} already exists on {remote}")
    rm = git(root, "rev-parse", "--verify", "--quiet", f"refs/remotes/{remote}/{branch}", check=False).stdout.strip()
    if rm and git(root, "merge-base", "--is-ancestor", rm, head, check=False).returncode != 0:
        raise ReleaseError(f"{branch} is behind or has diverged from {remote}/{branch}; pull (rebase) first")
    if git(root, "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}", check=False).returncode == 0:
        raise ReleaseError(f"tag {tag} already exists")
    pfx = cfg.get("tag_prefix", "v")
    have = tags(root, pfx)
    if have and parse_version(new) <= have[-1][0]:
        raise ReleaseError(f"{new} is not newer than the latest tag {have[-1][1]}")
    return head


def tracked(root, rel):
    return git(root, "ls-files", "--error-unmatch", "--", rel, check=False).returncode == 0


# ---- hooks and checks ----------------------------------------------------------------------

def run_cmds(root, cmds, ctx, env, label, dry):
    for c in cmds or []:
        cmd = fill(c, ctx)
        if dry:
            out(f"[dry-run] {label}: {cmd}")
            continue
        out(f"{label}: {cmd}")
        r = subprocess.run(["bash", "-c", cmd], cwd=root, env=env)
        if r.returncode != 0:
            raise ReleaseError(f"{label} failed ({r.returncode}): {cmd}")


def hook_env(env, ctx, dry):
    e = dict(env)
    for k in ("version", "old", "tag", "old_tag", "date", "title", "build", "project"):
        if ctx.get(k) is not None:
            e[f"RELEASE_{k.upper()}"] = str(ctx[k])
    e["RELEASE_DRY_RUN"] = "1" if dry else "0"
    return e


def confirm(question, yes):
    if yes:
        return True
    if not sys.stdin.isatty():
        raise ReleaseError("not a terminal: pass --yes to confirm")
    ans = input(f"{question} [Y/n] ").strip().lower()
    return ans in ("", "y", "yes")


# ---- crates.io -----------------------------------------------------------------------------

def crates_index(name):
    """Published versions of `name` from the crates.io sparse index (set of strings)."""
    base = os.environ.get("WEIRDNET_RELEASE_CRATES_INDEX", "https://index.crates.io")
    n = name.lower()
    pfx = {1: "1", 2: "2", 3: f"3/{n[0]}"}.get(len(n), f"{n[:2]}/{n[2:4]}")
    if "://" not in base:  # a local index directory (tests)
        f = Path(base) / pfx / n
        return {json.loads(l)["vers"] for l in f.read_text().splitlines() if l.strip()} if f.is_file() else set()
    try:
        with urllib.request.urlopen(f"{base}/{pfx}/{n}", timeout=30) as r:
            body = r.read().decode()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return set()
        raise ReleaseError(f"crates.io index for {name}: {e}")
    except OSError as e:
        raise ReleaseError(f"crates.io index for {name}: {e}")
    return {json.loads(l)["vers"] for l in body.splitlines() if l.strip()}


def crate_plan(root, cfg, tag, prev_tag):
    """[(path, name, version, action, reason)] for [crates] publish, in the configured order.
    publish = false crates are skipped; a version already on crates.io is skipped (with a warning
    when the crate changed since the previous tag, because it then needs a version bump)."""
    ws = (Path(root) / "Cargo.toml").read_text(encoding="utf-8") if (Path(root) / "Cargo.toml").is_file() else ""
    ws_ver = _table_version(ws, "workspace.package")
    res = []
    for rel in (cfg.get("crates") or {}).get("publish", []):
        mf = Path(root) / rel / "Cargo.toml"
        if not mf.is_file():
            raise ReleaseError(f"[crates] {rel}: no Cargo.toml")
        t = mf.read_text(encoding="utf-8")
        name, ver = package_name(t), _table_version(t, "package")
        ver = ws_ver if ver == "workspace" else ver
        if not publish_flag(t):
            res.append((rel, name, ver, "skip", "publish = false"))
            continue
        if prev_tag:
            changed = git(root, "diff", "--quiet", prev_tag, tag, "--", rel, check=False).returncode != 0
        else:
            changed = True
        have = crates_index(name)
        if ver in have:
            res.append((rel, name, ver, "skip", "already on crates.io" + (" (changed since " + prev_tag + ": bump its version)" if changed else "")))
        else:
            # A version crates.io doesn't have is always affected (its own or the workspace bump).
            res.append((rel, name, ver, "publish", f"{ver} not on crates.io yet"))
    return res


def publish_crates(root, cfg, env, tag, prev_tag, dry, yes):
    plan = crate_plan(root, cfg, tag, prev_tag)
    if not plan:
        return
    cargo = os.environ.get("WEIRDNET_RELEASE_CARGO", "cargo")
    todo = [p for p in plan if p[3] == "publish"]
    for rel, name, ver, act, why in plan:
        out(f"{'[dry-run] ' if dry else ''}crate {name} {ver}: {act} ({why})")
    if not todo or dry:
        for rel, name, ver, _, _ in todo:
            out(f"[dry-run] {cargo} publish --locked --manifest-path {rel}/Cargo.toml")
        return
    if not confirm(f"publish {', '.join(f'{n} {v}' for _, n, v, _, _ in todo)} to crates.io?", yes):
        raise ReleaseError("crates.io publish declined; run `release.py crates --tag " + tag + "` later")
    for rel, name, ver, _, _ in todo:
        out(f"publish {name} {ver}")
        r = subprocess.run([cargo, "publish", "--locked", "--manifest-path", f"{rel}/Cargo.toml"], cwd=root, env=env)
        if r.returncode != 0:
            raise ReleaseError(f"cargo publish {name} failed; the release is pushed. Fix it and run `release.py crates --tag {tag}`")


# ---- cut -----------------------------------------------------------------------------------

def show_diff(rel, o, n, limit=80):
    lines = list(difflib.unified_diff((o or "").splitlines(True), (n or "").splitlines(True),
                                      f"a/{rel}" if o is not None else "/dev/null", f"b/{rel}" if n is not None else "/dev/null", n=1))
    sys.stdout.writelines(lines[:limit])
    if len(lines) > limit:
        out(f"... ({len(lines) - limit} more diff lines)")
    if lines and not lines[min(len(lines), limit) - 1].endswith("\n"):
        out()


def cut(a, root):
    cfg = load_config(root, a.config)
    env = apply_env(cfg)
    dry = a.dry_run
    D = "[dry-run] " if dry else ""
    old = read_current(root, cfg)
    pfx = cfg.get("tag_prefix", "v")
    if a.version:
        new = a.version
        parse_version(new)
        part = "explicit"
    else:
        part = a.part or "minor"
        new = bump(old, part)
    if parse_version(new) <= parse_version(old):
        raise ReleaseError(f"version must increase ({old} -> {new})")
    tag = f"{pfx}{new}"
    date = a.date or today(cfg["timezone"])
    if not re.match(r"^\d{4}-\d{2}-\d{2}$", date):
        raise ReleaseError(f"--date {date!r} is not YYYY-MM-DD")
    messages = [m for m in (a.message or []) if m.strip()]
    if cfg.get("require_message") and not messages:
        raise ReleaseError("this project needs a release message")
    title = messages[0].strip().splitlines()[0].strip() if messages else ""
    title = re.sub(r"^v?" + re.escape(new) + r"\s*[:\-]\s*", "", title)
    title = re.sub(r"^(" + "|".join(ORDER) + r"):\s+", "", title).rstrip(".").strip()
    prev = tags(root, pfx)
    ctx = {"version": new, "old": old, "tag": tag, "old_tag": f"{pfx}{old}", "date": date, "title": title,
           "title_cell": title.replace("|", "/"), "project": cfg["project"], "tag_prefix": pfx, "build": ""}
    head = preflight(root, cfg, tag, new, a.expect_head, fetch=not a.no_fetch)
    henv = hook_env(env, ctx, dry)
    hooks = cfg.get("hooks") or {}
    if not a.skip_hooks:
        # Read-only checks: run in --dry-run too.
        run_cmds(root, hooks.get("pre"), ctx, henv, "pre hook", False)
    plan = plan_files(root, cfg, ctx)
    henv = hook_env(env, ctx, dry)
    cl = cfg.get("changelog") or {}
    section = a.section or ("Fixed" if part == "patch" else cl.get("default_section", "Added"))
    if cl:
        rel = cl.get("path", "CHANGELOG.md")
        bullets = messages[1:] if cl.get("first_message_is_title") else messages
        plan.put(rel, rewrite_changelog(plan.read(rel), cl, ctx, bullets, section, a.allow_empty_changelog))
    commit_subject = fill(cfg["commit_message"], ctx)
    body = "\n\n".join(m.strip() for m in messages)
    commit_msg = commit_subject + ("\n\n" + body if body else "") + "\n"
    tag_msg = fill(cfg["tag_message"], ctx).replace(": \n", "\n").rstrip(": ").strip()
    stage_extra = [fill(p, ctx) for p in hooks.get("stage", [])]
    remote, branch = cfg["remote"], cfg["branch"]
    push = ["git", "push", "--atomic", remote, f"HEAD:refs/heads/{branch}", f"refs/tags/{tag}"]

    out(f"{D}{cfg['project']} {old} -> {new} ({part}) on {branch} at {head[:12]}, {date}")
    for rel, (o, n) in plan.edits.items():
        what = "delete" if n is None else "create" if o is None else "write"
        note = "" if o is None or tracked(root, rel) else " (not tracked; written, not committed)"
        out(f"{D}{what} {rel}{note}")
    if not a.skip_hooks:
        for h in hooks.get("post_bump", []):
            out(f"{D}post_bump hook: {fill(h, ctx)}")
    if not a.skip_checks:
        for c in (cfg.get("checks") or {}).get("commands", []):
            out(f"{D}check: {fill(c, ctx)}")
    if stage_extra:
        out(f"{D}stage: {' '.join(stage_extra)}")
    out(f"{D}commit: {commit_subject}")
    out(f"{D}annotated tag: {tag} ({tag_msg})")
    out(f"{D}push: {'skipped (--no-push)' if a.no_push else ' '.join(push)}")
    if dry:
        for rel, (o, n) in plan.edits.items():
            show_diff(rel, o, n)
        if cl:
            out("\n[dry-run] changelog section:\n" + changelog_section(plan.edits.get(cl.get("path", "CHANGELOG.md"), ("", ""))[1] or "", ctx, cl))
        out("[dry-run] commit message:\n" + commit_msg)
        if not a.skip_crates and not a.no_push and cfg.get("crates"):
            try:
                publish_crates_preview(root, cfg, prev[-1][1] if prev else None)
            except ReleaseError as e:
                out(f"[dry-run] crates: {e}")
        for n in cfg.get("next", []):
            out(f"[dry-run] next: {fill(n, ctx)}")
        out("[dry-run] nothing changed")
        return 0
    if not confirm(f"release {cfg['project']} {tag}?", a.yes):
        raise ReleaseError("not confirmed; nothing changed")

    stage = [rel for rel, (o, n) in plan.edits.items() if (o is None and n is not None) or tracked(root, rel)]
    try:
        for rel, (o, n) in plan.edits.items():
            p = root / rel
            if n is None:
                p.unlink()
                try:
                    p.parent.rmdir()
                except OSError:
                    pass
            else:
                p.parent.mkdir(parents=True, exist_ok=True)
                with open(p, "w", encoding="utf-8", newline="") as f:
                    f.write(n)
        if not a.skip_hooks:
            run_cmds(root, hooks.get("post_bump"), ctx, henv, "post_bump hook", False)
        if not a.skip_checks:
            run_cmds(root, (cfg.get("checks") or {}).get("commands"), ctx, henv, "check", False)
        if git(root, "rev-parse", "HEAD").stdout.strip() != head:
            raise ReleaseError("HEAD moved while preparing the release")
        if stage:
            git(root, "add", "--", *stage)
        extra = [p for p in stage_extra if (root / p).exists() or git(root, "ls-files", "--", p).stdout.strip()]
        if extra:
            git(root, "add", "-A", "--", *extra)
        stray = git(root, "status", "--porcelain", "--untracked-files=no").stdout
        stray = [l for l in stray.splitlines() if l[1:2] != " "]
        if stray:
            raise ReleaseError("files changed outside the release set (add them to hooks.stage):\n" + "\n".join(stray))
        r = git(root, "commit", "--quiet", "-F", "-", input=commit_msg, check=False)
        if r.returncode != 0:
            raise ReleaseError(f"git commit failed: {(r.stderr or r.stdout).strip()}")
    except BaseException:
        # The tree was clean at the start (preflight), so this only drops the release's own edits.
        git(root, "reset", "--quiet", "--hard", head, check=False)
        for rel, (o, n) in plan.edits.items():
            p = root / rel
            if o is None and p.is_file() and not tracked(root, rel):
                p.unlink()
            elif o is not None and not tracked(root, rel):
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(o, encoding="utf-8")
        left = git(root, "status", "--porcelain").stdout.strip()
        print(f"release: stopped before the commit; restored {head[:12]}" + (f" (untracked files left by hooks:\n{left})" if left else ""), file=sys.stderr)
        raise
    commit = git(root, "rev-parse", "HEAD").stdout.strip()
    if git(root, "rev-parse", "HEAD~1").stdout.strip() != head:
        raise ReleaseError(f"the release commit {commit[:12]} is not on top of {head[:12]}; not tagging")
    out(f"committed {commit[:12]} {commit_subject}")
    git(root, "tag", "-a", tag, "-m", tag_msg)
    out(f"tagged {tag}")
    if a.no_push:
        out(f"not pushed; push with: {' '.join(push)}")
        return 0
    r = subprocess.run(["git", "-C", str(root), *push[1:]], text=True)
    if r.returncode != 0:
        raise ReleaseError(f"push failed; {tag} and the commit are local. Retry: {' '.join(push)}")
    out(f"pushed {branch} and {tag}")
    if not a.skip_crates and cfg.get("crates"):
        publish_crates(root, cfg, env, tag, prev[-1][1] if prev else None, False, a.yes)
    if not a.skip_hooks:
        run_cmds(root, hooks.get("post_push"), ctx, henv, "post_push hook", False)
    for n in cfg.get("next", []):
        out(f"next: {fill(n, ctx)}")
    return 0


def publish_crates_preview(root, cfg, prev_tag):
    """Dry-run view of crates.io: changes are measured from the previous tag to HEAD."""
    for rel, name, ver, act, why in crate_plan(root, cfg, "HEAD", prev_tag):
        out(f"[dry-run] crate {name} {ver}: {act} ({why})")


# ---- TAUR ----------------------------------------------------------------------------------

# makepkg's srcinfo.sh order (srcinfo_write_global and multivalued_arch_attrs).
_SUMS = ["cksums", "md5sums", "sha1sums", "sha224sums", "sha256sums", "sha384sums", "sha512sums", "b2sums"]
BASE_FIELDS = ["pkgdesc", "pkgver", "pkgrel", "epoch", "url", "install", "changelog", "arch", "groups", "license",
               "checkdepends", "makedepends", "depends", "optdepends", "provides", "conflicts", "replaces",
               "noextract", "options", "backup", "source", "validpgpkeys"] + _SUMS
ARCH_FIELDS = ["source", "provides", "conflicts", "depends", "replaces", "optdepends", "makedepends",
               "checkdepends"] + _SUMS


def _words(s):
    res, i = [], 0
    while i < len(s):
        c = s[i]
        if c.isspace():
            i += 1
        elif c == "#":
            j = s.find("\n", i)
            i = len(s) if j < 0 else j
        elif c in "'\"":
            j = s.find(c, i + 1)
            if j < 0:
                raise ReleaseError(f"PKGBUILD: unterminated quote in {s[i:i + 40]!r}")
            res.append((s[i + 1:j], c == '"'))
            i = j + 1
        else:
            j = i
            while j < len(s) and not s[j].isspace():
                j += 1
            res.append((s[i:j], True))
            i = j
    return res


def _strip_quoted(s):
    return re.sub(r"'[^']*'|\"[^\"]*\"", "", s)


def _close_paren(s):
    q = None
    for k, c in enumerate(s):
        if q:
            if c == q:
                q = None
        elif c in "'\"":
            q = c
        elif c == ")":
            return k
    raise ReleaseError("PKGBUILD: unterminated array")


def parse_pkgbuild(text):
    """Top-level literal assignments of a PKGBUILD, with $var / ${var} expanded; functions skipped."""
    vals, i, lines = {}, 0, text.splitlines()

    def expand(v, dq):
        if not dq:
            return v

        def sub(m):
            x = vals.get(m.group(1) or m.group(2), "")
            return " ".join(x) if isinstance(x, list) else str(x)
        return re.sub(r"\$\{(\w+)\}|\$(\w+)", sub, v)

    while i < len(lines):
        m = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$", lines[i])
        if not m:
            i += 1
            continue
        name, rest = m.groups()
        if rest.startswith("("):
            body = rest[1:]
            while ")" not in _strip_quoted(body):
                i += 1
                if i >= len(lines):
                    raise ReleaseError(f"PKGBUILD: unterminated array {name}")
                body += "\n" + lines[i]
            body = body[:_close_paren(body)]
            vals[name] = [expand(w, dq) for w, dq in _words(body)]
        else:
            ws = _words(rest)
            vals[name] = expand(ws[0][0], ws[0][1]) if ws else ""
        i += 1
    return vals


def srcinfo(text):
    """.SRCINFO for a single-package PKGBUILD, as makepkg --printsrcinfo prints it."""
    v = parse_pkgbuild(text)
    name = v.get("pkgname")
    if not isinstance(name, str) or not name:
        raise ReleaseError("PKGBUILD has no single pkgname")
    res = [f"pkgbase = {v.get('pkgbase', name)}"]
    for f in BASE_FIELDS:
        x = v.get(f)
        if x is None or x == "" or x == []:
            continue
        for item in (x if isinstance(x, list) else [x]):
            res.append(f"\t{f} = {item}")
    arches = v.get("arch", [])
    for ar in arches if isinstance(arches, list) else [arches]:
        for f in ARCH_FIELDS:
            for item in v.get(f"{f}_{ar}", []) or []:
                res.append(f"\t{f}_{ar} = {item}")
    res += ["", f"pkgname = {name}", ""]
    return "\n".join(res)


def make_srcinfo(pkgdir, mode="auto"):
    """makepkg --printsrcinfo when asked for (or 'auto' and on PATH), else the built-in generator."""
    text = (Path(pkgdir) / "PKGBUILD").read_text(encoding="utf-8")
    mk = os.environ.get("WEIRDNET_RELEASE_MAKEPKG", "makepkg")
    if mode == "makepkg" or (mode == "auto" and shutil.which(mk)):
        r = subprocess.run([mk, "--printsrcinfo"], cwd=pkgdir, capture_output=True, text=True)
        if r.returncode != 0 or not r.stdout.strip():
            raise ReleaseError(f"makepkg --printsrcinfo failed in {pkgdir}: {r.stderr.strip()[:300]}")
        return r.stdout
    return srcinfo(text)


def taur_check(version, pkgdir, pkg=None):
    """Problems with the recipe for VERSION (empty when it can be uploaded)."""
    pkg = pkg or {}
    pkgdir = Path(pkgdir)
    try:
        text = (pkgdir / "PKGBUILD").read_text(encoding="utf-8")
    except OSError as e:
        return [f"missing PKGBUILD: {e}"]
    v = parse_pkgbuild(text)
    errs = []
    if pkg.get("name") and v.get("pkgname") != pkg["name"]:
        errs.append(f"PKGBUILD pkgname is {v.get('pkgname')!r}, not {pkg['name']!r}")
    if v.get("pkgver") != version:
        errs.append(f"PKGBUILD pkgver is {v.get('pkgver')!r}, not {version!r}")
    if not re.fullmatch(r"\d+", str(v.get("pkgrel", ""))):
        errs.append(f"PKGBUILD pkgrel {v.get('pkgrel')!r} is not a number")
    sums = [k for k in v if re.fullmatch(r"sha256sums(_\w+)?", k)]
    if not sums:
        errs.append("PKGBUILD has no sha256sums")
    for k in sums:
        if not v[k] or not all(HEX64.match(s) for s in v[k]):
            errs.append(f"{k} {v[k]!r} is not all real sha256 (SKIP and placeholders can't be published)")
    want = fill(pkg.get("source_contains", "v{version}"), {"version": version})
    for k in [k for k in v if re.fullmatch(r"source(_\w+)?", k)]:
        for s in v[k]:
            if "://" in s and want not in s:
                errs.append(f"{k} {s!r} is not {want}'s")
    if pkg.get("require_maintainer", True):
        m = re.search(r"^# Maintainer:\s*(.*)$", text, re.M)
        if not m or not m.group(1).strip() or "placeholder" in m.group(1).lower():
            errs.append("the # Maintainer: line is missing or a placeholder")
    if pkg.get("srcinfo", "committed") == "committed":
        try:
            have = (pkgdir / ".SRCINFO").read_text(encoding="utf-8")
        except OSError:
            errs.append(".SRCINFO is missing (run release.py taur-srcinfo)")
        else:
            gen = make_srcinfo(pkgdir, pkg.get("srcinfo_tool", "auto"))
            if have != gen:
                errs.append(".SRCINFO doesn't match the PKGBUILD (run release.py taur-srcinfo)")
    return errs


def taur_pack(pkgdir, out_dir, version, srcinfo_text=None):
    """NAME-VERSION-taur.tar.gz with NAME/PKGBUILD, NAME/.SRCINFO and any install/extra files
    the PKGBUILD names locally; fixed mtimes and owners (reproducible)."""
    pkgdir = Path(pkgdir)
    text = (pkgdir / "PKGBUILD").read_text(encoding="utf-8")
    v = parse_pkgbuild(text)
    base = v["pkgname"]
    files = [("PKGBUILD", text.encode()), (".SRCINFO", (srcinfo_text if srcinfo_text is not None else (pkgdir / ".SRCINFO").read_text()).encode())]
    extra = [v["install"]] if isinstance(v.get("install"), str) and v.get("install") else []
    for k in [k for k in v if re.fullmatch(r"source(_\w+)?", k)]:
        extra += [s for s in v[k] if "://" not in s and (pkgdir / s).is_file()]
    for n in extra:
        if n not in ("PKGBUILD", ".SRCINFO"):
            files.append((n, (pkgdir / n).read_bytes()))
    res = Path(out_dir) / f"{base}-{version}-taur.tar.gz"
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as t:
        d = tarfile.TarInfo(base)
        d.type, d.mode, d.mtime = tarfile.DIRTYPE, 0o755, 0
        t.addfile(d)
        for n, data in files:
            if len(data) > 256 * 1024:
                raise ReleaseError(f"{n} is over TAUR's 256 KiB file limit")
            i = tarfile.TarInfo(f"{base}/{n}")
            i.size, i.mode, i.mtime = len(data), 0o644, 0
            t.addfile(i, io.BytesIO(data))
    res.write_bytes(gzip.compress(buf.getvalue(), mtime=0))
    return res, [n for n, _ in files]


def _sha256_url(url):
    h = hashlib.sha256()
    with urllib.request.urlopen(url, timeout=120) as r:
        for chunk in iter(lambda: r.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def verify_sources(pkgdir, fetch=None):
    v = parse_pkgbuild((Path(pkgdir) / "PKGBUILD").read_text(encoding="utf-8"))
    fetch = fetch or _sha256_url
    errs = []
    for k in [k for k in v if re.fullmatch(r"source(_\w+)?", k)]:
        sk = k.replace("source", "sha256sums", 1)
        for s, want in zip(v.get(k) or [], v.get(sk) or []):
            url = s.split("::", 1)[-1]
            if "://" not in url:
                continue
            try:
                got = fetch(url)
            except Exception as e:  # noqa: BLE001
                errs.append(f"can't download {url}: {e}")
                continue
            if got != want:
                errs.append(f"{url}: sha256 {got}, the PKGBUILD says {want}")
    return errs


def github_slug(root):
    url = git(root, "config", "--get", "remote.origin.url", check=False).stdout.strip()
    m = re.fullmatch(r"(?:git@github\.com:|ssh://git@github\.com/|https://github\.com/)([\w.-]+)/([\w.-]+?)(?:\.git)?/?", url)
    return f"{m.group(1)}/{m.group(2)}" if m else None


def confirm_public(root):
    """Private repositories are never published to TAUR: gh must say PUBLIC."""
    slug = github_slug(root)
    if not slug:
        raise ReleaseError("origin is not a GitHub repository; can't confirm it is public")
    gh = os.environ.get("WEIRDNET_RELEASE_GH", "gh")
    r = subprocess.run([gh, "repo", "view", slug, "--json", "visibility", "--jq", ".visibility"], capture_output=True, text=True)
    vis = r.stdout.strip()
    if r.returncode != 0 or vis != "PUBLIC":
        raise ReleaseError(f"{slug} is not confirmed PUBLIC ({vis or r.stderr.strip()[:200] or 'no answer'}); private repositories are not published to TAUR")
    return slug


def taur(a, root):
    cfg = load_config(root, a.config)
    env = apply_env(cfg)
    tc = cfg.get("taur")
    if not tc:
        raise ReleaseError(f"{CONFIG} has no [taur] table (this project does not publish to TAUR)")
    version = a.version.lstrip("v")
    parse_version(version)
    dry = a.dry_run
    D = "[dry-run] " if dry else ""
    pkgs = tc.get("packages", [])
    if a.package:
        pkgs = [p for p in pkgs if p.get("name") == a.package]
        if not pkgs:
            raise ReleaseError(f"no [[taur.packages]] named {a.package}")
    org = tc.get("org", "WeirdBrains")
    taurctl = a.taurctl or os.environ.get("TAURCTL", "taurctl")
    ctx = {"version": version, "tag": f"{cfg.get('tag_prefix', 'v')}{version}", "project": cfg["project"]}
    for pkg in pkgs:
        work = Path(tempfile.mkdtemp(prefix=f"taur-{pkg.get('name', 'pkg')}-"))
        try:
            ctx["work"] = str(work)
            henv = hook_env(env, ctx, dry)
            if pkg.get("command"):
                # A project-specific publisher (it does its own checks, upload and verification).
                cmd = fill(pkg["command"], ctx) + (" " + pkg.get("dry_run_flag", "--dry-run") if dry else "")
                out(f"{pkg.get('name')}: delegated to {cmd}")
                if subprocess.run(["bash", "-c", cmd], cwd=root, env=henv).returncode != 0:
                    raise ReleaseError(f"{cmd} failed")
                continue
            if tc.get("require_public", True):
                slug = confirm_public(root)
                out(f"{slug} is public")
            run_cmds(root, pkg.get("prepare"), ctx, henv, "prepare", False)
            pkgdir = (root / fill(pkg["dir"], ctx)) if not Path(fill(pkg["dir"], ctx)).is_absolute() else Path(fill(pkg["dir"], ctx))
            text = None
            if pkg.get("srcinfo", "committed") == "generate":
                text = make_srcinfo(pkgdir, pkg.get("srcinfo_tool", "auto"))
            errs = taur_check(version, pkgdir, pkg)
            if errs:
                raise ReleaseError(f"{pkg.get('name')} recipe not ready:\n  " + "\n  ".join(errs))
            out(f"{pkg.get('name')} {version}: recipe ok ({pkgdir.relative_to(root) if pkgdir.is_relative_to(root) else pkgdir})")
            if pkg.get("verify_sources", True) and not a.no_verify_sources:
                errs = verify_sources(pkgdir)
                if errs:
                    raise ReleaseError("release files don't match the PKGBUILD:\n  " + "\n  ".join(errs))
                out("release files match the PKGBUILD's sha256sums")
            archive, names = taur_pack(pkgdir, work, version, text)
            out(f"archive {archive.name}: {', '.join(names)}")
            if not shutil.which(taurctl):
                raise ReleaseError(f"{taurctl} not found: install taurctl (the taur package) first")
            who = subprocess.run([taurctl, "whoami"], capture_output=True, text=True)
            if who.returncode != 0:
                raise ReleaseError(f"taurctl is not signed in ({(who.stderr or who.stdout).strip()[:200]}); run: taurctl login NAME")
            out(f"taurctl: {who.stdout.strip()}")
            v = parse_pkgbuild((pkgdir / "PKGBUILD").read_text())
            name, want = v["pkgname"], f"{version}-{v['pkgrel']}"
            cur = taur_version(taurctl, name)
            if cur == want:
                out(f"TAUR already has {name} {want}; nothing to upload")
                continue
            cmd = [taurctl, "pkg", "upload", str(archive), "--org", org, "--yes"]
            if dry:
                out(f"{D}{' '.join(cmd[:3] + [archive.name] + cmd[4:])}  (TAUR has {cur or 'nothing'})")
                continue
            if not confirm(f"upload {name} {want} to TAUR (org {org})?", a.yes):
                raise ReleaseError("upload declined")
            up = subprocess.run(cmd, capture_output=True, text=True)
            if up.returncode != 0:
                raise ReleaseError(f"taurctl pkg upload failed: {(up.stderr or up.stdout).strip()[:400]}")
            got = taur_version(taurctl, name)
            if got != want:
                raise ReleaseError(f"uploaded, but TAUR reports {name} {got!r}, not {want!r}")
            out(f"TAUR has {name} {want}")
        finally:
            shutil.rmtree(work, ignore_errors=True)
    return 0


def taur_version(taurctl, name):
    r = subprocess.run([taurctl, "info", name, "--json"], capture_output=True, text=True)
    try:
        res = json.loads(r.stdout)["results"]
        return res[0]["Version"] if res else None
    except (ValueError, KeyError, IndexError, TypeError):
        return None


# ---- platforms and release assets ----------------------------------------------------------

OSES = ["linux", "macos", "windows", "android", "ios"]
ARCHES = ["x86_64", "aarch64", "universal"]
FORMATS = {
    "linux": ["tar.gz", "zip", "pkg.tar.zst", "deb", "rpm", "nix", "appimage", "flatpak"],
    "macos": ["tar.gz", "zip", "dmg", "app", "pkg", "brew-formula", "brew-cask"],
    "windows": ["zip", "exe", "exe-wix", "msi", "winget"],
    "android": ["apk", "aab"],
    "ios": ["ipa"],
}

# Where a release reaches people. github = assets on the GitHub release.
CHANNELS = {
    "linux": ["github", "taur", "nix", "flathub", "apt-repo", "rpm-repo"],
    "macos": ["github", "homebrew", "mac-app-store"],
    "windows": ["github", "winget", "microsoft-store"],
    "android": ["github", "google-play", "fdroid", "apk-sideload"],
    "ios": ["app-store", "testflight", "eu-marketplace", "altstore", "adhoc-ipa"],
}


def platform_report(cfg):
    """(lines, problems) for the [[platforms]] matrix against the collective platform rule:
    every OS is either listed or opted out with a reason."""
    plats = cfg.get("platforms") or []
    lines, problems, seen = [], [], set()
    for p in plats:
        os_ = p.get("os")
        if os_ not in OSES:
            problems.append(f"[[platforms]] os {os_!r} is not one of {OSES}")
            continue
        seen.add(os_)
        if p.get("opt_out"):
            lines.append(f"  {os_}: opted out ({p['opt_out']})")
            continue
        arch = p.get("arch", ["x86_64", "aarch64"])
        bad = [a for a in arch if a not in ARCHES]
        badf = [f for f in p.get("formats", []) if f not in FORMATS[os_]]
        if bad:
            problems.append(f"{os_}: unknown arch {bad} (use {ARCHES})")
        if badf:
            problems.append(f"{os_}: unknown formats {badf} (use {FORMATS[os_]})")
        if not p.get("formats"):
            problems.append(f"{os_}: no formats")
        badc = [c for c in p.get("channels", []) if c not in CHANNELS[os_]]
        if badc:
            problems.append(f"{os_}: unknown channels {badc} (use {CHANNELS[os_]})")
        need = ("aarch64",) if os_ in ("android", "ios") else ("x86_64", "aarch64")
        missing_arch = [a for a in need if a not in arch and "universal" not in arch]
        note = f"; no {', '.join(missing_arch)}" + (f" ({p['arch_note']})" if p.get("arch_note") else "") if missing_arch else ""
        ch = p.get("channels", [])
        lines.append(f"  {os_} [{', '.join(arch)}]: {', '.join(p.get('formats', []))}{note}"
                     + (f"; channels {', '.join(ch)}" if ch else "; no channels")
                     + ("" if p.get("assets") else "; no release assets yet"))
    for os_ in OSES:
        if plats and os_ not in seen:
            lines.append(f"  {os_}: not listed (add it, or opt out with a reason)")
    return lines, problems


def assets(a, root):
    """Check the GitHub release for VERSION has every [[platforms]] asset."""
    cfg = load_config(root, a.config)
    plats = [p for p in cfg.get("platforms") or [] if not p.get("opt_out")]
    if not plats:
        raise ReleaseError(f"{CONFIG} has no [[platforms]] with assets")
    version = a.version.lstrip("v")
    ctx = {"version": version, "tag": f"{cfg.get('tag_prefix', 'v')}{version}", "project": cfg["project"]}
    slug = github_slug(root)
    gh = os.environ.get("WEIRDNET_RELEASE_GH", "gh")
    r = subprocess.run([gh, "release", "view", ctx["tag"], "-R", slug, "--json", "assets,isDraft"], capture_output=True, text=True)
    if r.returncode != 0:
        raise ReleaseError(f"gh release view {ctx['tag']}: {(r.stderr or r.stdout).strip()[:300]}")
    rel = json.loads(r.stdout)
    have = [x["name"] for x in rel.get("assets", [])]
    bad = 0
    for p in plats:
        for pat in p.get("assets", []):
            want = fill(pat, ctx)
            hit = [n for n in have if fnmatch.fnmatch(n, want)]
            out(f"{'ok ' if hit else 'MISSING'} {p['os']}: {want}" + (f" ({', '.join(hit)})" if hit and hit != [want] else ""))
            bad += not hit
    if rel.get("isDraft"):
        out(f"note: {ctx['tag']} is still a draft")
    if bad:
        raise ReleaseError(f"{bad} expected asset(s) missing from {ctx['tag']}")
    return 0


# ---- main ----------------------------------------------------------------------------------

def find_root(repo):
    r = subprocess.run(["git", "-C", str(repo or "."), "rev-parse", "--show-toplevel"], capture_output=True, text=True)
    if r.returncode != 0:
        raise ReleaseError("run this inside the project's git checkout (or pass --repo)")
    return Path(r.stdout.strip())


def parser():
    ap = argparse.ArgumentParser(prog="release.py", description=__doc__.split("\n\n")[1].strip(),
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", help="project checkout (default: the current git repository)")
    ap.add_argument("--config", help=f"config file (default: REPO/{CONFIG})")
    sub = ap.add_subparsers(dest="cmd")
    c = sub.add_parser("cut", help="bump, changelog, checks, commit, tag, push, crates (the default)")
    g = c.add_mutually_exclusive_group()
    g.add_argument("--major", dest="part", action="store_const", const="major")
    g.add_argument("--minor", dest="part", action="store_const", const="minor", help="the default")
    g.add_argument("--patch", dest="part", action="store_const", const="patch")
    g.add_argument("--version", help="explicit X.Y.Z (must increase)")
    c.add_argument("message", nargs="*", help="changelog bullets / commit body (`Fixed: ...` picks a section)")
    c.add_argument("--dry-run", action="store_true", help="print every step and diff; change nothing")
    c.add_argument("--yes", "-y", action="store_true", help="don't ask for confirmation")
    c.add_argument("--no-push", action="store_true", help="commit and tag, don't push (skips crates)")
    c.add_argument("--no-fetch", action="store_true", help="skip origin checks (offline)")
    c.add_argument("--skip-checks", action="store_true", help="don't run [checks] commands")
    c.add_argument("--skip-hooks", action="store_true", help="don't run [hooks] commands")
    c.add_argument("--skip-crates", action="store_true", help="don't publish to crates.io")
    c.add_argument("--expect-head", metavar="SHA")
    c.add_argument("--date", help="release date (default: today in the config timezone)")
    c.add_argument("--section", choices=ORDER, help="changelog section for plain messages")
    c.add_argument("--allow-empty-changelog", action="store_true")
    sub.add_parser("validate", help="check release.toml and print the planned bump; change nothing")
    k = sub.add_parser("crates", help="publish affected public crates for an existing tag")
    k.add_argument("--tag", help="release tag (default: the latest)")
    k.add_argument("--dry-run", action="store_true")
    k.add_argument("--yes", "-y", action="store_true")
    t = sub.add_parser("taur", help="publish the project's TAUR recipe(s) for VERSION")
    t.add_argument("version")
    t.add_argument("--package")
    t.add_argument("--dry-run", action="store_true")
    t.add_argument("--yes", "-y", action="store_true")
    t.add_argument("--no-verify-sources", action="store_true")
    t.add_argument("--taurctl")
    s = sub.add_parser("taur-srcinfo", help="write DIR/.SRCINFO from DIR/PKGBUILD")
    s.add_argument("dir", nargs="?", default=".")
    s.add_argument("--tool", choices=["auto", "makepkg", "builtin"], default="builtin")
    ck = sub.add_parser("taur-check", help="is the recipe in DIR ready for VERSION")
    ck.add_argument("version")
    ck.add_argument("dir", nargs="?")
    sa = sub.add_parser("assets", help="check the GitHub release has every [[platforms]] asset")
    sa.add_argument("version")
    sub.add_parser("version", help="print the script version")
    return ap


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    cmds = {"cut", "validate", "assets", "crates", "taur", "taur-srcinfo", "taur-check", "version", "-h", "--help"}
    # `cut` is the default subcommand: insert it after the global options.
    i = 0
    while i < len(argv) and argv[i] in ("--repo", "--config"):
        i += 2
    if i >= len(argv) or argv[i] not in cmds:
        argv.insert(i, "cut")
    a = parser().parse_args(argv)
    try:
        if a.cmd == "version":
            out(__version__)
            return 0
        if a.cmd == "taur-srcinfo":
            d = Path(a.dir)
            (d / ".SRCINFO").write_text(make_srcinfo(d, a.tool), encoding="utf-8")
            out(f"wrote {d / '.SRCINFO'}")
            return 0
        root = find_root(a.repo)
        if a.cmd == "taur-check":
            cfg = load_config(root, a.config)
            pkgs = (cfg.get("taur") or {}).get("packages", [])
            pkg = next((p for p in pkgs if a.dir is None or p.get("dir") == a.dir), {})
            d = root / (a.dir or pkg.get("dir", "."))
            errs = taur_check(a.version.lstrip("v"), d, pkg)
            for e in errs:
                print(f"taur: {e}", file=sys.stderr)
            if errs:
                return 1
            name = parse_pkgbuild((d / "PKGBUILD").read_text(encoding="utf-8")).get("pkgname", d.name)
            out(f"{name} {a.version}: ready for TAUR")
            return 0
        if a.cmd == "taur":
            return taur(a, root)
        if a.cmd == "assets":
            return assets(a, root)
        if a.cmd == "validate":
            cfg = load_config(root, a.config)
            old = read_current(root, cfg)
            new = bump(old, "minor")
            pfx = cfg.get("tag_prefix", "v")
            ctx = {"version": new, "old": old, "tag": f"{pfx}{new}", "old_tag": f"{pfx}{old}", "date": "2000-01-01",
                   "title": "validate", "title_cell": "validate", "project": cfg["project"], "tag_prefix": pfx, "build": ""}
            plan = plan_files(root, cfg, ctx)
            cl = cfg.get("changelog")
            if cl:
                p = cl.get("path", "CHANGELOG.md")
                rewrite_changelog(plan.read(p), cl, ctx, ["validate"], "Added", True)
            for rel in (cfg.get("crates") or {}).get("publish", []):
                if not (root / rel / "Cargo.toml").is_file():
                    raise ReleaseError(f"[crates] {rel}: no Cargo.toml")
            for pkg in (cfg.get("taur") or {}).get("packages", []):
                if not pkg.get("command") and not pkg.get("prepare") and not (root / fill(pkg["dir"], ctx) / "PKGBUILD").is_file():
                    raise ReleaseError(f"[[taur.packages]] {pkg.get('name')}: no PKGBUILD in {pkg['dir']}")
            lines, problems = platform_report(cfg)
            if problems:
                raise ReleaseError("[[platforms]]: " + "; ".join(problems))
            if lines:
                out("platforms:")
                for l in lines:
                    out(l)
            out(f"{cfg['project']}: release.toml ok (helpers {__version__}); next minor {old} -> {new} rewrites {len(plan.edits) + (1 if cl else 0)} files")
            return 0
        if a.cmd == "crates":
            cfg = load_config(root, a.config)
            pfx = cfg.get("tag_prefix", "v")
            have = tags(root, pfx)
            tag = a.tag or (have[-1][1] if have else None)
            if not tag:
                raise ReleaseError("no release tag yet")
            older = [t for v, t in have if t != tag and v < parse_version(tag[len(pfx):])]
            publish_crates(root, cfg, apply_env(cfg), tag, older[-1] if older else None, a.dry_run, a.yes)
            return 0
        return cut(a, root)
    except ReleaseError as e:
        print(f"release: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
