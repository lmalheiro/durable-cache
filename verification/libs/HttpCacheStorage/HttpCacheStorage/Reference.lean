-- SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
-- SPDX-License-Identifier: Apache-2.0

import HttpCacheStorage.Contract

/-!
# A reference implementation

A storage that runs: given the calls of a trace without their results, it
answers each, as an in-memory storage that keeps one entry for each key and
every write in flight would. A lookup prefers a write in flight for the key
to the stored entry, as `MemCache` does; a read gives every byte the hit may
deliver; a purge or an expiry removes or marks only the stored entry its
target names.

`conforms` proves that the contract allows every trace it produces, for any
calls in any order: the contract is not so strict that no storage meets it.
-/

namespace HttpCacheStorage.Reference

/-- A call of a trace, without its result. -/
inductive Call where
  | lookup (key : Key) (tag : Option Nat)
  | read (hit : Nat)
  | seek (hit : Nat) (start : Nat) (stop : Option Nat)
  | finishHit (hit : Nat)
  | missHandler (key : Key) (metadata : Meta)
  | write (writer : Nat) (data : Bytes) (eof : Bool)
  | finish (writer : Nat)
  | drop (writer : Nat)
  | purge (key : Key) (target : Target) (kind : PurgeKind)
  | expire (key : Key) (target : Target)
  | updateMeta (key : Key) (metadata : Meta)
  deriving DecidableEq, Repr, Inhabited

/-- A handle no open one has: one past the greatest. -/
def fresh (l : List (Nat × β)) : Nat :=
  l.foldl (fun n p => max n (p.1 + 1)) 0

/-- The result the reference storage gives a call, in the state the earlier
calls left. -/
def respond (s : State) : Call → Event
  | .lookup key (some w) =>
    match find? s.writes w with
    | some wr =>
      if wr.key == key && wr.status == Status.inFlight then
        .lookup key (some w) (.hit (fresh s.hits) wr.metadata wr.id)
      else .lookup key (some w) .miss
    | none => .lookup key (some w) .miss
  | .lookup key none =>
    match s.writes.find? (fun p => p.2.key == key && p.2.status == Status.inFlight) with
    | some (_, wr) => .lookup key none (.hit (fresh s.hits) wr.metadata wr.id)
    | none =>
      match find? s.entries key with
      | some e =>
        .lookup key none (.hit (fresh s.hits)
          ⟨e.metadata.label, e.metadata.fresh && !e.stale⟩ e.id)
      | none => .lookup key none .miss
  | .read h =>
    match find? s.hits h with
    | none => .read h .error
    | some hit =>
      let (body, whole) := hit.content s
      if hit.pos < body.length then .read h (.chunk (body.drop hit.pos))
      else if whole && hit.pos == body.length then .read h .done
      else .read h .error
  | .seek h start stop =>
    match find? s.hits h with
    | none => .seek h start stop false
    | some hit =>
      let (body, whole) := Hit.content s { hit with stop := none }
      .seek h start stop !(whole && decide (body.length ≤ start))
  | .finishHit h => .finishHit h
  | .missHandler key m => .missHandler key m (some (fresh s.writes, none))
  | .write w data eof =>
    match find? s.writes w with
    | some wr => .write w data eof (wr.status == Status.inFlight && !wr.eof)
    | none => .write w data eof false
  | .finish w =>
    match find? s.writes w with
    | some wr =>
      if wr.status == Status.inFlight then .finish w (.created wr.body.length)
      else .finish w .error
    | none => .finish w .error
  | .drop w => .drop w
  | .purge key target kind =>
    match find? s.entries key with
    | some e =>
      if target.names e then .purge key target kind (.purged e.id)
      else .purge key target kind .notFound
    | none => .purge key target kind .notFound
  | .expire key target =>
    match find? s.entries key with
    | some e =>
      if target.names e then .expire key target .expired
      else .expire key target .notFound
    | none => .expire key target .notFound
  | .updateMeta key m =>
    match find? s.entries key with
    | some _ => .updateMeta key m (some true)
    | none => .updateMeta key m (some false)

/-- The state after the reference storage answers `c`. -/
def next (s : State) (c : Call) : State :=
  match step s (respond s c) with
  | .ok s' => s'
  | .error _ => s

/-- The trace the reference storage makes of `calls`, from `s`. -/
def run (s : State) : List Call → List Event
  | [] => []
  | c :: cs => respond s c :: run (next s c) cs

/-! ## The proof -/

theorem find?_mem [BEq α] [LawfulBEq α] {l : List (α × β)} {a : α} {b : β}
    (h : find? l a = some b) : (a, b) ∈ l := by
  unfold find? at h
  cases hf : l.find? (fun p => p.1 == a) with
  | none => simp [hf] at h
  | some p =>
    simp [hf] at h
    have hm := List.mem_of_find?_eq_some hf
    have hp := List.find?_some hf
    simp at hp
    cases p
    simp_all

/-- A write in flight for the key, with this metadata and identity, makes a
lookup by the write's tag hit. -/
theorem lookupHit_tag {s : State} {key : Key} {w : Nat} {wr : Write}
    (hw : find? s.writes w = some wr) (hk : wr.key = key)
    (hs : wr.status = Status.inFlight) :
    (lookupHit s key (some w) wr.metadata wr.id).isSome := by
  have hm := find?_mem hw
  unfold lookupHit
  simp only [hw]
  have : (s.writes.find? (fun p => p.2.key == key && p.2.status == Status.inFlight &&
      p.2.metadata == wr.metadata && p.2.id == wr.id && p.1 == w)).isSome := by
    rw [List.find?_isSome]
    exact ⟨(w, wr), hm, by simp [hk, hs]⟩
  revert this
  cases s.writes.find? (fun p => p.2.key == key && p.2.status == Status.inFlight &&
      p.2.metadata == wr.metadata && p.2.id == wr.id && p.1 == w) <;> simp

/-- The first write in flight for the key makes a lookup without a tag hit. -/
theorem lookupHit_inFlight {s : State} {key : Key} {n : Nat} {wr : Write}
    (hf : s.writes.find? (fun p => p.2.key == key && p.2.status == Status.inFlight) =
      some (n, wr)) :
    (lookupHit s key none wr.metadata wr.id).isSome := by
  have hm := List.mem_of_find?_eq_some hf
  have hp := List.find?_some hf
  simp at hp
  unfold lookupHit
  simp
  exact Or.inr ⟨n, wr, hm, ⟨hp, rfl⟩, rfl⟩

/-- The stored entry makes a lookup without a tag hit, with its metadata as
it stands. -/
theorem lookupHit_stored {s : State} {key : Key} {e : Entry}
    (he : find? s.entries key = some e) :
    (lookupHit s key none ⟨e.metadata.label, e.metadata.fresh && !e.stale⟩ e.id).isSome := by
  unfold lookupHit
  simp [he]

theorem take_drop_self (l : List α) (n : Nat) :
    (l.drop n).take (l.length - n) = l.drop n := by
  rw [← List.length_drop]
  exact List.take_length

/-- The contract allows every result the reference storage gives. -/
theorem step_respond (s : State) (c : Call) : ∃ s', step s (respond s c) = .ok s' := by
  cases c with
  | lookup key tag =>
    cases tag with
    | some w =>
      simp only [respond]
      cases hw : find? s.writes w with
      | none => exact ⟨s, rfl⟩
      | some wr =>
        simp only
        split
        · next hc =>
          simp at hc
          have := lookupHit_tag hw hc.1 hc.2
          simp only [step]
          revert this
          cases lookupHit s key (some w) wr.metadata wr.id <;> simp
        · exact ⟨s, rfl⟩
    | none =>
      simp only [respond]
      split
      · next n wr hf =>
        have := lookupHit_inFlight hf
        simp only [step]
        revert this
        cases lookupHit s key none wr.metadata wr.id <;> simp
      · split
        · next e he =>
          have := lookupHit_stored he
          simp only [step]
          revert this
          cases lookupHit s key none ⟨e.metadata.label, e.metadata.fresh && !e.stale⟩ e.id <;>
            simp
        · exact ⟨s, rfl⟩
  | read h =>
    simp only [respond]
    cases hh : find? s.hits h with
    | none => exact ⟨s, by simp [step, hh]⟩
    | some hit =>
      simp only
      split
      · simp [step, hh, take_drop_self]
      · split
        · next hd => exact ⟨s, by simp_all [step]⟩
        · exact ⟨s, by simp [step, hh]⟩
  | seek h start stop =>
    simp only [respond]
    cases hh : find? s.hits h with
    | none => exact ⟨s, by simp [step]⟩
    | some hit =>
      simp only
      cases hw : (Hit.content s { hit with stop := none }).2 &&
          decide ((Hit.content s { hit with stop := none }).1.length ≤ start) with
      | true => exact ⟨s, by simp_all [step]⟩
      | false =>
        simp only [step, hh, Bool.not_false]
        rw [hw]
        simp
  | finishHit h => exact ⟨_, rfl⟩
  | missHandler key m => exact ⟨_, rfl⟩
  | write w data eof =>
    simp only [respond]
    cases hw : find? s.writes w with
    | none => exact ⟨s, by simp [step]⟩
    | some wr =>
      cases hc : (wr.status == Status.inFlight && !wr.eof) with
      | true => simp_all [step]
      | false => exact ⟨s, by simp [step, hw, hc]⟩
  | finish w =>
    simp only [respond]
    cases hw : find? s.writes w with
    | none => exact ⟨s, by simp [step, hw]⟩
    | some wr =>
      simp only
      split
      · next hc => simp_all [step]
      · simp [step, hw]
  | drop w =>
    simp only [respond, step]
    split
    · split <;> exact ⟨_, rfl⟩
    · exact ⟨_, rfl⟩
  | purge key target kind =>
    simp only [respond]
    cases he : find? s.entries key with
    | none => exact ⟨s, by simp [step, purgeStep, he]⟩
    | some e =>
      simp only
      split
      · next hn =>
        refine ⟨{ s with entries := erase s.entries key }, ?_⟩
        simp [step, purgeStep, he, hn]
      · next hn => exact ⟨s, by simp [step, purgeStep, he, hn]⟩
  | expire key target =>
    simp only [respond]
    cases he : find? s.entries key with
    | none => exact ⟨s, by simp [step, purgeStep, he]⟩
    | some e =>
      simp only
      split
      · next hn => simp [step, he, hn]
      · next hn => exact ⟨s, by simp [step, purgeStep, he, hn]⟩
  | updateMeta key m =>
    simp only [respond]
    cases he : find? s.entries key with
    | none => exact ⟨s, by simp [step]⟩
    | some e => simp [step, he]

theorem go_run (i : Nat) (s : State) (calls : List Call) :
    ∃ s', check.go i s (run s calls) = .ok s' := by
  induction calls generalizing i s with
  | nil => exact ⟨s, rfl⟩
  | cons c cs ih =>
    obtain ⟨s', hs⟩ := step_respond s c
    have hn : next s c = s' := by simp [next, hs]
    simp only [run, check.go, hs, hn]
    exact ih (i + 1) s'

/-- The contract allows every trace the reference storage makes. -/
theorem conforms (calls : List Call) : ∃ s, check (run {} calls) = .ok s :=
  go_run 0 {} calls

end HttpCacheStorage.Reference
