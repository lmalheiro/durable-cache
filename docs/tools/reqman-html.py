#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The durable-cache authors
# SPDX-License-Identifier: Apache-2.0

"""Render the specification as one self-contained HTML page, for reading.

The PDF is the specification's canonical form; this is a reading copy. It
exists because a browser's reader mode (Firefox's, Safari's) wants an
article: headings, paragraphs, lists, and little else. So this does not try
to reproduce the PDF. It drops what reader mode would strip anyway -- the
requirement diagrams, the sequence diagrams, the page furniture -- and keeps
the prose, in document order, with every cross-reference a link.

It reads what the LaTeX build already produced rather than parsing LaTeX
again: `<jobname>-requirements.json` carries all 100 artifacts with their
bodies already converted to Markdown, their relationships, and the file and
line each was written at. The narrative between the requirements -- the
Introduction, the System Context, the appendices -- is converted with
`reqman-report.py`'s own `strip_latex`, so both halves of the page go
through one converter and read alike.

Usage:

    python3 tools/reqman-html.py [--jobname durable-cache] [-o OUT.html]

Run it after `latexmk -pdf <jobname>`, since it needs that run's JSON.
"""

from __future__ import annotations

import argparse
import html
import importlib.util
import json
import os
import re
import sys
from datetime import date, timezone, datetime

HERE = os.path.dirname(os.path.abspath(__file__))
DOCS = os.path.dirname(HERE)


def _load_reqman():
    """Import `reqman-report.py` for its LaTeX-to-Markdown converter.

    The hyphen in the filename keeps it from being an ordinary import, and
    reusing it matters more than the awkwardness: the narrative and the
    requirement bodies must come out in the same dialect or the page reads
    like two documents.
    """
    path = os.path.join(HERE, "reqman-report.py")
    spec = importlib.util.spec_from_file_location("reqman_report", path)
    if spec is None or spec.loader is None:
        sys.exit(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


REQMAN = _load_reqman()

# Artifact-ID shape, e.g. FDS-STORAGE.EXECUTOR-01. Used both to link
# cross-references and to recognise a requirement's own heading.
ID_RE = re.compile(r"\b([A-Z]{2,4}-[A-Z0-9]+(?:\.[A-Z0-9]+)?-\d{2})\b")

# Environments that carry no prose: figures are diagrams, and the diagram
# macros expand to nothing readable.
DROP_ENVIRONMENTS = ("figure", "tikzpicture", "sequencediagram", "adjustbox", "subfigure")
# `table` is a float around content worth keeping, so it is unwrapped
# rather than dropped; its caption becomes an ordinary paragraph.
UNWRAP_ENVIRONMENTS = ("table", "center")
DROP_MACROS = (
    "localreqdiagram",
    "localreqdiagrams",
    "reqdiaglink",
    "includetracingreport",
    "reqmantype",
    "reqmanlabel",
    "reqmanoutdir",
    "label",
    "centering",
    "clearpage",
    "renewcommand",
    "arraybackslash",
    "raggedright",
    "small",
    "hline",
)


SUPERSCRIPT = str.maketrans("0123456789+-n", "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻ⁿ")

MATH_WORDS = {
    r"\\rightarrow": "→",
    r"\\leftarrow": "←",
    r"\\to": "→",
    r"\\times": "×",
    r"\\approx": "≈",
    r"\\leq": "≤",
    r"\\geq": "≥",
    r"\\sigma": "σ",
    r"\\mu": "μ",
    r"\\cdot": "·",
    r"\\ldots": "…",
    r"\\dots": "…",
}


def _math_span(expr: str) -> str:
    """One `$...$` span as readable text.

    The document's mathematics is a handful of magnitudes, one Greek letter
    and some arrows, so Unicode carries all of it. That keeps the page plain
    text, which is what a reader mode wants -- no MathML to strip, nothing
    that renders as a broken glyph when the styles are thrown away.
    """
    out = expr
    # Digit grouping (`65{,}536`) first: its braces would otherwise defeat
    # the brace matching of every rule below it.
    out = out.replace("{,}", ",")
    out = re.sub(r"\\sqrt\s*\{([^{}]*)\}", r"√(\1)", out)
    for macro, glyph in MATH_WORDS.items():
        out = re.sub(macro + r"\b", glyph, out)
    # Superscripts: 10^7, 10^{7}.
    out = re.sub(
        r"\^\{([^{}]*)\}|\^(\S)",
        lambda m: (m.group(1) or m.group(2)).translate(SUPERSCRIPT),
        out,
    )
    out = out.replace("\\,", "\u2009").replace("\\%", "%")
    out = re.sub(r"[{}]", "", out)
    return out.strip()


def math_to_text(text: str) -> str:
    """Replace every inline `$...$` with its readable form."""
    return re.sub(r"\$([^$]*)\$", lambda m: _math_span(m.group(1)), text)


def read(path: str) -> str:
    with open(path, encoding="utf-8") as handle:
        return handle.read()


# ── LaTeX narrative → Markdown ────────────────────────────────────────────


def drop_environment(text: str, name: str) -> str:
    """Remove every `\\begin{name}...\\end{name}`, nesting included."""
    out = []
    i = 0
    begin = "\\begin{" + name
    while True:
        start = text.find(begin, i)
        if start == -1:
            out.append(text[i:])
            return "".join(out)
        out.append(text[i:start])
        depth = 0
        j = start
        while j < len(text):
            if text.startswith("\\begin{" + name, j):
                depth += 1
                j += len("\\begin{" + name)
            elif text.startswith("\\end{" + name + "}", j):
                depth -= 1
                j += len("\\end{" + name + "}")
                if depth == 0:
                    break
            else:
                j += 1
        i = j


def strip_requirement_blocks(text: str) -> str:
    """Drop the requirement environments; they come from the JSON instead."""
    return drop_environment(text, "requirement")


def extract_tables(tex: str, ids: set[str]) -> tuple[str, list[str]]:
    """Pull out `tabular`/`xltabular` bodies as HTML, leaving a placeholder.

    The narrative carries one table -- the legend of relationship arrows in
    the Introduction -- and dropping it would lose the key to every
    relationship the document then uses. `strip_latex` has no notion of a
    table, so it is converted here and stitched back in afterwards.
    """
    tables: list[str] = []

    def one(match: re.Match[str]) -> str:
        inner = match.group("body")
        # `tabularx`/`xltabular` take a width before the column spec, so
        # strip leading brace groups until the rows begin.
        for _ in range(2):
            trimmed = re.sub(r"^\s*\{[^{}]*(\{[^{}]*\}[^{}]*)*\}", "", inner, count=1)
            if trimmed == inner:
                break
            inner = trimmed
        rows_html = []
        for raw_row in re.split(r"\\\\", inner):
            raw_row = re.sub(r"\\(hline|toprule|midrule|bottomrule|endhead)\b", "", raw_row)
            cells = [c.strip() for c in raw_row.split("&")]
            if not any(cells):
                continue
            tag = "th" if not rows_html else "td"
            cooked = [REQMAN.strip_latex(math_to_text(c)) for c in cells]
            if not any(cooked):
                continue
            rows_html.append(
                "<tr>"
                + "".join(f"<{tag}>{inline(c, ids)}</{tag}>" for c in cooked)
                + "</tr>"
            )
        tables.append("<table>" + "".join(rows_html) + "</table>")
        return f"\n\n@@TABLE{len(tables) - 1}@@\n\n"

    pattern = re.compile(
        r"\\begin\{(?P<env>tabularx|xltabular|tabular)\}(?P<body>.*?)\\end\{(?P=env)\}",
        re.S,
    )
    return pattern.sub(one, tex), tables


def narrative_to_markdown(tex: str) -> str:
    tex = math_to_text(tex)
    for env in DROP_ENVIRONMENTS:
        tex = drop_environment(tex, env)
    for env in UNWRAP_ENVIRONMENTS:
        tex = re.sub(r"\\begin\{" + env + r"\}(\[[^\]]*\])?", "", tex)
        tex = tex.replace("\\end{" + env + "}", "")
    # Comment lines, but not an escaped percent.
    tex = re.sub(r"(?<!\\)%.*", "", tex)
    for macro in DROP_MACROS:
        tex = re.sub(r"\\" + macro + r"\*?(\[[^\]]*\])?(\{[^{}]*\})*", "", tex)
    tex = re.sub(r"\\section\*?\{([^{}]*)\}", r"\n\n### \1\n\n", tex)
    tex = re.sub(r"\\subsection\*?\{([^{}]*)\}", r"\n\n#### \1\n\n", tex)
    return REQMAN.strip_latex(tex)


# ── Markdown → HTML ───────────────────────────────────────────────────────


def typography(text: str) -> str:
    """LaTeX's ASCII dashes and quotes as the characters they stand for.

    The PDF renders `---` as an em dash; left as three hyphens on a page
    that reader mode strips to plain text, it just looks like a typo.
    """
    out = text.replace("---", "\u2014").replace("``", "\u201c")
    out = re.sub(r"(?<!-)--(?!-)", "\u2013", out)
    return out.replace("''", "\u201d")


def inline(text: str, ids: set[str], self_id: str | None = None) -> str:
    """Escape, then apply the inline marks `strip_latex` emits."""
    out = html.escape(text, quote=False)
    # `code` first: its content must not be re-marked afterwards.
    chunks = re.split(r"(`[^`]*`)", out)
    for k, chunk in enumerate(chunks):
        if chunk.startswith("`") and chunk.endswith("`") and len(chunk) >= 2:
            chunks[k] = "<code>" + chunk[1:-1] + "</code>"
            continue
        chunk = typography(chunk)
        chunk = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", chunk)
        chunk = re.sub(r"(?<![\w*])_([^_]+)_(?![\w*])", r"<em>\1</em>", chunk)
        chunks[k] = chunk
    out = "".join(chunks)

    def link(match: re.Match[str]) -> str:
        found = match.group(1)
        if found not in ids or found == self_id:
            return found
        return f'<a href="#{found}">{found}</a>'

    # Not inside an <a> or <code> we just produced.
    parts = re.split(r"(<code>.*?</code>|<a [^>]*>.*?</a>)", out, flags=re.S)
    for k, part in enumerate(parts):
        if not part.startswith(("<code>", "<a ")):
            parts[k] = ID_RE.sub(link, part)
    return "".join(parts)


def split_absorbed_bullets(md: str) -> str:
    """Put a list's first item on its own line.

    `strip_latex` runs the lead-in sentence and the first item together, so
    `...as one unit: - lookup ...` arrives as one line with the rest of the
    items below it. Left alone, the first item reads as part of the
    paragraph and the list starts at the second. The split is only made when
    the next line really is an item, which is the only shape this produces.
    """
    lines = md.split("\n")
    out: list[str] = []
    for index, line in enumerate(lines):
        following = lines[index + 1] if index + 1 < len(lines) else ""
        if not line.startswith("- ") and following.startswith("- ") and " - " in line:
            head, _, tail = line.partition(" - ")
            if head.strip():
                out.append(head)
            out.append("- " + tail)
        else:
            out.append(line)
    return "\n".join(out)


def markdown_to_html(md: str, ids: set[str], self_id: str | None = None) -> str:
    """The small subset `strip_latex` emits: paragraphs, bullets, headings."""
    md = split_absorbed_bullets(md)
    blocks: list[str] = []
    bullets: list[str] = []

    def flush_bullets() -> None:
        if bullets:
            items = "".join(f"<li>{inline(b, ids, self_id)}</li>" for b in bullets)
            blocks.append(f"<ul>{items}</ul>")
            bullets.clear()

    for raw_line in md.split("\n"):
        line = raw_line.rstrip()
        if not line.strip():
            flush_bullets()
            continue
        heading = re.match(r"^(#{3,4})\s+(.*)$", line)
        if heading:
            flush_bullets()
            level = len(heading.group(1))
            blocks.append(
                f"<h{level}>{inline(heading.group(2), ids, self_id)}</h{level}>"
            )
            continue
        bullet = re.match(r"^[-*]\s+(.*)$", line)
        if bullet:
            bullets.append(bullet.group(1))
            continue
        if bullets:
            # A wrapped continuation of the bullet above.
            bullets[-1] += " " + line.strip()
            continue
        blocks.append(f"<p>{inline(line.strip(), ids, self_id)}</p>")
    flush_bullets()
    return "\n".join(blocks)


# ── The document ──────────────────────────────────────────────────────────

REL_LABELS = {
    "derives_from": "Derives from",
    "derived_by": "Derived by",
    "refines": "Refines",
    "refined_by": "Refined by",
    "satisfies": "Satisfies",
    "satisfied_by": "Satisfied by",
    "verifies": "Verifies",
    "verified_by": "Verified by",
    "traces_to": "Traces to",
    "traced_by": "Traced by",
    "conflicts_with": "Conflicts with",
    "copies": "Copies",
    "copied_by": "Copied by",
}


def chapter_order(master_tex: str) -> list[tuple[str, str | None]]:
    """(`\\input` stem, enclosing chapter title) in the master's own order."""
    order: list[tuple[str, str | None]] = []
    pending: str | None = None
    in_appendix = False
    for line in master_tex.split("\n"):
        stripped = line.strip()
        if stripped.startswith("%"):
            continue
        if stripped.startswith("\\appendix"):
            in_appendix = True
        chapter = re.match(r"\\chapter\{(.*)\}", stripped)
        if chapter:
            pending = REQMAN.strip_latex(math_to_text(chapter.group(1)))
            pending = pending.replace("`", "")
            if in_appendix:
                pending = "Appendix: " + pending
            continue
        included = re.match(r"\\input\{([^}]*)\}", stripped)
        if included:
            order.append((included.group(1), pending))
            pending = None
    return order


def narrative_html(tex: str, ids: set[str]) -> list[str]:
    """A span of chapter prose as HTML blocks, tables restored."""
    without_reqs = strip_requirement_blocks(tex)
    staged, tables = extract_tables(without_reqs, ids)
    markdown = narrative_to_markdown(staged)
    if not markdown.strip():
        return []
    out = markdown_to_html(markdown, ids)
    for index, table in enumerate(tables):
        out = out.replace(f"<p>@@TABLE{index}@@</p>", table)
        out = out.replace(f"@@TABLE{index}@@", table)
    return [out]


def requirement_body_markdown(aid: str, art: dict, docs_dir: str, jobname: str) -> str:
    """The requirement's prose, preferring the build's raw body file.

    The JSON body has already been through `strip_latex`, which drops the
    maths macros it does not know -- `$\\sigma \\approx 39$` arrives as
    `$39$`. `build/<jobname>-reqbody-<ID>.tex` is the same body before that
    pass, so converting the maths first and stripping afterwards keeps it.
    """
    raw_path = os.path.join(docs_dir, "build", f"{jobname}-reqbody-{aid}.tex")
    if os.path.exists(raw_path):
        try:
            return REQMAN.strip_latex(math_to_text(read(raw_path)))
        except Exception:  # noqa: BLE001 - fall back rather than fail the page
            pass
    return math_to_text(art.get("body") or "")


def requirement_html(
    aid: str, art: dict, ids: set[str], docs_dir: str, jobname: str
) -> str:
    title = html.escape(art.get("title") or aid, quote=False)
    label = html.escape(art.get("reqtype_label") or art.get("type") or "", quote=False)
    status = html.escape(art.get("status") or "", quote=False)
    parts = [f'<section class="req" id="{aid}">']
    parts.append(f"<h3>{html.escape(aid, quote=False)} &mdash; {title}</h3>")
    meta = f"<em>{label}</em>" if label else ""
    if status:
        meta = f"{meta} &middot; status: {status}" if meta else f"status: {status}"
    if meta:
        parts.append(f'<p class="meta">{meta}</p>')
    body = requirement_body_markdown(aid, art, docs_dir, jobname)
    if body:
        parts.append(markdown_to_html(body, ids, self_id=aid))

    rels = art.get("relationships") or {}
    shown = []
    for key, heading in REL_LABELS.items():
        targets = rels.get(key)
        if not targets:
            continue
        links = ", ".join(
            f'<a href="#{t}">{html.escape(t, quote=False)}</a>'
            if t in ids
            else html.escape(t, quote=False)
            for t in targets
        )
        shown.append(f"<li><strong>{heading}:</strong> {links}</li>")
    if shown:
        parts.append('<ul class="rels">' + "".join(shown) + "</ul>")
    parts.append("</section>")
    return "\n".join(parts)


def build(jobname: str, docs_dir: str) -> str:
    json_path = os.path.join(docs_dir, f"{jobname}-requirements.json")
    if not os.path.exists(json_path):
        sys.exit(
            f"{json_path} not found -- build the specification first:\n"
            f"    cd {docs_dir} && latexmk -pdf {jobname}"
        )
    artifacts: dict = json.load(open(json_path, encoding="utf-8"))
    ids = set(artifacts)

    master = read(os.path.join(docs_dir, f"{jobname}.tex"))
    title_match = re.search(r"\\title\{(.*?)(?:%|\\\\)", master, re.S)
    doc_title = (
        REQMAN.strip_latex(title_match.group(1)).strip() if title_match else jobname
    )

    # Where each requirement was written, so the page keeps document order.
    by_file: dict[str, list[tuple[int, str]]] = {}
    for aid, art in artifacts.items():
        src = art.get("source") or {}
        if src.get("file"):
            by_file.setdefault(src["file"], []).append((src.get("line", 0), aid))
    for entries in by_file.values():
        entries.sort()

    body_parts: list[str] = []
    toc: list[str] = []

    for stem, chapter_title in chapter_order(master):
        path = os.path.join(docs_dir, stem + ".tex")
        if not os.path.exists(path):
            continue
        tex = read(path)
        if chapter_title:
            anchor = re.sub(r"[^a-z0-9]+", "-", chapter_title.lower()).strip("-")
            body_parts.append(
                f'<h2 id="{anchor}">{html.escape(chapter_title, quote=False)}</h2>'
            )
            toc.append(
                f'<li><a href="#{anchor}">'
                f"{html.escape(chapter_title, quote=False)}</a></li>"
            )

        lines = tex.split("\n")
        here = by_file.get(os.path.basename(path), [])
        # Walk the chapter, emitting narrative up to each requirement and
        # then the requirement itself from the JSON.
        cursor = 0
        for line_no, aid in here:
            before = "\n".join(lines[cursor : line_no - 1])
            body_parts.extend(narrative_html(before, ids))
            body_parts.append(
                requirement_html(aid, artifacts[aid], ids, docs_dir, jobname)
            )
            # Skip past this requirement's own environment.
            end = line_no - 1
            while end < len(lines) and not lines[end].strip().startswith(
                "\\end{requirement}"
            ):
                end += 1
            cursor = end + 1
        tail = "\n".join(lines[cursor:])
        body_parts.extend(narrative_html(tail, ids))

    generated = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    counts: dict[str, int] = {}
    for art in artifacts.values():
        counts[art.get("type", "?")] = counts.get(art.get("type", "?"), 0) + 1
    census = ", ".join(f"{n} {t}" for t, n in sorted(counts.items()))

    return TEMPLATE.format(
        title=html.escape(doc_title, quote=False),
        generated=generated,
        census=html.escape(census, quote=False),
        total=len(artifacts),
        toc="".join(toc),
        body="\n".join(body_parts),
    )


TEMPLATE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="Reading copy of the specification; the PDF is canonical.">
<style>
  :root {{ color-scheme: light dark; }}
  body {{ margin: 0 auto; max-width: 46rem; padding: 2rem 1rem 6rem;
         font: 1rem/1.65 Georgia, "Times New Roman", serif; }}
  h1 {{ font-size: 1.9rem; line-height: 1.25; }}
  h2 {{ margin-top: 3rem; font-size: 1.5rem; border-bottom: 1px solid #8884;
        padding-bottom: .3rem; }}
  h3 {{ margin-top: 2.2rem; font-size: 1.15rem; }}
  h4 {{ margin-top: 1.6rem; font-size: 1rem; }}
  code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
          font-size: .9em; }}
  .meta {{ margin-top: -.6rem; font-size: .88rem; opacity: .75; }}
  .rels {{ font-size: .88rem; opacity: .85; }}
  .subtitle {{ opacity: .75; }}
  nav {{ font-size: .95rem; }}
  nav ul {{ padding-left: 1.2rem; }}
  a {{ color: inherit; }}
</style>
</head>
<body>
<article>
<h1>{title}</h1>
<p class="subtitle">Reading copy, generated {generated} from the LaTeX sources.
The PDF is the canonical form: it carries the requirement and sequence
diagrams and the traceability report, which this page leaves out.
{total} artifacts &mdash; {census}.</p>
<nav><ul>{toc}</ul></nav>
{body}
</article>
</body>
</html>
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobname", default="durable-cache")
    parser.add_argument("--docs-dir", default=DOCS)
    parser.add_argument("-o", "--output", default=None)
    args = parser.parse_args()

    page = build(args.jobname, args.docs_dir)
    out = args.output or os.path.join(args.docs_dir, f"{args.jobname}.html")
    with open(out, "w", encoding="utf-8") as handle:
        handle.write(page)
    size = os.path.getsize(out)
    print(f"wrote {out} ({size / 1024:.0f} KiB)")


if __name__ == "__main__":
    main()
