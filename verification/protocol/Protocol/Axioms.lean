-- SPDX-FileCopyrightText: 2026 The durable-cache authors
-- SPDX-License-Identifier: Apache-2.0

module

public meta import Lean

/-!
The axioms the Lean checks rest on (Appendix D of the specification, "How the
Checks Are Kept Honest"):

* `#assert_axioms thm` lists the axioms the theorem `thm` rests on;
* `#assert_axioms_in ns` lists those that the theorems of the namespace
  `ns` declared in the file rest on, together: a Veil model's verification
  conditions, once `#gen_theorems` has made them theorems.

Each fails the build on an axiom outside the reviewed list. A proof with a
gap rests on `sorryAx`, and one taken on an SMT solver's word on an axiom of
its own, neither of which the list holds. `cargo xtask verify` reads the
list each prints, and fails on an axiom the record does not name.
-/

public meta section

open Lean Elab Command

/-- The reviewed axioms: Lean's own three. -/
def reviewedAxioms : List Name := [``propext, ``Classical.choice, ``Quot.sound]

/-- Fail on an axiom of `axioms` outside the reviewed list, naming `what`. -/
def assertReviewed (what : MessageData) (axioms : Array Name) : CommandElabM Unit := do
  let unreviewed := axioms.filter (!reviewedAxioms.contains ·)
  unless unreviewed.isEmpty do
    throwError m!"{what} rests on axioms outside the reviewed list: {unreviewed.toList}"
  logInfo m!"{what} rests on {axioms.toList}"

elab "#assert_axioms " id:ident : command => do
  let name ← liftCoreM <| realizeGlobalConstNoOverloadWithInfo id
  assertReviewed m!"{name}" (← liftCoreM <| collectAxioms name)

elab "#assert_axioms_in " id:ident : command => do
  let ns := id.getId
  let mut theorems : Array Name := #[]
  for (name, info) in (← getEnv).constants.map₂.toList do
    if ns.isPrefixOf name && !name.isInternal then
      if let .thmInfo _ := info then
        theorems := theorems.push name
  if theorems.isEmpty then
    throwError m!"{ns} holds no theorem in this file"
  let mut axioms : Array Name := #[]
  for name in theorems do
    for a in ← liftCoreM <| collectAxioms name do
      unless axioms.contains a do
        axioms := axioms.push a
  assertReviewed m!"{ns} ({theorems.size} theorems)" axioms

end
