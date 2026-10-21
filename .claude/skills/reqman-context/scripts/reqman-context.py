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

<jobname>-requirements.json is looked up in --dir (default: "docs"), since
that's where this project's LaTeX build (see docs/latexmkrc) writes it
alongside the PDF.
"""
import argparse
import json
import os
import sys

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


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("jobname")
    parser.add_argument("ids", nargs="*", help="Requirement ID(s) to fetch")
    parser.add_argument("--search", metavar="TERM", help="Search titles/bodies for TERM")
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

    if not args.ids:
        parser.error("provide at least one requirement ID, or use --search TERM")

    visited = collect_context(data, args.ids, args.depth, args.direction, pending=pending)

    if args.format == "json":
        print(json.dumps({aid: data[aid] for aid in visited if aid in data},
                          indent=2, ensure_ascii=False))
    else:
        sys.stdout.write(render_markdown(data, visited, args.ids))


if __name__ == "__main__":
    main()
