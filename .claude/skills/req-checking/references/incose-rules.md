# INCOSE Rule Reference (expanded)

Expanded rationale and examples for the Layer-1 rule IDs in SKILL.md. Read this
when a finding is contested, borderline, or when the user asks *why* a rule
matters. Rules are distilled from the INCOSE Guide to Writing Requirements
(GtWR); IDs are local to this skill, not GtWR numbering.

## L1 — Single obligation

One requirement = one verifiable thought. Compound statements break
traceability (which half does the TC verify?), break status tracking (half
implemented?), and hide conflicts.

- Bad: "The controller shall log all faults and notify the operator within 5 s."
- Fix: two requirements sharing a parent; cross-reference with `\reqref` if the
  coupling matters.
- Borderline that is OK: a single obligation with an atomic compound *object*
  ("shall record the timestamp, fault code, and channel ID of each fault") —
  one action, one verification.

## L2 — Modal correctness

"Shall" = binding obligation, the only verifiable modal. "Will" = statement of
fact/intent about the environment. "Should" = goal, not verifiable. "May" =
permission. "Must" is banned to avoid a second binding modal. A document mixing
"shall/must/will" for obligations makes the binding set ambiguous — flag every
non-"shall" obligation, and every "shall" used for something that is not an
obligation (e.g. definitions).

## L3 — Active voice, named subject

Passive hides the responsible element: "the message shall be encrypted" — by
the sender, the transport, the store? Verification needs to know where to
probe. Also flag "It shall be possible to X" (capability without an actor) —
rewrite as "The \<element\> shall provide/perform X" or as an event-driven EARS
statement with the actor as trigger.

## L4 — No vague terms

Non-exhaustive ban list; treat as case-insensitive stems:
appropriate, adequate, sufficient, efficient, effective, user-friendly,
easy, simple, flexible, robust, reliable*, durable*, seamless, transparent,
fast, quick, timely, real-time (without a bound), support, handle, process
(as a vague verb), manage, minimize, maximize, optimize, improve, "etc.",
"and/or", "including but not limited to", state-of-the-art, best-practice.

\* "reliable"/"durable" are acceptable only when the same statement (or an
explicitly referenced one) defines the scenario and observable outcome — see
S2. Otherwise they are L4 findings.

## L5 — Measurable values

Every number needs a unit and a bound type (max, min, tolerance, percentile).
"Within 200 ms" — is that always, 99th percentile, mean? If the source doesn't
say, that is the finding. Unbounded superlatives ("as fast as possible") are
L5+L6 combined. TBD/TBS placeholders are allowed only with an owner and a
resolution date recorded in the body.

## L6 — No escape clauses

"Where possible", "if practical", "as appropriate", "as required", "to the
extent feasible" make failure unfalsifiable — any non-compliance can claim
impossibility. If feasibility is genuinely uncertain, the honest forms are: a
`draft`/`proposed` status, an explicit assumption in prose, or a derived trade
study (ADR) — not a hedge inside the shall-statement.

## L7 — Bounded negatives

"Shall not fail", "shall not allow unauthorized access" cannot be verified by
any finite test. Rewrite as positive, bounded behavior ("shall reject requests
lacking a valid token with HTTP 401") or as a bounded negative over an
enumerable set ("shall not expose ports other than 443"). Safety prohibitions
that must stay negative should carry the verification method Analysis or
Inspection explicitly.

## L8 — No embedded rationale/design

Rationale ("because", "in order to") inside the normative sentence invites
arguing with the reason instead of meeting the obligation, and design detail
("using PostgreSQL") over-constrains lower levels. Both belong outside the
shall-sentence: rationale in body prose, design in ADR/DSN artifacts linked by
`\satisfy`/`\trace`. At DSN level, technology mentions are fine — L8 is
level-sensitive; cross-check with S3.

## L9 — Consistent terminology

Each concept gets exactly one term, matching the parent and the glossary.
Synonym drift ("user"/"operator"/"actor" for the same person) creates phantom
distinctions that later readers will assume are intentional. When another
requirement's content is needed, `\reqref` it — restating creates a shadow copy
that will diverge (that is what `\copies` is for, when a literal copy is truly
needed). Cross-check with S8 when reviewing an artifact whose source
requirement has since changed.

## L10 — Pronoun/reference clarity

"It", "this", "the data", "the value" with more than one plausible antecedent.
The test: can each noun phrase be resolved by a reader who sees only this
requirement plus its depth-1 context? If not, name the element.

---

# Verification methods — decision aid (for S1)

| Method | Choose when | Wording implication |
|--------|-------------|---------------------|
| Test | Behavior can be exercised with pass/fail observation | Scenario + observable + bound must be explicit |
| Analysis | Property provable by calculation/model (worst-case timing, capacity, reliability math) | Inputs and assumptions of the analysis must be stated or referenced |
| Demonstration | Qualitative observation suffices, no instrumentation | Observable success criterion still required |
| Inspection | Static examination of an artifact | The artifact and the property inspected must be named |

A requirement is unverifiable (Critical) if none of the four methods can be
assigned; it is mismatched (Major) if its declared coverage contradicts the
assigned method (e.g. an analysis-only property demanding `verify{TC}` with no
test observable).
