# EARS Pattern Reference (expanded)

Expanded rationale and examples for the Layer-2 rule IDs in SKILL.md. Read
this when a pattern match is contested, when a free-form statement needs a
suggested rewrite, or when the user asks *why* WHEN/WHILE/IF differ. EARS
("Easy Approach to Requirements Syntax") was introduced by Mavin et al.
(Rolls-Royce, REFSQ 2009); IDs (E1-E4) are local to this skill, not EARS's
own numbering — EARS defines patterns, not a rule-ID scheme. `req-writing`
Step 3 has the same six patterns as a compact drafting table; this file is
the auditor's expanded version, with the confusions that actually show up.

## The six patterns

Every EARS statement ends in `the <system> shall <response>`. What precedes
it is exactly one of the six forms below — never a free mix (that is E1).

### 1. Ubiquitous — no trigger, always active

`The <system> shall <response>.`

Use for invariants and standing properties, not for reactions to anything.

> The persistence layer shall encrypt cached response bodies at rest.

### 2. Event-driven — WHEN

`WHEN <trigger>, the <system> shall <response>.`

Use when the response fires off a discrete, instantaneous event — something
that *happens* at a point in time.

> WHEN a client sends a request with an `If-None-Match` header matching the
> cached ETag, the cache shall respond `304 Not Modified`.

### 3. State-driven — WHILE

`WHILE <state>, the <system> shall <response>.`

Use when the response holds *throughout* a condition that has duration —
something that *is true*, not something that happens.

> WHILE the cache is in degraded-storage mode, the eviction manager shall
> reject new admissions.

### 4. Unwanted behavior — IF...THEN

`IF <trigger>, THEN the <system> shall <response>.`

Reserved for undesired, abnormal, or fault conditions — the one pattern
with two keywords, marking the condition as something the system reacts to
as a problem, not as routine operation.

> IF the on-disk object's stored checksum does not match its content, THEN
> the cache shall evict the entry and log an integrity fault.

### 5. Optional feature — WHERE

`WHERE <feature is included>, the <system> shall <response>.`

Use for product-line/build variability — the condition is about which
*configuration is running*, not about runtime state. This is the
pattern most often confused with WHILE; see the pitfall below.

> WHERE the deployment enables the S3 backend, the cache shall write
> objects using multipart upload for bodies over 100 MB.

### 6. Complex — combinations

`WHILE <state>, WHEN <trigger>, the <system> shall <response>.` (and other
orderings of the above, e.g. WHILE + WHEN + IF/THEN)

Use only when the response genuinely depends on more than one condition
holding simultaneously. Reaching for Complex to avoid writing two
requirements is itself a finding — see E1.

> WHILE the cache is serving from the degraded-storage fallback, WHEN a
> write completes successfully, the eviction manager shall re-admit the
> object into normal rotation.

## E1 — Exactly one pattern

A statement either matches one of the six forms cleanly, or it is two
requirements wearing one ID.

- Bad: "When the disk is full, the cache shall evict entries, and while
  under memory pressure it shall throttle new admissions." Two independent
  obligations stitched together with "and" do not become a Complex
  requirement — Complex is one response under multiple *simultaneous*
  conditions, not two unrelated event/state pairs.
- Fix: split into two requirements — a WHEN (disk full -> evict) and a
  WHILE (memory pressure -> throttle) — each with its own coverage.
- Free-form statements with no keyword at all should still be flagged, with
  the pattern they implicitly want named in the finding: "The cache removes
  entries once their TTL elapses" implies WHEN ("WHEN an entry's TTL
  elapses, the cache shall remove it") — the fix is the reworded statement,
  not just "add a keyword."

## E2 — WHEN vs WHILE vs IF/THEN

The three trigger keywords get swapped constantly; the test is always *what
kind of thing is the condition*, not which word sounds natural in prose.

| Ask | If yes |
|-----|--------|
| Does this happen at an instant (an edge)? | WHEN |
| Does this hold true over a span (a level)? | WHILE |
| Is this a fault/abnormal/undesired condition being handled? | IF...THEN |

- Bad: "When the system is in maintenance mode, ..." — "maintenance mode"
  is a state, not an event. Fix: "WHILE the system is in maintenance
  mode, ...".
- Bad: "While the request times out, the proxy shall return 504." — a
  timeout is the instant a deadline is crossed, not a duration. Fix:
  "WHEN the upstream request exceeds its deadline, the proxy shall return
  504."
- Borderline: "WHEN the checksum fails, the cache shall discard the
  object." Syntactically a valid WHEN (checksum failure is an event), but
  it is *also* an unwanted condition — in this project's convention that
  makes it an E2 finding: rewrite as "IF the on-disk object's checksum
  fails, THEN the cache shall discard the object and log an integrity
  fault," which also forces the fault-logging obligation into the open
  instead of leaving it implicit.

## E3 — Trigger and response on the same abstraction level

A requirement's trigger and its response must both live at the level the
requirement itself is written at. Mixing levels smuggles a design decision
into a stakeholder-level statement, or under-specifies a design-level one.

- Bad (HLR mixing in implementation detail): "WHEN a user requests a
  resource, the LRU eviction thread shall acquire the shard mutex before
  reading." A user-facing trigger paired with a thread/mutex-level
  response — the mutex detail belongs in a DSN that `\satisfy`s this HLR,
  not in the HLR itself.
- Fix at HLR level: "WHEN a user requests a cached resource, the cache
  shall serve it without observable inconsistency under concurrent
  access." — then a derived DSN specifies the locking mechanism.
- The reverse also counts: a DSN whose trigger is left at stakeholder
  abstraction ("when the user wants speed...") without naming the actual
  triggering signal or interface at its level is equally an E3 finding.

## E4 — Most specific correct responding element

"The system shall" is a default that should almost always be replaced by
the actual owning element once one is known — it under-specifies
allocation and makes verification ownership unclear.

- Bad: "WHEN storage utilization exceeds 90%, the system shall begin
  evicting entries." Which subsystem? If eviction is allocated to a named
  component, name it.
- Fix: "WHEN storage utilization exceeds 90%, the eviction manager shall
  begin evicting entries."
- Exception: genuinely cross-cutting requirements (a stakeholder-level HLR
  before any allocation has happened) may legitimately say "the system" —
  E4 applies once a more specific element is known or the level implies
  one exists (compare against S3/S4).

## Quick disambiguation table

| Torn between... | Ask |
|---|---|
| WHEN vs WHILE | Does it happen at an instant, or hold over a duration? |
| WHEN vs IF/THEN | Is this routine operation, or a fault/abnormal condition? |
| WHILE vs WHERE | Is this runtime state, or which build/configuration is running? |
| Simple vs Complex | Does the response need more than one condition *at once*, or are these two separate obligations? |
| Keep "the system" vs name a element | Has this requirement's content already been allocated to a specific component? |
