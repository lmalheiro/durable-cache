---
name: req-checking
description: Audit and check requirements written in this project's reqman.sty LaTeX format, a requirement model inspired by SysML requirement diagrams, for linguistic quality (INCOSE rules, EARS conformance, ambiguity, verifiability), semantic soundness (compound statements, level mismatch, untestable wording), and structural/traceability correctness (relationship direction rules, deriveReqt fan-in, cycles, conflicts, coverage design). Use this skill whenever the user asks to check, review, audit, validate, lint, critique, or find problems in requirements, specs, use cases, ADRs, or test-case coverage — or asks "is this requirement good/verifiable/ambiguous", mentions traceability gaps, or wants a quality report over a spec document. Complements (does not repeat) the automated checks in reqman-report.py.
---

# Checking Requirements (reqman.sty workflow)

An audit has four layers: what the **tooling already checks** (don't redo it —
read its output), what needs **linguistic judgment** (INCOSE/EARS), what
needs **semantic/structural judgment** (meaning, levels, graph shape), and
what needs **presentation/tooling-fit judgment** (diagram rendering, citation
conventions). Work through them in that order and report findings in the
standard format at the end of this file.

Companion skills: `reqman-context` to fetch requirement text and neighborhoods;
`req-writing` to produce the corrected text for any finding.

## Layer 0 — Harvest the automated checks first

`reqman-report.py` (run via `latexmk -pdf <jobname>` or directly) already
detects, deterministically:

- **Orphans** — artifacts with no links at all.
- **Coverage gaps** — declared `\reqcover`/`\reqexpect` obligations with no
  matching incoming link (prefix + relation, or exact ID + relation).
- **Planned Requirements** — unmet `\reqexpect` obligations grouped by the
  specific target ID they forward-declare, whether or not that ID exists
  yet — the deterministic "what's still missing" list; import it rather
  than re-deriving it by grep.
- **Unaccounted artifacts** — non-terminal, non-deprecated artifacts that
  declare no `\reqcover`/`\reqexpect` at all.
- **Undeclared type prefixes** — IDs whose prefix has no `\reqmantype`.
- **ID grammar** — reqman.sty itself errors on IDs without exactly 2 hyphens.

So: ensure `<jobname>-requirements.json` is fresh (rebuild if `.tex` files are
newer), read the tracing-report data (or the JSON's `coverage` blocks via
`reqman-context`), and *import* these findings into the report rather than
re-deriving them. Everything below is what the tooling cannot judge.

## Layer 1 — Linguistic checks (per requirement body)

Apply to the normative statement of every in-scope requirement. Rule IDs are
for the findings report. Full rationale and more examples:
`references/incose-rules.md`.

| ID | Check | Typical trigger |
|----|-------|-----------------|
| L1 | Single obligation | "and", "and/or", multiple "shall" |
| L2 | Modal correctness | "should"/"will"/"must"/"may" where "shall" is meant, or mixed modals |
| L3 | Active voice, named subject | "It shall be possible…", "X shall be done" (by whom?) |
| L4 | No vague terms | appropriate, adequate, fast, user-friendly, support, handle, minimize, robust, seamless, etc., "etc." itself |
| L5 | Measurable values | numbers without units, tolerances, or bounds; "quickly", "high performance" |
| L6 | No escape clauses | "where possible", "if practical", "as appropriate", "as required" |
| L7 | Bounded negatives | "shall not X" with unbounded scope |
| L8 | No embedded rationale/design | "because", "in order to", "using \<technology\>" inside the shall-sentence at the stakeholder-requirement level |
| L9 | Consistent terminology | same concept under two names vs. the parent/glossary; restated (rather than `\reqref`'d) content of another requirement |
| L10 | Pronoun/reference clarity | "it", "this", "the data" with ambiguous antecedent |

## Layer 2 — EARS conformance

Full rationale, all six patterns, and worked examples:
`references/ears-patterns.md`.

- **E1** — The statement matches exactly one EARS pattern (Ubiquitous / WHEN /
  WHILE / IF-THEN / WHERE / Complex). Free-form statements that clearly have a
  trigger or state should be flagged with the suggested pattern.
- **E2** — WHEN vs WHILE vs IF used correctly: WHEN = discrete event, WHILE =
  sustained state, IF/THEN = unwanted condition. "When the system is in
  maintenance mode" is an E2 hit (state → WHILE).
- **E3** — Trigger and response are on the same abstraction level (no
  stakeholder-level trigger with register-level response).
- **E4** — The responding element is the most specific correct one (subsystem
  named where the requirement is allocated to a subsystem).

## Layer 3 — Semantic checks

- **S1 Verifiability route.** For each requirement, name the verification
  method (test / analysis / demonstration / inspection) implied by its wording
  and check it is consistent with its declared coverage (`\reqcover{verify}{TC}`
  present for testable leaves; terminal marker justified). A requirement whose
  verifying test cannot be sketched in one sentence fails S1 even if its
  grammar is perfect.
- **S2 Quantitative-where-quantifiable.** Qualitative wording ("durable",
  "reliable") is acceptable only when the scenario and observable outcome are
  defined; if a number is knowable (latency, capacity, rate), its absence is a
  finding.
- **S3 Level appropriateness.** Every requirement-role artifact — from
  stakeholder requirements through functional/base requirements — must stay
  solution-free; derived does not mean design. Design detail belongs in the
  decision/design-role artifacts that satisfy it (a `designConstraint`, or a
  design-stage `structuralRequirement`/`functionalRequirement`). Conversely,
  a design-role artifact that just restates its parent without adding
  design content is padding. Two concrete tests, not just the general
  principle:
  - *Word-level test:* for the shall-sentence's main verb/predicate, ask
    "could more than one materially different implementation satisfy this
    exactly as written?" A verb naming a mechanism ("reconstructed",
    "replicated", "logged", "cached") usually admits only the one
    implementation it names, even if grammatically clean and EARS-conformant.
    Restate it as the outcome/invariant that any such mechanism would have to
    produce, and let the mechanism live in the ADR that `\satisfy`s this
    requirement.
  - *Structural test:* an enumerated list of candidate approaches with a
    stated "adopted" choice, sitting inside a requirement's body, is ADR
    content that hasn't been moved yet — it belongs in that ADR's own
    Alternatives-considered section. The `\satisfy`/`\deriveReqt` links
    already carry the relationship structurally, so no narration of which
    ADR "resolves" the requirement is needed in the requirement body either.
- **S4 Parent responsiveness.** A derived/refining artifact must actually
  address its parent's content — fetch the parent via `reqman-context` and
  compare. Term drift between parent and child is an L9+S4 combined finding.
- **S5 Duplication / near-duplication.** Two requirements imposing the same
  obligation (use `reqman-context --search` on key terms). Legitimate copies
  must use `\copies` and match the master verbatim.
- **S6 Contradiction.** Requirements imposing incompatible obligations without
  a `\conflictsWith` declaration; and every declared `\conflictsWith` must have
  a visible resolution plan or an owner — an undocumented standing conflict is
  a finding at severity Major.
- **S7 Status sanity.** `implemented` with unmet verify coverage; `accepted`
  children of `draft` parents; links pointing at `deprecated` targets from
  non-deprecated sources.
- **S8 Downstream restatement currency.** When a requirement's normative
  wording or title changes, every *other* artifact that paraphrases its
  obligation in prose — not only the ones that `\reqref` it — must be
  re-checked against the new wording. Look for an ADR's Context describing
  what a parent requirement "requires", a TC's Objective/Steps describing
  the behavior it verifies, or a sibling's rationale: anywhere a
  restatement was used instead of a `\reqref`. This is the failure mode
  L9's `\reqref`-over-restating guidance exists to prevent — a restatement
  is a shadow copy, and shadow copies diverge silently when their source
  changes.
- **S9 Forward-declared coverage fulfillment.** A requirement's
  `expected_by` field (`reqman-context`, or the JSON export directly) lists
  every `\reqexpect{rel}{THIS-ID}{note}` some other requirement recorded
  against it, met or not. For each entry, compare the note's stated
  expectation against this requirement's actual body. A link existing
  (`"met": true`) only means the relationship graph is structurally
  complete — it says nothing about whether the content actually delivers
  what the note promised. Flag Major when the body doesn't address what was
  promised despite the link existing; flag Minor for partial or ambiguous
  overlap. This is the check that closes the loop opened by `\reqexpect`:
  the note is a commitment made before the artifact existed, and this rule
  is what verifies the commitment was kept.
- **S10 Decision completeness.** A design-role artifact (ADR) must document
  genuine alternatives it weighed, with rejection rationale — not just a
  Decision and Consequences with no live options recorded. First apply the
  project's own deletion test (removing the ADR should force a real
  trade-off to be re-argued, not just lose a blueprint) to confirm it's
  actually a decision; if it is, its alternatives must appear in *its own*
  Alternatives-considered section — not only implied in prose, and not
  documented under a different ADR's section for a decision that ADR didn't
  itself make. An ADR bundling several independently-arguable decisions
  (each with its own alternatives) is a candidate for splitting, the same
  way a compound requirement under L1 is a candidate for splitting.

## Layer 4 — Structural / graph checks

Direction and typing rules, borrowed from the semantics of SysML 1.x/2.0
requirement diagrams (`docs/tools/sysml-requirement-diagram-reference.md`)
and phrased in their stereotype vocabulary rather than any one document's
prefix letters, since
prefixes are a per-document `\reqmantype` mapping — check the target
document's own declarations before applying these. A stereotype alone
doesn't always settle an artifact's *role*, either: a document may reuse one
stereotype for two different roles (e.g. `functionalRequirement` for both a
stakeholder-derived functional requirement and a later functional design
elaborating a decision). Where that happens, resolve the role from the
document's own stated model (a "Refinement Model"/kind-taxonomy section, if
it has one) rather than the bare stereotype string.

Watch for a related, easy-to-miss trap: SysML's own "requirement" is broader
than the word suggests — decision and design stereotypes
(`designConstraint`, `structuralRequirement`, `functionalRequirement`) are
themselves requirement subtypes, so a generic reference table describing a
relation's target as "Requirement" already includes decisions and designs.
A document that draws its own narrower `requirement` *kind*, distinct from
`decision`/`design`, and names a separate umbrella term for the broader
sense (e.g. `artifact`) is layering project vocabulary on top of SysML, not
contradicting it. Resolve which sense is meant before treating a
target-kind mismatch as a finding.

- **G1** `\deriveReqt` connects one requirement-role artifact to another
  (`stakeholderRequirement`, `requirement`, `functionalRequirement`,
  `designConstraint`, `structuralRequirement`, ... per the document's
  declarations), and must point toward the more abstract or more
  foundational originating artifact: ordinary decomposition moves from a
  concrete requirement up to its broader parent (same-level sideways
  derivation, or a parent deriving from its own child, is a finding); a
  constraint that a foundational decision generates derives from that
  decision the same way, when the document's own model documents that
  pattern. No cycles through any mix of deriveReqt/refine.
- **G2** `\satisfy`'s source must play a decision, design, or implementation
  role (e.g. `designConstraint`, a design-stage `structuralRequirement`/
  `functionalRequirement`, or a terminal implementation artifact) — never
  one playing a requirement role, even if it happens to share a
  requirement-family stereotype; target must play a requirement role.
- **G3** `\verify`'s source should play the test-case role (the document's
  terminal verification artifact, typically stereotyped `verify`), or
  another terminal verification artifact; a requirement-role artifact
  "verifying" another is a misused relation.
- **G4** `\refine` is for elaboration by a non-requirement-role model element
  (e.g. a `useCase` elaborating a `stakeholderRequirement`); if a
  requirement-role-to-requirement-role link adds constraint/detail, it
  should be `\deriveReqt` instead.
- **G5** `\trace` overuse: a `\trace` where a specific relation fits is a
  Minor finding — trace links carry no semantics for coverage checking.
- **G6** Fan-in/fan-out heuristics: a child with >4 `\deriveReqt` parents, or a
  parent whose obligation is "covered" by one giant child, suggests a missing
  intermediate level or a compound child (cross-check with L1).
- **G7** Multi-parent derivation without rationale: when a requirement derives
  from ≥2 parents, its body should explain how each parent contributes.
- **G8** Coverage design: obligations should form a coherent chain per level
  (e.g. a stakeholder requirement refined by a use case, a functional/base
  requirement deriving from it, satisfied by a decision or design, and
  verified by a test case); a stakeholder requirement whose only obligation
  is verification, skipping decomposition, is worth questioning.

## Layer 5 — Presentation & citation checks

Project-tooling conventions, not INCOSE/EARS/SysML content rules — but
findings here still block a clean read of the document.

- **P1 Title fits the diagram card.** Every artifact renders into a
  fixed-width box in every diagram `reqman-report.py` draws (its
  `NODE_TIKZ_COL`, 2.2in at `\small`, no wrapping or auto-shrink). A Title
  much over ~36 characters gets visibly clipped against the card border —
  check any title that long by rendering the page (or compare against
  existing titles of known-good length). Condense using a term the ID's own
  SUBID already establishes rather than cutting meaning. Minor finding.
- **P2 Source-code references use footnoted `\pgsrc`.** A claim naming a
  concrete code symbol, file, or line should cite it via
  `\pgsrc{file}{line}{display}` wrapped in a `\footnote{...}` (this
  project's established citation convention), not a bare `\texttt{}`
  mention with no link, and not a generic "exact call sites... per the
  targeted version" placeholder once real line numbers are knowable. Minor
  finding, unless the cited line is verifiably wrong — then Major, since a
  false citation is worse than none.

## Procedure

1. Determine scope: whole document, one chapter/file, or listed IDs.
2. Refresh data (`latexmk -pdf <jobname>` or `python3 tools/reqman-report.py
   <jobname> build`), harvest Layer 0 findings from the tracing report/JSON.
3. For each in-scope requirement, fetch it with depth-1 context
   (`reqman-context <jobname> <ID>`), run Layers 1–3 and Layer 5; run Layer 4
   over the neighborhood as you go (graph-wide checks like cycles at the
   end). If the requirement's wording or title changed since it was last
   audited, also grep the whole document for its ID and for the superseded
   terms to catch prose restatements that carry no declared relationship
   (S8) — depth-1 context only surfaces graph-declared links, so a
   `\satisfy`/`\verify` source is already reached this way, but a
   plain-prose mention elsewhere is not.
4. Never rewrite requirements during the audit — report findings and proposed
   fixes; apply fixes only when the user asks (then via `req-writing`).

## Findings report format

Group by requirement, ordered by severity. Severities: **Critical** (wrong or
unverifiable obligation, contradiction, broken direction rule), **Major**
(ambiguity that changes meaning, missing rationale for multi-parent
derivation, S7 status issues, S9 note/content mismatch), **Minor** (style,
G5 trace overuse, terminology drift with obvious intent).

```
## Audit: <scope> — <date>
### Summary
<N requirements audited; C critical / M major / m minor; Layer-0 imports: ...>

### <ID> — <title>   [status, stereotype]
- [Critical|Major|Minor] <RuleID>: <finding, quoting the offending words>
  Fix: <concrete proposed rewording or relationship change>
```

Every finding quotes the exact offending words and proposes a concrete fix —
"L4: vague term" alone is not actionable; "L4: 'quickly' — propose 'within
30 s for caches up to 10 GB' (bound TBD by owner)" is.

Close the report with the three most valuable improvements if the user can
only do three things.
