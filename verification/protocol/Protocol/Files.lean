-- SPDX-FileCopyrightText: 2026 The durable-cache authors
-- SPDX-License-Identifier: Apache-2.0

-- IMP-TC.STORAGE.PROTOCOL-01: satisfy TC-STORAGE.PROTOCOL-01

module

public import Veil
public import Protocol.Axioms

/-!
# The protocol of the backend's files, across crashes

A model, in Veil, of what the backend does to its files with stores
synchronised (ADR-PERSISTENCE.DURABILITY-02), and of what a host crash may
leave of it, with the invariants TC-STORAGE.PROTOCOL-01 checks:

* A store writes its own temporary, synchronises its data, and, under the
  key's lock, renames it over the key's canonical name, unless an
  invalidation has ended it (FDS-STORAGE.PUBLISH-03). The directory is not
  synchronised after.
* An invalidation, under the key's lock, ends the key's stores in flight and
  unlinks its record; then synchronises the directory, then answers
  (FDS-STORAGE.PURGE-01). An eviction unlinks, unsynchronised.
* A metadata update rewrites the record in place, and a withdrawal its
  header, unsynchronised (FDS-STORAGE.METAUPDATE-01).
* The file system writes a directory's pending changes, and a file's data,
  whenever it chooses.
* A host crash keeps, for each name, one of the files it may refer to; loses
  the data of every file not synchronised; and leaves each record with a
  change not yet written back as it was, as changed, or torn.

What it assumes: the read-time checks of a record (SDS-LAYOUT.OBJECTFILE-03)
reject every record a crash tore, so a torn record is a miss; each change
the running system makes is atomic, its concurrency being the loom checks';
and a crash may keep any of a name's pending renames, a superset of what a
journalled file system keeps.

`veil.smt.trust` is off: Lean reconstructs and checks each of the solver's
proofs, so the invariants rest on Lean's kernel, not on cvc5.
-/

set_option veil.smt.trust false

veil module Files

type key
type file
type version

-- The key's canonical name refers to the file, as the running system sees it.
relation named : key → file → Bool
-- After a crash, the name may refer to the file: its rename may have reached
-- stable storage.
relation survives : key → file → Bool
-- After a crash, the name may refer to nothing.
relation mayBeEmpty : key → Bool

-- The file exists: a store's temporary, or a record.
relation used : file → Bool
-- The file was opened for a store of the key.
relation owner : file → key → Bool
-- The file is the temporary of a store of the key still in flight.
relation inFlight : file → key → Bool
-- An invalidation ended the store writing the file.
relation ended : file → Bool
-- The file's data reached stable storage once.
relation synced : file → Bool
-- A crash lost the file's data, never synchronised.
relation lost : file → Bool
-- The version the file holds, as the running system sees it.
relation holds : file → version → Bool
-- The version the file holds on stable storage.
relation durable : file → version → Bool
-- The record has a change, an update or a withdrawal, not yet written back.
relation pending : file → Bool
-- The record is withdrawn, as the running system sees it, and on stable
-- storage.
relation withdrawn : file → Bool
relation withdrawnDurable : file → Bool
-- A crash tore the record: its read-time checks fail.
relation torn : file → Bool
-- Scratch for a crash's choices: which pending records keep their change,
-- and which tear.
relation keepChange : file → Bool
relation tear : file → Bool

-- Ghost: a store or an update of the key wrote the version.
relation stored : key → version → Bool
-- Ghost: an invalidation of the key unlinked the file's record, or ended its
-- store, and has not answered yet.
relation unlinked : key → file → Bool
-- Ghost: an invalidation of the key that answered removed the file's record,
-- or ended its store.
relation purged : key → file → Bool

#gen_state

after_init {
  named K F := false
  survives K F := false
  mayBeEmpty K := true
  used F := false
  owner F K := false
  inFlight F K := false
  ended F := false
  synced F := false
  lost F := false
  holds F V := false
  durable F V := false
  pending F := false
  withdrawn F := false
  withdrawnDurable F := false
  torn F := false
  keepChange F := false
  tear F := false
  stored K V := false
  unlinked K F := false
  purged K F := false
}

-- `get_miss_handler`: a new temporary, into which the store writes its
-- version.
action open_store (k : key) (f : file) (v : version) {
  require ¬ used f
  used f := true
  owner f k := true
  inFlight f k := true
  holds f v := true
  stored k v := true
}

-- `fdatasync` of a temporary, or the kernel writing its data back.
action sync_data (k : key) (f : file) {
  require inFlight f k
  synced f := true
  durable f V := holds f V
}

-- `finish`: under the key's lock, a store no invalidation has ended renames
-- its synchronised temporary over the canonical name.
action publish (k : key) (f : file) {
  require inFlight f k
  require ¬ ended f
  require synced f
  named k F := F = f
  survives k f := true
  inFlight f k := false
}

-- A store that ends unpublished: its temporary is removed.
action end_store (k : key) (f : file) {
  require inFlight f k
  inFlight f k := false
}

-- An invalidation, under the key's lock: end the key's stores in flight,
-- and unlink its record.
action purge_unlink (k : key) {
  unlinked k F := unlinked k F || named k F || inFlight F k
  ended F := ended F || inFlight F k
  named k F := false
  mayBeEmpty k := true
}

-- The invalidation, after the lock: synchronise the directory, then answer.
action purge_answer (k : key) {
  survives k F := named k F
  mayBeEmpty k := *
  require mayBeEmpty k ↔ ∀ f, ¬ named k f
  purged k F := purged k F || unlinked k F
  unlinked k F := false
}

-- An eviction: unlink the record, unsynchronised.
action evict (k : key) {
  named k F := false
  mayBeEmpty k := true
}

-- `update_meta`, in place, under the key's lock: the record now holds the
-- new version.
action update_meta (k : key) (f : file) (v : version) {
  require named k f
  require ¬ lost f ∧ ¬ torn f ∧ ¬ withdrawn f
  holds f V := V = v
  pending f := true
  stored k v := true
}

-- A withdrawal: the record's header rewritten, marked withdrawn.
action withdraw (k : key) (f : file) {
  require named k f
  withdrawn f := true
  pending f := true
}

-- The file system writes the directory's pending changes.
action persist (k : key) {
  survives k F := named k F
  mayBeEmpty k := *
  require mayBeEmpty k ↔ ∀ f, ¬ named k f
}

-- The file system writes a record's pending change back.
action write_back (f : file) {
  require pending f
  durable f V := holds f V
  withdrawnDurable f := withdrawn f
  pending f := false
}

-- A host crash: each name keeps one of the files it may refer to, or none
-- where it may be empty; data never synchronised is lost; each record with
-- a change not written back keeps it, loses it, or tears; no store is in
-- flight any more, and no invalidation answers.
action crash {
  named K F := *
  require ∀ k f, named k f → survives k f
  require ∀ k f g, named k f ∧ named k g → f = g
  require ∀ k, (∀ f, ¬ named k f) → mayBeEmpty k
  keepChange F := *
  tear F := *
  require ∀ f, tear f → pending f
  holds F V := (pending F && keepChange F && holds F V) || (!(pending F && keepChange F) && durable F V)
  withdrawn F := (pending F && keepChange F && withdrawn F) || (!(pending F && keepChange F) && withdrawnDurable F)
  torn F := torn F || tear F
  keepChange F := false
  tear F := false
  lost F := lost F || !synced F
  durable F V := holds F V
  withdrawnDurable F := withdrawn F
  pending F := false
  inFlight F K := false
  unlinked K F := false
  survives K F := named K F
  mayBeEmpty K := *
  require ∀ k, mayBeEmpty k ↔ ∀ f, ¬ named k f
}

-- What a lookup of the key may serve: the version of the record its name
-- refers to, unless a crash lost or tore it, or it is withdrawn.
ghost relation servable (k : key) (v : version) :=
  ∃ f, named k f ∧ ¬ lost f ∧ ¬ torn f ∧ ¬ withdrawn f ∧ holds f v

-- 1. Synchronised stores: no name refers, or may refer after a crash, to a
-- file whose data never reached stable storage (ADR-PERSISTENCE.DURABILITY-02,
-- FDS-STORAGE.PUBLISH-03, ADR-STORAGE.ATOMICPUBLISH-01).
safety [tc_storage_protocol_01_synchronised_names] survives K F → synced F
-- 2. No mixtures: a lookup serves only a version a store or an update of
-- the key wrote, whole, before or after any update or withdrawal, across any
-- crash (HLR-PERSISTENCE.CRASHSAFE-02, FDS-STORAGE.METAUPDATE-01).
safety [tc_storage_protocol_01_no_mixtures] servable K V → stored K V
-- 3. Purges hold: no name refers, or may refer after a crash, to a record
-- an invalidation that answered removed (FRE-COMPLIANCE.PURGEDURABLE-01).
safety [tc_storage_protocol_01_purges_hold] purged K F → ¬ survives K F ∧ ¬ named K F
-- 4. Purges end stores in flight: no store an invalidation ended is ever
-- published (FRE-COMPLIANCE.PURGE-01, FDS-STORAGE.PURGE-01).
safety [tc_storage_protocol_01_purges_end_stores] ended F → ¬ named K F ∧ ¬ survives K F

-- The strengthening that makes the four inductive.
invariant [one_name] named K F ∧ named K G → F = G
invariant [named_survives] named K F → survives K F
invariant [used_files] owner F K → used F
invariant [in_flight_owned] inFlight F K → owner F K
invariant [named_owned] survives K F → owner F K
invariant [one_owner] owner F K ∧ owner F L → K = L
invariant [one_version] holds F V1 ∧ holds F V2 → V1 = V2
invariant [one_durable_version] durable F V1 ∧ durable F V2 → V1 = V2
invariant [owned_version] owner F K ∧ holds F V → stored K V
invariant [owned_durable_version] owner F K ∧ durable F V → stored K V
invariant [in_flight_unpublished] inFlight F K → ¬ survives L F
invariant [unlinked_in_flight_ended] unlinked K F ∧ inFlight F K → ended F
invariant [purged_in_flight_ended] purged K F ∧ inFlight F K → ended F
invariant [unlinked_gone] unlinked K F → ¬ named K F
invariant [holds_used] holds F V → used F
invariant [durable_used] durable F V → used F
invariant [unlinked_used] unlinked K F → used F
invariant [purged_used] purged K F → used F
invariant [pending_synced] pending F → synced F
invariant [no_choices] ¬ keepChange F ∧ ¬ tear F

#gen_spec

-- The model is not vacuous: it lets a store be served, an eviction a crash
-- undoes bring the record back, a crash tear an update, and a crash lose an
-- unsynchronised temporary.
sat trace [a_store_is_served] {
  open_store
  sync_data
  publish
  assert (∃ k v, servable k v)
}
sat trace [an_unsynchronised_eviction_comes_back] {
  open_store
  sync_data
  publish
  persist
  evict
  crash
  assert (∃ k v, servable k v)
}
sat trace [a_crash_tears_an_update] {
  open_store
  sync_data
  publish
  update_meta
  crash
  assert (∃ f, torn f)
}
sat trace [a_crash_loses_a_temporary] {
  open_store
  crash
  assert (∃ f, lost f)
}

-- Every reachable state of an instance with one key, two files and two
-- versions.
#model_check { key := Fin 1, file := Fin 2, version := Fin 2 } { }

-- The invariants, for any number of keys, files and versions.
#check_invariants
#gen_theorems

#assert_axioms_in Files

end Files
