#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The durable-cache authors
# SPDX-License-Identifier: Apache-2.0

"""
reqman-context.py — Graph-scoped requirement context for coding agents.

Reads <jobname>-requirements.json (written by reqman-report.py) and returns
a requirement plus its linked neighborhood, instead of a coding agent
having to read whole .tex chapters and re-derive the relationship graph
from scattered \\refine/\\satisfy/\\reqcover macro calls.

Usage (run from the repository root):
  python3 reqman-context.py <jobname> <ID> [<ID> ...] [--depth N] [--direction up|down|both] [--format md|json]
  python3 reqman-context.py <jobname> --search TERM
  python3 reqman-context.py <jobname> --overview
  python3 reqman-context.py <jobname> --changed REV [--depth N] [--format md|json]
  python3 reqman-context.py <jobname> --ripple <ID> [<ID> ...] [--depth N] [--format md|json]

<jobname>-requirements.json is looked up in --dir (default: "docs"), since
that's where this project's LaTeX build (see docs/latexmkrc) writes it
alongside the PDF; --overview lists the artifact types in the order
<jobname>-types.json, written beside it, says the document declares them.

--changed compares the requirement blocks of the .tex sources the export
names at git revision REV with the working tree, and --ripple lists what a
change to a requirement reaches: see "Tracing a change" in SKILL.md.
"""
import argparse
import json
import os
import re
import subprocess
import sys
from collections import defaultdict

# "up" = toward parents (this requirement refines/satisfies/verifies/etc. X)
# "down" = toward children (X refines/satisfies/verifies/etc. this requirement)
# conflicts_with is symmetric, so it belongs to both directions.
UP_KEYS = {
    "derives_from", "satisfies", "refines", "verifies", "traces_to",
    "copies", "conflicts_with",
}
DOWN_KEYS = {
    "derived_by", "satisfied_by", "refined_by", "verified_by", "traced_by",
    "copied_by", "conflicts_with",
}

# The order --ripple lists relationships in: the links that build on a
# requirement first, the weaker trace last.
RIPPLE_DOWN = ["refined_by", "derived_by", "satisfied_by", "verified_by",
               "copied_by", "traced_by"]
RIPPLE_UP = ["refines", "derives_from", "satisfies", "verifies", "copies",
             "traces_to"]

# An artifact ID: TYPE-SUBID-VERSION (reqman.sty enforces the two hyphens).
ID_PATTERN = r"[A-Z]+-[A-Za-z0-9.]+-\d+"
BEGIN_RE = re.compile(r"\\begin\{requirement\}\{(" + ID_PATTERN + r")\}")
STATUS_RE = re.compile(r"^(\\begin\{requirement\}\{[^}]+\})\[[^\]]*\]")


def load_requirements(jobname, directory):
    path = os.path.join(directory, f"{jobname}-requirements.json")
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        print(f"error: {path} not found -- run reqman-report.py {jobname} first",
              file=sys.stderr)
        sys.exit(1)


def load_pending(jobname, directory):
    """Best-effort: <jobname>-pending-coverage.json only exists once some
    \\reqexpect obligation targets an ID that hasn't been written yet, so a
    missing file just means there's nothing pending -- not an error."""
    path = os.path.join(directory, f"{jobname}-pending-coverage.json")
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return {}


def load_types(jobname, directory):
    """The artifact type prefixes in the order the document declares them
    (<jobname>-types.json, which reqman-report.py writes beside the
    requirements). Best-effort, like load_pending: without the file,
    --overview lists the types alphabetically."""
    path = os.path.join(directory, f"{jobname}-types.json")
    try:
        with open(path) as f:
            return [t["prefix"] for t in json.load(f)]
    except FileNotFoundError:
        return []


def neighbor_ids(entry, direction):
    keys = set()
    if direction in ("up", "both"):
        keys |= UP_KEYS
    if direction in ("down", "both"):
        keys |= DOWN_KEYS
    rel = entry.get("relationships", {})
    return [tgt for k in keys for tgt in rel.get(k, [])]


def _fmt_expectation(e):
    item = f"{e['source']} wants {e['rel']}"
    if e.get("note"):
        item += f' -- "{e["note"]}"'
    return item


def collect_context(data, seed_ids, depth, direction, pending=None):
    pending  = pending or {}
    visited  = set()
    frontier = [sid for sid in seed_ids if sid in data]
    for sid in seed_ids:
        if sid not in data:
            if sid in pending:
                exps = pending[sid]
                parts = "; ".join(_fmt_expectation(e) for e in exps)
                print(f"note: '{sid}' has not been written yet; "
                      f"{len(exps)} requirement(s) expect coverage from it: {parts}",
                      file=sys.stderr)
            else:
                print(f"warning: unknown ID '{sid}'", file=sys.stderr)
    for _ in range(depth + 1):
        next_frontier = []
        for nid in frontier:
            if nid in visited:
                continue
            visited.add(nid)
            for n in neighbor_ids(data[nid], direction):
                if n in data and n not in visited:
                    next_frontier.append(n)
        frontier = next_frontier
        if not frontier:
            break
    return visited


def render_markdown(data, ids, seed_ids):
    out = []
    ordered = list(seed_ids) + sorted(i for i in ids if i not in seed_ids)
    for aid in ordered:
        if aid not in data:
            continue
        e = data[aid]
        out.append(f"## [{e['type']}] {aid} -- {e['title']}")
        flag = " (undeclared type)" if e.get("undeclared") else ""
        out.append(f"*{e['reqtype_label']}* - status: {e['status']}{flag}")
        out.append("")
        if e["body"]:
            out.append(e["body"])
            out.append("")
        rel = e.get("relationships", {})
        for key in sorted(rel):
            targets = rel[key]
            if not targets:
                continue
            label    = key.replace("_", " ").capitalize()
            rendered = ", ".join(
                f"{t} ({data[t]['title']})" if t in data else t for t in targets
            )
            out.append(f"**{label}:** {rendered}")
        cov = e.get("coverage", {})
        if cov.get("required"):
            parts = []
            for r in cov["required"]:
                mark   = "[x]" if r["met"] else "[ ]"
                target = r.get("target_id") or r["prefix"]
                item   = f"{mark} {target} via {r['rel']}"
                if r.get("note"):
                    item += f' -- "{r["note"]}"'
                parts.append(item)
            out.append(f"**Required coverage:** {', '.join(parts)}")
        elif cov.get("terminal"):
            reason = cov.get("terminal_reason", "")
            out.append("**Required coverage:** none (terminal"
                       + (f" -- {reason})" if reason else ")"))
        expected = e.get("expected_by") or []
        if expected:
            parts = [f"{'[x]' if x['met'] else '[ ]'} {_fmt_expectation(x)}"
                     for x in expected]
            out.append(f"**Expected by (forward-declared):** {', '.join(parts)}")
        out.append("")
    return "\n".join(out).rstrip() + "\n"


def cmd_search(data, term):
    term_lower = term.lower()
    matches = sorted(
        aid for aid, e in data.items()
        if term_lower in e["title"].lower() or term_lower in e["body"].lower()
    )
    if not matches:
        print(f"No matches for '{term}'.")
        return
    for aid in matches:
        e = data[aid]
        print(f"[{e['type']}] {aid}\t{e['title']}\t({e['status']})")


def domain_of(aid):
    """The domain an ID names: the part of its sub-ID before the first dot
    (HLR-PERSISTENCE.DURABLE-01 belongs to PERSISTENCE)."""
    return aid.split("-")[1].split(".")[0]


def cmd_overview(data, type_order):
    """The PDF's domain overview as text: each domain with how many artifacts
    of each type it holds, types in type_order, then the relationships from
    one domain's artifacts to another's, counted by kind, trace aside."""
    per_domain = defaultdict(lambda: defaultdict(int))
    for aid, e in data.items():
        per_domain[domain_of(aid)][e["type"]] += 1
    pairs = defaultdict(lambda: defaultdict(int))
    conflicts = set()
    for aid, e in data.items():
        rel = e.get("relationships", {})
        for key in sorted(UP_KEYS - {"traces_to"}):
            for tgt in rel.get(key, []):
                if tgt not in data or domain_of(aid) == domain_of(tgt):
                    continue
                if key == "conflicts_with":
                    # Listed on both artifacts of a conflict: count it once.
                    if frozenset((aid, tgt)) in conflicts:
                        continue
                    conflicts.add(frozenset((aid, tgt)))
                pairs[(domain_of(aid), domain_of(tgt))][key.replace("_", " ")] += 1

    def order(types):
        return ([t for t in type_order if t in types]
                + sorted(t for t in types if t not in type_order))

    domains = sorted(per_domain, key=lambda d: (-sum(per_domain[d].values()), d))
    print(f"## Domains: {len(data)} artifacts in {len(domains)} domains")
    print()
    print("| Domain | Artifacts | By type |")
    print("|---|---:|---|")
    for d in domains:
        counts = per_domain[d]
        by_type = ", ".join(f"{t} {counts[t]}" for t in order(counts))
        print(f"| {d} | {sum(counts.values())} | {by_type} |")
    print()
    print("## Relationships between domains (trace left out)")
    print()
    print("| From | To | Relationships |")
    print("|---|---|---|")
    for (src, tgt), rels in sorted(pairs.items(), key=lambda kv: (-sum(kv[1].values()), kv[0])):
        listed = ", ".join(f"{n} {name}" for name, n in sorted(rels.items(), key=lambda kv: -kv[1]))
        print(f"| {src} | {tgt} | {listed} |")


# ── Tracing a change (--changed, --ripple) ─────────────────────────────────


def split_version(aid):
    """TYPE-SUBID-VERSION as (TYPE-SUBID, VERSION as a number)."""
    stem, _, number = aid.rpartition("-")
    return stem, int(number)


def next_version(aid):
    """The ID a changed requirement takes: its VERSION one higher."""
    stem, number = split_version(aid)
    return f"{stem}-{number + 1:02d}"


def id_regex(ids):
    """A pattern matching any of `ids` as a whole ID, not inside a longer one
    (FDS-X.Y-01 neither in IMP-FDS-X.Y-01 nor in FDS-X.Y-012)."""
    alternatives = "|".join(re.escape(i) for i in sorted(ids, key=len, reverse=True))
    return re.compile(r"(?<![\w.-])(" + alternatives + r")(?![\w-])")


def substitute_ids(text, mapping):
    if not mapping:
        return text
    return id_regex(mapping).sub(lambda m: mapping[m.group(1)], text)


def git(root, *args):
    return subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)


def repo_root():
    r = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit("error: --changed and --ripple need a git checkout")
    return r.stdout.strip()


def source_files(data):
    """The .tex files the export says the requirements come from, relative
    to --dir: no list of chapters to keep in step with the document."""
    return sorted({e["source"]["file"] for e in data.values()
                   if isinstance(e.get("source"), dict) and e["source"].get("file")})


def requirement_blocks(text):
    """Each requirement environment in `text`, from its \\begin to its
    \\end, by ID."""
    blocks = {}
    for m in BEGIN_RE.finditer(text):
        end = text.find("\\end{requirement}", m.end())
        blocks[m.group(1)] = text[m.start():end if end != -1 else len(text)]
    return blocks


def blocks_at(rev, files, directory, root):
    """The requirement blocks of `files` at git revision `rev`, or in the
    working tree when `rev` is None. A file missing there has none."""
    found = {}
    for name in files:
        local = os.path.join(directory, name)
        if rev is None:
            try:
                with open(local) as f:
                    text = f.read()
            except FileNotFoundError:
                continue
        else:
            path = os.path.relpath(os.path.abspath(local), root)
            r = git(root, "show", f"{rev}:{path}")
            if r.returncode != 0:
                continue
            text = r.stdout
        found.update(requirement_blocks(text))
    return found


def compare_blocks(base, head):
    """Classify the requirements of `base` and `head` (ID -> block text).

    A requirement that left `base` as TYPE-SUBID-NN and is in `head` as the
    same TYPE-SUBID with a higher VERSION is a new version, not a removal
    and an addition. Texts are compared with whitespace collapsed and with
    every new version's old ID read as its new one, so a requirement whose
    only edits follow those renames counts as unchanged."""
    added, removed = set(head) - set(base), set(base) - set(head)
    later = defaultdict(list)
    for aid in added:
        later[split_version(aid)[0]].append(aid)
    versions = {}
    for old in sorted(removed):
        stem, number = split_version(old)
        newer = sorted((a for a in later.get(stem, []) if split_version(a)[1] > number),
                       key=lambda a: split_version(a)[1])
        if newer:
            versions[old] = newer[0]
    added -= set(versions.values())
    removed -= set(versions)

    def text(block):
        # The status, the first optional argument, is left out: moving a
        # requirement from draft to accepted changes nothing it says.
        return " ".join(STATUS_RE.sub(r"\1", block, count=1).split())

    def same(old, new):
        return text(substitute_ids(base[old], versions)) == text(head[new])

    return {
        "versions": versions,
        "versions_without_change": sorted(o for o, n in versions.items() if same(o, n)),
        "changed_without_version": sorted(a for a in set(base) & set(head) if not same(a, a)),
        "added": sorted(added),
        "removed": sorted(removed),
    }


def mentioned_in(data, aid):
    """The artifacts whose text names `aid`, besides `aid` itself."""
    pattern = id_regex([aid])
    return sorted(other for other, e in data.items()
                  if other != aid and pattern.search(e.get("body", "")))


def cited_in_files(root, aid, exclude_dir=None):
    """(path, count) for each tracked text file that names `aid`, the .tex
    sources in `exclude_dir` left out when given."""
    args = ["grep", "-I", "-c", "-E", r"(^|[^A-Za-z0-9_.-])" + re.escape(aid) + r"($|[^A-Za-z0-9_-])",
            "--", "."]
    if exclude_dir:
        args.append(f":(exclude){exclude_dir}/*.tex")
    r = git(root, *args)
    cited = []
    for line in r.stdout.splitlines():
        path, _, count = line.rpartition(":")
        cited.append((path, int(count)))
    return cited


def ripple(data, aid, depth):
    """What a change to `aid` reaches: the artifacts linked to it in either
    direction, those that only name it in their text, and, with depth > 1,
    the ones built on those in turn."""
    rel = data[aid].get("relationships", {})
    down = [(k, rel[k]) for k in RIPPLE_DOWN if rel.get(k)]
    up = [(k, rel[k]) for k in RIPPLE_UP if rel.get(k)]
    conflicts = rel.get("conflicts_with", [])
    linked = {t for _, ts in down + up for t in ts} | set(conflicts)
    further = []
    seen = {aid} | {t for k, ts in down if k != "traced_by" for t in ts}
    frontier = sorted(seen - {aid})
    for hop in range(2, depth + 1):
        next_frontier = []
        for node in frontier:
            for k in RIPPLE_DOWN:
                if k == "traced_by":
                    continue
                for t in data.get(node, {}).get("relationships", {}).get(k, []):
                    if t not in seen:
                        seen.add(t)
                        next_frontier.append(t)
                        further.append({"hop": hop, "id": t, "rel": k, "of": node})
        frontier = next_frontier
    return {
        "down": dict(down),
        "up": dict(up),
        "conflicts_with": conflicts,
        "mentioned_in": [m for m in mentioned_in(data, aid) if m not in linked],
        "expected_by": data[aid].get("expected_by") or [],
        "further_down": further,
    }


def _titled(data, ids):
    return ", ".join(f"{t} ({data[t]['title']})" if t in data else t for t in ids)


def render_ripple(data, aid, r, old=None, cited=None, stale=None):
    out = []
    title = data[aid]["title"] if aid in data else "?"
    out.append(f"### {aid} -- {title}" + (f" (new version of {old})" if old else ""))
    if r["down"]:
        out.append("Built on it -- review each against the new text:")
        for k, ts in r["down"].items():
            out.append(f"- {k.replace('_', ' ')}: {_titled(data, ts)}")
    if r["further_down"]:
        out.append("Further down -- built on those in turn:")
        for f in r["further_down"]:
            out.append(f"- hop {f['hop']}: {_titled(data, [f['id']])}, "
                       f"{f['rel'].replace('_', ' ')} {f['of']}")
    if r["up"]:
        out.append("It builds on -- check it still serves them:")
        for k, ts in r["up"].items():
            out.append(f"- {k.replace('_', ' ')}: {_titled(data, ts)}")
    if r["conflicts_with"]:
        out.append(f"Conflicts with: {_titled(data, r['conflicts_with'])}")
    if r["mentioned_in"]:
        out.append("Named in the text of, without a link -- check what they say of it: "
                   + _titled(data, r["mentioned_in"]))
    if r["expected_by"]:
        out.append("Expected by (forward-declared): "
                   + ", ".join(_fmt_expectation(x) for x in r["expected_by"]))
    if cited:
        out.append("Cited outside the specification: "
                   + ", ".join(f"{p} ({n})" for p, n in cited))
    if stale:
        out.append(f"STILL CITING {old}: " + ", ".join(f"{p} ({n})" for p, n in stale))
    out.append("")
    return out


def cmd_changed(data, rev, depth, directory, fmt):
    root = repo_root()
    if git(root, "rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}").returncode != 0:
        sys.exit(f"error: no such revision: {rev}")
    files = source_files(data)
    base = blocks_at(rev, files, directory, root)
    head = blocks_at(None, files, directory, root)
    result = compare_blocks(base, head)
    spec_dir = os.path.relpath(os.path.abspath(directory), root)

    seeds = [(new, old) for old, new in sorted(result["versions"].items())]
    seeds += [(aid, None) for aid in result["changed_without_version"]]
    ripples = {}
    for aid, old in seeds:
        stale = cited_in_files(root, old) if old else []
        if aid not in data:
            print(f"warning: {aid} is not in the export: rebuild the specification",
                  file=sys.stderr)
            ripples[aid] = {"old": old, "stale": stale}
            continue
        ripples[aid] = dict(ripple(data, aid, depth), old=old,
                            cited=cited_in_files(root, aid, spec_dir), stale=stale)

    if fmt == "json":
        print(json.dumps(dict(result, revision=rev, ripple=ripples), indent=2,
                         ensure_ascii=False))
        return
    out = [f"## Requirements changed since {rev}", ""]
    if result["versions"]:
        out.append(f"New versions ({len(result['versions'])}):")
        for old, new in sorted(result["versions"].items()):
            note = " -- same text: no change calls for it" \
                if old in result["versions_without_change"] else ""
            out.append(f"- {old} -> {new}{note}")
        out.append("")
    if result["changed_without_version"]:
        out.append(f"Changed without a new version ({len(result['changed_without_version'])}):")
        out += [f"- {a} -> {next_version(a)}" for a in result["changed_without_version"]]
        out.append("")
    for key, label in (("added", "Added"), ("removed", "Removed")):
        if result[key]:
            out.append(f"{label} ({len(result[key])}): " + ", ".join(result[key]))
            out.append("")
    if not seeds:
        out.append("No existing requirement changed.")
    else:
        out.append("## Ripple")
        out.append("")
        for aid, old in seeds:
            r = ripples[aid]
            if aid not in data:
                out.append(f"### {aid} -- not in the export: rebuild the specification")
                out.append("")
                continue
            out += render_ripple(data, aid, r, old=old, cited=r["cited"], stale=r["stale"])
    sys.stdout.write("\n".join(out).rstrip() + "\n")


def cmd_ripple(data, ids, depth, directory, fmt):
    root = repo_root()
    spec_dir = os.path.relpath(os.path.abspath(directory), root)
    ripples = {}
    for aid in ids:
        if aid not in data:
            print(f"warning: unknown ID '{aid}'", file=sys.stderr)
            continue
        ripples[aid] = dict(ripple(data, aid, depth), cited=cited_in_files(root, aid, spec_dir))
    if fmt == "json":
        print(json.dumps(ripples, indent=2, ensure_ascii=False))
        return
    out = []
    for aid, r in ripples.items():
        out += render_ripple(data, aid, r, cited=r["cited"])
    sys.stdout.write("\n".join(out).rstrip() + "\n")


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("jobname")
    parser.add_argument("ids", nargs="*", help="Requirement ID(s) to fetch")
    parser.add_argument("--search", metavar="TERM", help="Search titles/bodies for TERM")
    parser.add_argument("--overview", action="store_true",
                        help="List the domains and the relationships between them")
    parser.add_argument("--changed", metavar="REV",
                        help="List the requirements changed since git revision REV, "
                             "and what each change reaches")
    parser.add_argument("--ripple", action="store_true",
                        help="List what a change to each given ID reaches")
    parser.add_argument("--depth", type=int, default=1, help="Traversal hops (default: 1)")
    parser.add_argument("--direction", choices=["up", "down", "both"], default="both")
    parser.add_argument("--format", choices=["md", "json"], default="md")
    parser.add_argument("--dir", default="docs",
                         help="Directory containing <jobname>-requirements.json (default: docs)")
    args = parser.parse_args()

    data    = load_requirements(args.jobname, args.dir)
    pending = load_pending(args.jobname, args.dir)

    if args.search:
        cmd_search(data, args.search)
        return

    if args.overview:
        cmd_overview(data, load_types(args.jobname, args.dir))
        return

    if args.changed:
        cmd_changed(data, args.changed, args.depth, args.dir, args.format)
        return

    if not args.ids:
        parser.error("provide at least one requirement ID, or use --search TERM, "
                     "--overview or --changed REV")

    if args.ripple:
        cmd_ripple(data, args.ids, args.depth, args.dir, args.format)
        return

    visited = collect_context(data, args.ids, args.depth, args.direction, pending=pending)

    if args.format == "json":
        print(json.dumps({aid: data[aid] for aid in visited if aid in data},
                          indent=2, ensure_ascii=False))
    else:
        sys.stdout.write(render_markdown(data, visited, args.ids))


if __name__ == "__main__":
    main()
