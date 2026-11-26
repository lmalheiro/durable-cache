-- SPDX-FileCopyrightText: 2026 The durable-cache authors
-- SPDX-License-Identifier: Apache-2.0

import Lean

/-!
`#assert_axioms thm`: list the axioms the theorem `thm` rests on, and fail
the build on any outside the reviewed list (Appendix D of the specification,
"How the Checks Are Kept Honest"). A proof with a gap rests on `sorryAx`,
which the list does not hold.
-/

open Lean Elab Command

/-- The reviewed axioms: Lean's own three. -/
def reviewedAxioms : List Name := [``propext, ``Classical.choice, ``Quot.sound]

elab "#assert_axioms " id:ident : command => do
  let name ← liftCoreM <| realizeGlobalConstNoOverloadWithInfo id
  let axioms ← liftCoreM <| collectAxioms name
  let unreviewed := axioms.filter (!reviewedAxioms.contains ·)
  unless unreviewed.isEmpty do
    throwError "{name} rests on axioms outside the reviewed list: {unreviewed.toList}"
  logInfo m!"{name} rests on {axioms.toList}"
