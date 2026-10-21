---
name: req-writing
description: Write SysML 1.x software/system requirements in this project's reqman.sty LaTeX format, applying EARS sentence patterns, INCOSE quality rules, verification-method classification, and correct traceability relationships. Use this skill whenever the user asks to write, draft, add, decompose, derive, split, or rewrite a requirement, use case, ADR, design artifact, or test case obligation — or mentions EARS, INCOSE, "shall" statements, requirement IDs (e.g. HLR-*, DFR-*, REQ-*, UC-*, ADR-*, DSN-*, TC-*), or the requirement LaTeX environment. Also use it when reviewing a draft the user wrote before it is committed.
---

# Writing SysML Requirements (reqman.sty workflow)

This project stores requirements as LaTeX `requirement` environments processed by
`reqman.sty` + `reqman-report.py`. A new requirement is not just prose: it is an
identified, typed, statused artifact with declared relationships and declared
downstream coverage obligations. Follow the workflow below in order — each step
feeds the next.

Companion skills: use `reqman-context` to fetch existing requirements before
writing (never guess parent text); use `req-checking` to audit the result.

## Workflow

1. **Fetch context first.** Before writing, retrieve the parent(s) and neighbors
   with `reqman-context` (or read the relevant `.tex` if the JSON export is
   unavailable). A derived requirement must respond to the actual parent text,
   not a remembered paraphrase. Also check whether the new ID was already
   forward-declared as the expected target of some other requirement's
   `\reqexpect` — `reqman-context <jobname> <new-ID>` reports this even
   before the ID exists (via `<jobname>-pending-coverage.json`); treat any
   note found there as binding scope for the draft, not just a suggestion.
2. **Classify** the requirement (level, stereotype, verification method).
3. **Choose an EARS pattern** and draft the statement.
4. **Apply the INCOSE quality pass** to the draft.
5. **Assign the ID, status, and stereotype.**
6. **Declare relationships** (`\deriveReqt`, `\refine`, …) and **coverage
   obligations** (`\reqcover`, `\reqexpect`, or `\reqterminal`).
7. **Emit the LaTeX** and remind the user to rebuild (`latexmk -pdf <jobname>`)
   so the tracing report and `<jobname>-requirements.json` pick it up.

### Editing an existing requirement

The same seven steps apply, but with one added risk: other artifacts may
already paraphrase the text you're changing. Before emitting the edit, grep
the whole document for the ID and for its key terms — not just
`\reqref{ID}` occurrences, since a plain-prose mention carries no link and
won't show up in `reqman-context`'s neighborhood. For every prose
restatement you find — an ADR's Context describing what this requirement
"requires", a TC's Objective/Steps describing the behavior, a sibling's
rationale — update it in the same change, or explicitly flag it for a
`req-checking` pass (rule S8, `../req-checking/SKILL.md`). A restatement
left unsynchronized will silently misstate a requirement it once accurately
described.

## Step 2 — Classification

### Level and prefix

Use the prefixes declared via `\reqmantype` in the target document. The default
mapping in this project:

| Prefix | Stereotype | Typical role |
|--------|-----------------------------|--------------|
| HLR | stakeholderRequirement | High-level / stakeholder need |
| UC | useCase | Behavioral scenario refining an HLR |
| FEAT | feature | Product feature |
| REQ / DFR | functionalRequirement | System/functional requirement |
| ADR | designConstraint | Architecture decision record |
| DSN | structuralRequirement | Design artifact |
| TC | verify | Test case (terminal) |

If the user's document declares different prefixes, honor those — check the
`\reqmantype` calls or the stereotype table in the tracing report. Never invent
a prefix without a matching `\reqmantype` declaration; undeclared prefixes are
flagged in red in the tracing report. If a new prefix is genuinely needed,
add the `\reqmantype{PREFIX}{stereotype}{Description}[terminal?]` declaration
as part of the change.

### Verification method (decide this before wording)

Classify how the requirement will be shown satisfied — this drives both the
wording precision and the coverage obligation:

- **Test** — behavior exercised against pass/fail criteria. Most functional
  requirements. Obligation: `\reqcover{verify}{TC}`.
- **Analysis** — shown by calculation, model, or simulation (e.g. timing
  budgets, capacity math). Quantitative wording with units is mandatory.
- **Demonstration** — observed qualitative behavior without instrumented
  measurement (e.g. "operator can complete X without the manual").
- **Inspection** — verified by examining artifacts (code review, document
  check, physical examination).

Qualitative requirements ("the cache shall be durable across restarts") are
legitimate — but they are verified by *test/demonstration*, so the wording must
define the scenario and observable outcome precisely enough for a test case to
be written. If you cannot sketch the verifying test in one sentence, the
requirement is not yet verifiable: sharpen it.

## Step 3 — EARS patterns

Pick the ONE pattern that matches the requirement's trigger structure (rule
ID **E1** in `req-checking`). The generic frame is: *[preconditions]
[trigger] the \<system\> shall \<response\>.* Full rationale and worked
examples: `../req-checking/references/ears-patterns.md`.

| Pattern | Template | Use when |
|---------|----------|----------|
| Ubiquitous | The \<system\> shall \<response\>. | Always active, no trigger |
| Event-driven | WHEN \<trigger\>, the \<system\> shall \<response\>. | Response to a discrete event |
| State-driven | WHILE \<state\>, the \<system\> shall \<response\>. | Active throughout a state |
| Unwanted behavior | IF \<undesired condition\>, THEN the \<system\> shall \<response\>. | Fault/error/abnormal handling |
| Optional feature | WHERE \<feature is included\>, the \<system\> shall \<response\>. | Product-line variability |
| Complex | WHILE \<state\>, WHEN \<trigger\>, the \<system\> shall \<response\>. | Combined conditions |

Rules of thumb (same rule IDs `req-checking` audits against):
- **E2** WHEN = edge (something happens); WHILE = level (something is true);
  IF/THEN is reserved for conditions you'd rather never occur.
- **E3** Keep trigger and response at the same abstraction level as the
  artifact you're writing — don't let an HLR's response reach down to
  implementation detail (that belongs in a derived DSN).
- **E4** Name the responding system element explicitly ("the drive
  controller shall"), not "the system" when a subsystem is meant, and never
  a dangling passive ("torque shall be removed").
- **E1** One EARS sentence per requirement. If you need two "shall"s, you
  need two requirements.

## Step 4 — INCOSE quality pass

Run the draft through the same rule IDs `req-checking` audits against — fix
every hit before emitting. Full rationale and examples:
`../req-checking/references/incose-rules.md`.

- **L1 Single obligation.** No "and"-chained obligations, no "and/or" — one
  requirement, one verifiable thought.
- **L2 Modal correctness.** "Shall" is the only binding modal. Reserve
  "will" for statements of fact, "should" for goals — and prefer not to use
  either in requirement bodies at all.
- **L3 Active voice, named subject.** Name the responsible element; never a
  dangling passive ("the message shall be encrypted") or "it shall be
  possible to X" with no actor.
- **L4 No vague terms.** Ban (non-exhaustive; full list in the reference):
  appropriate, adequate, fast, user-friendly, support, handle, minimize,
  robust, seamless, etc.
- **L5 Measurable values.** Numbers carry units and a bound type (max, min,
  tolerance, percentile) — "within 200 ms" needs to say which; "≥ 85 kWh"
  needs a source. TBD/TBS placeholders need an owner and date.
- **L6 No escape clauses.** "Where possible", "if practical", "as
  appropriate", "as required".
- **L7 Bounded negatives.** A "shall not X" needs a bounded, enumerable
  scope or it is unverifiable; prefer positive phrasing where feasible.
- **L8 No embedded rationale/design.** Rationale ("because", "in order to")
  and technology choices belong outside the shall-sentence — rationale in
  body prose, design in ADR/DSN artifacts linked by `\satisfy`/`\trace`.
- **L9 Consistent terminology.** One term per concept; reuse the exact terms
  of the parent requirement (retrieved in Step 1), and `\reqref` other
  requirements instead of restating their content.
- **L10 Pronoun/reference clarity.** "It", "this", "the data" must resolve
  unambiguously from the requirement plus its depth-1 context.

Also apply while drafting, though it's a semantic (not linguistic) check:
- **S3 Level appropriateness.** Every requirement-role artifact stays
  solution-free, not just the stakeholder-requirement level — "derived"
  means more concrete, not "design." Design choices belong in decision/
  design-role artifacts that `\satisfy`/`\trace` upward from it. Test the shall-sentence's main verb:
  if it names a mechanism ("reconstructed", "replicated", "cached") rather
  than the outcome that mechanism produces, more than one legitimate design
  could satisfy the literal wording — state the outcome/invariant instead
  and let the ADR pick the mechanism (worked example below). Likewise, an
  enumerated list of candidate approaches with an "adopted" choice belongs
  in that ADR's Alternatives-considered section, not the requirement body.

## Step 5 — ID, status, stereotype

ID grammar (enforced by reqman.sty, build error on violation):
`TYPE-SUBID-VERSION` with **exactly two hyphens**; SUBID may contain dots for
hierarchy. Examples: `DFR-LEND-01`, `HLR-PERSISTENCE.DURABLE-00`,
`REQ-ENCRYPT.ANONYMOUS-02`. Convention: VERSION starts at `00` or `01` and
increments on breaking rewrites; pick the next free SUBID by searching existing
IDs first (`reqman-context --search`).

Status is one of `draft` (default), `proposed`, `accepted`, `deprecated`,
`implemented`. New requirements are born `draft` unless the user says
otherwise. Never silently change the status of an existing requirement.

The stereotype (third, optional argument) may override the prefix default —
e.g. a `REQ-` that is really a performance requirement can carry
`[performanceRequirement]`. Only use stereotypes from the declared vocabulary.

Title length is constrained by the diagram cards: `reqman-report.py` renders
every artifact into a fixed-width box (`NODE_TIKZ_COL`, 2.2in at `\small`) in
every SysML diagram, with no wrapping or auto-shrink. Keep the Title argument
under about 36 characters where comfortable; past ~40 it gets tight against
the card border, and anything much longer will visibly overflow it. If a
precise title runs long, condense using a term the ID's own SUBID already
establishes (an ID ending `...UNSIZED-01` licenses "Unsized body..." in the
title) rather than dropping meaning.

## Step 6 — Relationships and coverage

All relationship macros are written **inside the child/source** artifact and
point at the target ID:

A terminology note on "Requirement" in the table below: it's SysML's
umbrella sense, which includes decision and design stereotypes
(`designConstraint`, `structuralRequirement`, `functionalRequirement`) —
not necessarily a document's own narrower `requirement` *kind* if it draws
one. Check the target document's own model before assuming "Requirement"
excludes its decisions or designs.

| Macro | Meaning (source → target) | Direction rule |
|-------|---------------------------|----------------|
| `\deriveReqt{ID}` | this req is derived from requirement ID | requirement-role → requirement-role, toward the more abstract/foundational originator (including a foundational decision a constraint derives from, per the document's own model) |
| `\refine{ID}` | this element clarifies requirement ID | e.g. a use case elaborating a stakeholder requirement |
| `\satisfy{ID}` | this design/impl element fulfils requirement ID | source must play a decision/design/implementation role, not a requirement role |
| `\verify{ID}` | this test case validates requirement ID | source plays the test-case role |
| `\trace{ID}` | general dependency | last resort when nothing fits |
| `\copies{ID}` | exact textual copy of master ID | text must match master verbatim |
| `\conflictsWith{ID}` | declared conflict | must come with a resolution plan in prose |

Derivation guidance:
- **Multi-source `\deriveReqt` is allowed and often right** — a child may
  synthesize several parents (e.g. a capacity figure from a performance parent
  plus a limit from a safety parent). When sources > 1, add a short rationale
  sentence in the body explaining how each parent contributes.
- If the child splits cleanly into independent obligations per parent, split it
  into separate requirements instead.
- More than ~4 parents suggests a missing intermediate level — raise it with
  the user rather than writing it.

Coverage — every non-terminal artifact must state what it expects downstream,
or the tracing report lists it as *unaccounted*:
- `\reqcover{refine}{UC}` — "a use case must refine me"
- `\reqcover{deriveReqt}{DFR}` — "a DFR must be derived from me"
- `\reqcover{satisfy}{DSN}` — "a design artifact must satisfy me"
- `\reqcover{verify}{TC}` — "a test case must verify me"
- `\reqexpect{rel}{FULL-ID}{note}` — "specifically FULL-ID must cover me via
  rel, and here is what it must specify." Use this instead of `\reqcover`
  when you already know the exact downstream ID — even if it doesn't exist
  yet, or exists but hasn't linked back — and what it needs to say. Unlike
  `\reqcover` (satisfied by *any* artifact of a class), this is satisfied
  only by that exact ID; the tracing report's "Planned Requirements" section
  lists every unmet one. Two typical uses: recording a decomposition plan
  before the children are written ("this HLR will be split into
  `DFR-CACHE.EVICT-01` and `DFR-CACHE.ADMIT-01`, each covering half of it");
  and needing *more than one* distinct artifact of the same class/relation,
  which class-level `\reqcover` cannot express — a second
  `\reqcover{verify}{TC}` on the same requirement is a no-op, since the
  first matching TC already satisfies it.
- `\reqterminal[reason]` — deliberately no further coverage; always give the
  reason.

Choose obligations from the verification method (Step 2) and the level: an HLR
typically demands refinement/derivation; a leaf functional requirement demands
`verify{TC}`; a TC is terminal by type.

## Step 7 — LaTeX emission

Template (arguments: `{ID}[status][stereotype]{Title}` then body):

```latex
\begin{requirement}{DFR-ESTOP.TORQUE-01}[draft][performanceRequirement]{Emergency-stop torque removal}
When the operator presses the emergency stop, the drive controller shall
remove motor torque within 200~ms.

\emph{Rationale:} derived jointly from \reqref{HLR-SAFETY-01} (operator
protection) and \reqref{HLR-DRIVE-02} (drivetrain performance envelope);
the 200~ms bound comes from the hazard analysis in \reqref{ADR-HAZOP-01}.

\deriveReqt{HLR-SAFETY-01}
\deriveReqt{HLR-DRIVE-02}
\trace{ADR-HAZOP-01}
\reqcover{satisfy}{DSN}
\reqcover{verify}{TC}
\end{requirement}
```

Body constraints from the tooling:
- No `\verb`/verbatim content — the body grab in reqman.sty cannot coexist
  with catcode changes. Use `\texttt{}`.
- `\textbf`, `\textit`, `\emph`, `\texttt`, `itemize`, `enumerate`, `\reqref`,
  `\rfcsec` all convert cleanly into the JSON export; obscure macros get
  unwrapped and logged — prefer the supported set.
- Inside the body, `\reqid` and `\reqdesc` expand to the ID and title — use
  them instead of repeating literals.
- Keep the normative statement as the first sentence(s); rationale, notes, and
  lists follow it.

### Citing source code

When a claim needs a concrete code reference (a trait method, a constant, a
call site), cite it with `\pgsrc{file}{line}{display}` (defined once in the
main `.tex`; links to the exact line on the pinned upstream version) wrapped
in a `\footnote{...}` attached to the relevant word or clause — that is this
project's established convention for source citations, not an inline
"Basis: ..." mention with no link.

Never guess a line number. Fetch or read the actual pinned-version source
before citing it; a wrong line number is worse than no citation at all.

## Worked example — rewriting a weak draft

User draft: *"The system should quickly restore its cache and be durable after
restarts if possible."*

Problems: "should", "quickly" (unmeasurable), "if possible" (escape clause),
two obligations (restore speed + durability), unnamed scenario.

Result — split into two, each in an EARS pattern with a defined verification
route:

```latex
\begin{requirement}{DFR-CACHE.DURABLE-01}[draft]{Cache durability across ungraceful restart}
When the service restarts following an ungraceful shutdown, the cache
subsystem shall restore every entry committed before the shutdown, without
corruption.
\deriveReqt{HLR-PERSISTENCE-01}
\reqcover{verify}{TC}
\end{requirement}

\begin{requirement}{DFR-CACHE.RESTORE-01}[draft][performanceRequirement]{Cache restore time}
When the service restarts, the cache subsystem shall complete cache
restoration within 30~s for a cache of up to 10~GB.
\deriveReqt{HLR-PERSISTENCE-01}
\deriveReqt{HLR-AVAILABILITY-01}
\reqcover{verify}{TC}
\end{requirement}
```

## Worked example — a mechanism word smuggled into the shall-sentence

User draft (already clean under L1–L10 and E1–E4 — grammatically it looks
fine): *"After a crash, the index shall be rebuilt from the write-ahead log
so that lookups reflect all committed writes."*

Problem (S3): "rebuilt from the write-ahead log" names one specific recovery
mechanism. A design that instead used continuous synchronous mirroring, or
periodic full snapshots, could equally satisfy the real need — that the
index's content matches what was actually committed — but would fail this
wording, because the wording quietly adopted WAL replay as the only
acceptable answer. Applying the word-level test: "could more than one
materially different implementation satisfy this exactly as written?" — no,
so the verb is design, not requirement.

Result — state the invariant any mechanism must produce, and leave the
mechanism to the ADR that satisfies this requirement:

```latex
\begin{requirement}{DFR-INDEX.CONSISTENT-01}[draft]{Index consistency after crash}
After a crash, the index shall reflect exactly the writes committed before
the crash: no committed write missing, no uncommitted write present.
\deriveReqt{HLR-PERSISTENCE-01}
\reqcover{satisfy}{ADR}
\reqcover{verify}{TC}
\end{requirement}
```

The ADR that later chooses WAL replay (or mirroring, or snapshotting)
records that decision in its own Decision section, with the rejected
options in Alternatives considered — not narrated back into the
requirement.

## After writing

Tell the user to rebuild (`latexmk -pdf <jobname>`) and check the tracing
report sections (Coverage Gaps, Planned Requirements, Unaccounted, Orphans)
— the new artifact will legitimately show open `$\square$` coverage boxes
until its downstream artifacts exist. If the requirement's own ID was the
target of someone else's `\reqexpect`, re-read that note against what you
just wrote before calling it done — `req-checking`'s S9 rule checks this
too, but catching a mismatch here is cheaper than a later audit finding.
Offer to run `req-checking` on the new text.

## External references

- [ISO/IEC/IEEE 29148-conformant SRS LaTeX template](https://github.com/bonellia/srs-ieee-latex)
  --- a general Software Requirements Specification *document* template
  (Introduction/References/Requirements/Verification/Appendices). Different
  scope from this skill: it structures a whole SRS document, while this
  project uses its own `reqman.sty` chapter structure rather than that
  outline. Useful as a cross-check for section-level SRS conventions
  (e.g. how a References or Verification section is normally organised),
  not as a template to follow directly for individual requirement text.
