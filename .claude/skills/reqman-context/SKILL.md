---
name: reqman-context
description: Use this skill to fetch a requirement's full text, relationships, and coverage status from this project's traceability data (reqman.sty/reqman-report.py) instead of parsing .tex files, and to trace a change to the requirements -- which ones changed since a revision, whether each took a new version, and what each change reaches. Trigger whenever a task references a requirement ID (e.g. DFR-COMPAT.UNSIZED-01, HLR-SCOPE.V1-03, ADR-EFFICIENCY.SINGLE-01), needs requirements discovered by keyword before reading or editing related specs/code, or changes a requirement.
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

# The map: every domain, how many artifacts of each type it holds, and the
# relationships between domains (trace left out)
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> --overview

# The requirements changed since a git revision, and what each change
# reaches (see "Tracing a change" below)
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> --changed main

# What a change to given IDs reaches, without a revision to compare with
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> --ripple <ID> [--depth 2]
```

Example, from the repository root:

```bash
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py durable-cache HLR-SCOPE.V1-03
```

## Getting oriented

An ID names its *domain* between the type prefix and the first dot:
`HLR-PERSISTENCE.DURABLE-01` belongs to PERSISTENCE. `--overview` prints the
domain overview the PDF shows in its Introduction ("Domains") as two short
Markdown tables: each domain with its artifact counts by type, and the
relationships from one domain's artifacts to another's, counted by kind.
Run it first when a task spans several requirements or you don't yet know
where a concern lives; then fetch the IDs you need. For the impact of a
change, follow the relationships instead: `--ripple` (see "Tracing a
change") lists what a change to an artifact reaches, and `--direction down
--depth N` everything that refines, satisfies or verifies it, N hops deep.

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

## Tracing a change

An existing requirement whose text a pull request changes takes a new
version: the last part of its ID goes up by one (`HLR-PERSISTENCE.DURABLE-01`
would become `HLR-PERSISTENCE.DURABLE-02`; see `req-writing`, step 5), and
every reference to it
follows. New requirements start at `-01`. `--changed` checks that rule and
shows what each change reaches:

```bash
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py <jobname> --changed <base of the pull request>
```

It compares the requirement environments of the `.tex` files the export
names, at that git revision and in the working tree, whitespace aside, and
lists:

- **New versions**: an ID that left as `TYPE-SUBID-NN` and is back with a
  higher `NN`. One whose text did not change is flagged: nothing calls for it.
- **Changed without a new version**, each with the ID it should take. A
  change of status alone does not count, nor does an edit that only follows
  another requirement's new version.
- **Added** and **removed** IDs.

Then, for every changed requirement, the ripple:

- *Built on it*: what refines, derives from, satisfies, verifies, copies or
  traces it. Review each against the new text. Where it no longer holds,
  change it too, and it takes a new version of its own; where it holds,
  only its reference changes.
- *Further down*, with `--depth N`: what is built on those in turn.
- *It builds on*: what it refines, satisfies, verifies or traces. Check that
  it still serves each, as their texts ask.
- *Named in the text of, without a link*: requirements whose prose names it.
  Check what they say of it.
- *Cited outside the specification*: tracked files that name it, from
  `git grep`: code comments, `IMP` markers, tests, the README, CI.
- *STILL CITING* the old ID: files the rename has not reached. A test case's
  test functions (`tc_<id>_…`) and its `IMP-TC.` marker carry its ID, which
  `cargo xtask trace-report` matches them by, so they follow its new version.

The ripple reads the relationships from the export: rebuild the
specification after renaming, so that it knows the new IDs. The work is done
when `--changed` against the base reports no change without a new version
and no file still citing an old ID. `--ripple <ID>…` gives the same report
for chosen IDs, without a revision to compare with.
