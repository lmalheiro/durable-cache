---
name: reqman-context
description: Use this skill to fetch a requirement's full text, relationships, and coverage status from this project's traceability data (reqman.sty/reqman-report.py) instead of parsing .tex files. Trigger whenever a task references a requirement ID (e.g. DFR-COMPAT.UNSIZED-01, HLR-SCOPE.V1-01, ADR-EFFICIENCY.SINGLE-01) or needs requirements discovered by keyword before reading or editing related specs/code.
allowed-tools: Bash(python3 ./.claude/skills/reqman-context/scripts/reqman-context.py:*)
---

# reqman-context

This project tracks requirements as LaTeX `requirement` environments (see
`docs/tools/reqman.sty`) spread across files like `durable-cache-hlr.tex`,
`durable-cache-refine.tex`, `durable-cache-design.tex`,
`durable-cache-uc.tex`, `durable-cache-tc.tex`. Relationships
(`\refine`, `\satisfy`, `\verify`, `\trace`, `\deriveReqt`, `\copies`,
`\conflictsWith`), coverage obligations (`\reqcover{relation}{PREFIX}`, e.g.
`\reqcover{verify}{TC}` = "must be verified by a test case"; or
`\reqexpect{relation}{FULL-ID}{note}` = "specifically FULL-ID must cover me
via relation, and here is what it must specify" -- a forward-declared,
specific-ID obligation that may target an ID that doesn't exist yet), and
terminal markers (`\reqterminal[reason]` = "deliberately no further
coverage") are scattered across these files as macro calls.

Don't grep or read the `.tex` files to answer "what does requirement X say"
or "what does X depend on / what depends on X" -- use
`./.claude/skills/reqman-context/scripts/reqman-context.py` instead. It reads
`docs/<jobname>-requirements.json`, a consolidated export built by
`docs/tools/reqman-report.py`, and returns a requirement plus its linked
neighborhood as plain Markdown (no LaTeX markup, relationships already
resolved to titles).

## Layout

This skill is self-contained under `.claude/skills/reqman-context/`:
- `SKILL.md` -- this file
- `scripts/reqman-context.py` -- the query CLI used below

It reads a JSON export produced by the LaTeX-side toolchain, which lives
separately under `docs/tools/` (`reqman.sty`, `reqman-report.py`) because
that half is consumed by `latexmk`/`pdflatex` during the document build (see
`docs/latexmkrc`), not by agents. The two sides share the
`<jobname>-requirements.json` schema -- if you change one, check the other.

## Prerequisite: fresh data

`docs/<jobname>-requirements.json` (e.g. `docs/durable-cache-requirements.json`)
must exist and be up to date. It's regenerated automatically whenever the
document is built with `latexmk -pdf <jobname>` from `docs/` (see
`docs/latexmkrc`), or directly with:

```bash
(cd docs && python3 tools/reqman-report.py <jobname> build)
```

(`build` here is the report's output directory and must match `$build_dir`
in `docs/latexmkrc`.)

If the file is missing, or you suspect it's stale (recent `.tex` edits that
haven't been rebuilt yet), run one of the above first. Note: `latexmk -C
<jobname>` (full clean) deletes this file along with the PDF -- `latexmk -c
<jobname>` (minimal clean) intentionally keeps both, so prefer `-c` if you
need to clean up without losing query access.

## Usage

Run from the repository root. `<jobname>-requirements.json` is looked up in
`docs/` by default; pass `--dir` to override.

```bash
# Fetch a requirement plus its immediate parents/children (default depth 1)
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> <ID>

# Multiple IDs at once
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> <ID1> <ID2>

# Widen the traversal -- use sparingly, see "Choosing depth" below
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> <ID> --depth 2

# Only parents ("up": refines/satisfies/verifies/etc.) or only children ("down")
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> <ID> --direction up
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> <ID> --direction down

# Raw JSON instead of Markdown, for further programmatic use
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> <ID> --format json

# Don't know the ID? Search titles/bodies by keyword first
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> --search "eviction"
```

Example, from the repository root:

```bash
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py durable-cache HLR-SCOPE.V1-01
```

## Choosing depth

Default is `--depth 1`: the requirement itself plus its direct
relationships. That's almost always the right amount of context for working
on one requirement.

`--depth 2` pulls in second-order relationships too (parents-of-parents,
everything that satisfies a sibling artifact, etc.) and can balloon
quickly -- a single `--depth 2` call can return 5+ requirements and 100+
lines. Only reach for it when you genuinely need to trace further than one
hop, e.g. following a use case all the way down to its design artifacts.

## Output

Markdown (default) gives, per requirement: type/ID/title header, stereotype
and status, plain-prose body, resolved relationships (IDs *and* titles, not
bare IDs), a required-coverage checklist, and -- when applicable -- an
**Expected by (forward-declared)** line listing every `\reqexpect` some
other requirement recorded against this exact ID, met or not. It's the same
underlying data as the LaTeX traceability report, just scoped to what's
relevant and without LaTeX markup noise.

## Querying an ID that doesn't exist yet

If you fetch an ID before it's been written, the script also checks
`docs/<jobname>-pending-coverage.json` (written alongside
`<jobname>-requirements.json` whenever any `\reqexpect` targets an ID with
no artifact yet). Instead of a bare "unknown ID" warning, you get who
expects coverage from it, via which relation, and the note describing what
it must specify -- exactly the information needed before drafting it. This
is the natural first command to run when about to author a requirement:
```bash
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> <new-ID>
```
Any note surfaced this way should be treated as binding scope for the draft
(see `req-writing`), and re-checked against the finished text afterward
(`req-checking`'s S9 rule).
