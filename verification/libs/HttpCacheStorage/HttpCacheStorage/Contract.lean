-- SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
-- SPDX-License-Identifier: Apache-2.0

/-!
# The storage contract

What the HTTP cache of the `pingora-cache` crate, version 0.9.0, expects of a
storage backend through its `Storage`, `HandleHit` and `HandleMiss` traits
(`src/storage.rs` of that crate), stated as a checker of traces: a trace is
the sequence of calls a driver made on one storage, one at a time, each with
the result the storage gave; `step` accepts a call's result when the
contract allows it in the state the earlier calls left, and says why not
otherwise.

What the contract allows, and what it forbids:

* A lookup may always miss, and any call may fail: a storage may lose an
  entry at any time, and the cache serves a miss or an error from the
  origin. A storage never serves what it does not hold.
* A hit is either of the entry stored for the key, with the metadata stored
  with it or set on it since, or of a write still in flight for the key
  (`support_streaming_partial_write`), and reports the entry's identity
  (`HandleHit::entry_id`) as its write did (`HandleMiss::entry_id`).
* A hit's body, read chunk by chunk with `read_body`, is the stored body, or
  the bytes the write has written so far; it ends (`None`) only at the end
  of the body, or of the range a `seek` asked for. A hit of a write in
  flight ends only once the write has written its last chunk (`eof`); it may
  fail otherwise, but never end early, since the cache would then serve a
  truncated body as whole.
* `finish` on a write makes its body, with the metadata it was opened with,
  the entry for its key. A purge or an expiry does not stop a write already
  in flight from landing afterwards: `Storage::expire`'s documentation says
  so of both.
* A purge removes the entry for its key, matching its identity when the
  target is exact (`PurgeTarget::Exact`); reports the identity of the entry
  it removed for an active target (`PurgeTarget::Active`); and once it
  reports the entry removed or not found, the entry is not served again.
  What it reports where nothing it names is stored is not checked: the
  cache then has nothing to remove from its eviction manager, and
  `MemCache` reports an entry removed for a key whose writes were all
  dropped.
* An expiry that reports `Expired` keeps the entry, and every later hit of it
  is stale; one that reports `Purged` is a purge; one that reports `NotFound`
  leaves nothing to serve.
* `update_meta` that reports success replaces the stored entry's metadata.

What it leaves out: how many bytes a chunk holds, the sizes `finish`
reports, the weights and accesses reported to the eviction manager, writes
that append to a stored entry (`MissFinishType::Appended`), and anything
about concurrency beyond the order of the calls a trace records.
-/

namespace HttpCacheStorage

/-- A byte string. -/
abbrev Bytes := List UInt8

/-- A cache key as storage sees it: `CacheKey::combined()`, which stands for
the primary key and its variance together. -/
abbrev Key := String

/-- The storage-defined identity of an entry (`CacheEntryId`), when the
storage gives one. -/
abbrev EntryId := Option Nat

/-- What a trace records of a `CacheMeta`: a label its driver gives each
metadata it builds, unique within the trace, and whether the metadata was
fresh when the call returned it or was given it. -/
structure Meta where
  label : String
  fresh : Bool
  deriving DecidableEq, Repr, Inhabited

/-- The entry a purge or an expiry targets (`PurgeTarget`). -/
inductive Target where
  /-- The active entry for the key. -/
  | active
  /-- The entry of exactly this identity. -/
  | exact (id : EntryId)
  deriving DecidableEq, Repr, Inhabited

/-- Why a purge is made (`PurgeType`). -/
inductive PurgeKind where
  | eviction
  | invalidation
  deriving DecidableEq, Repr, Inhabited

/-- What `Storage::lookup` or `Storage::lookup_streaming_write` returned. -/
inductive LookupResult where
  | miss
  /-- A hit, the handle the trace names it by, its metadata, and its
  `HandleHit::entry_id`. -/
  | hit (hit : Nat) (metadata : Meta) (id : EntryId)
  | error
  deriving DecidableEq, Repr, Inhabited

/-- What `HandleHit::read_body` returned. -/
inductive ReadResult where
  | chunk (data : Bytes)
  /-- `None`: no more body. -/
  | done
  | error
  deriving DecidableEq, Repr, Inhabited

/-- What `Storage::purge` or `Storage::expire` returned (`PurgeOutcome`). -/
inductive Outcome where
  | notFound
  | purged (id : EntryId)
  | expired
  | error
  deriving DecidableEq, Repr, Inhabited

/-- What `HandleMiss::finish` returned (`MissFinishType`). -/
inductive FinishResult where
  | created (size : Nat)
  | appended (size : Nat) (max : Option Nat)
  | error
  deriving DecidableEq, Repr, Inhabited

/-- One call of a trace, with its result. Handles are the trace's own
numbers: a hit's from the lookup that opened it, a write's from the
`get_miss_handler` call that opened it. -/
inductive Event where
  /-- `lookup`, or with the write's handle, `lookup_streaming_write` with
  that write's `streaming_write_tag`. -/
  | lookup (key : Key) (tag : Option Nat) (result : LookupResult)
  | read (hit : Nat) (result : ReadResult)
  /-- `HandleHit::seek(start, stop)`, and whether it succeeded. -/
  | seek (hit : Nat) (start : Nat) (stop : Option Nat) (ok : Bool)
  /-- `HandleHit::finish`, whatever it returned. -/
  | finishHit (hit : Nat)
  /-- `get_miss_handler`, and the write it opened with its
  `HandleMiss::entry_id`, or `none` for an error. -/
  | missHandler (key : Key) (metadata : Meta) (result : Option (Nat × EntryId))
  | write (writer : Nat) (data : Bytes) (eof : Bool) (ok : Bool)
  | finish (writer : Nat) (result : FinishResult)
  /-- The miss handler dropped without `finish`. -/
  | drop (writer : Nat)
  | purge (key : Key) (target : Target) (kind : PurgeKind) (outcome : Outcome)
  | expire (key : Key) (target : Target) (outcome : Outcome)
  /-- `update_meta`, and what it returned, or `none` for an error. -/
  | updateMeta (key : Key) (metadata : Meta) (result : Option Bool)
  deriving DecidableEq, Repr, Inhabited

/-- The entry stored for a key. -/
structure Entry where
  metadata : Meta
  /-- Expired since it was stored or its metadata last set. -/
  stale : Bool
  body : Bytes
  id : EntryId
  deriving DecidableEq, Repr, Inhabited

/-- Where a write stands. -/
inductive Status where
  | inFlight
  | finished
  /-- Dropped without `finish`, or failed. -/
  | abandoned
  deriving DecidableEq, Repr, Inhabited

/-- A write, from `get_miss_handler` on. -/
structure Write where
  key : Key
  metadata : Meta
  /-- The bytes written so far. -/
  body : Bytes
  /-- Whether its last chunk is written. -/
  eof : Bool
  id : EntryId
  status : Status
  deriving DecidableEq, Repr, Inhabited

/-- What a hit reads from. -/
inductive Source where
  /-- The stored body, as it was when the lookup returned. -/
  | stored (body : Bytes)
  /-- A write, in flight when the lookup returned. -/
  | write (writer : Nat)
  deriving DecidableEq, Repr, Inhabited

/-- An open hit: what it reads, the position its next chunk starts at, and
where the range a `seek` asked for stops. -/
structure Hit where
  source : Source
  pos : Nat
  stop : Option Nat
  deriving DecidableEq, Repr, Inhabited

/-- What the calls so far have left. -/
structure State where
  entries : List (Key × Entry) := []
  writes : List (Nat × Write) := []
  hits : List (Nat × Hit) := []
  deriving Repr, Inhabited

/-- The value for `a` in an association list. -/
def find? [BEq α] (l : List (α × β)) (a : α) : Option β :=
  (l.find? (fun p => p.1 == a)).map (·.2)

/-- An association list with `a` mapped to `b`, and no other value for `a`. -/
def put [BEq α] (l : List (α × β)) (a : α) (b : β) : List (α × β) :=
  (a, b) :: l.filter (fun p => !(p.1 == a))

/-- An association list without a value for `a`. -/
def erase [BEq α] (l : List (α × β)) (a : α) : List (α × β) :=
  l.filter (fun p => !(p.1 == a))

/-- A step the contract allows, or why it does not. -/
abbrev Outcome? := Except String State

/-- Whether an entry is the one a target names. -/
def Target.names : Target → Entry → Bool
  | .active, _ => true
  | .exact id, e => e.id == id

/-- The body a hit reads, as far as it is known now, and whether it is
whole: a stored body is; a write's is once its last chunk is written. A
range a `seek` asked for cuts both: the body stops at the range's end, and
is whole once it reaches it. -/
def Hit.content (s : State) (h : Hit) : Bytes × Bool :=
  let (body, whole) : Bytes × Bool := match h.source with
    | .stored body => (body, true)
    | .write w => match find? s.writes w with
      | some wr => (wr.body, wr.eof)
      | none => ([], false)
  match h.stop with
  | some stop => (body.take stop, whole || decide (stop ≤ body.length))
  | none => (body, whole)

/-- The hit a lookup reports, if the contract allows it: of the stored entry,
with its metadata and freshness and identity; or of a write in flight for
the key, with the write's. A lookup by a write's tag reports that write, in
flight or just stored, or nothing. -/
def lookupHit (s : State) (key : Key) (tag : Option Nat) (m : Meta)
    (id : EntryId) : Option Hit :=
  let stored : Option Hit := match find? s.entries key with
    | some e =>
      if e.metadata.label == m.label && m.fresh == (e.metadata.fresh && !e.stale) &&
          e.id == id then
        some ⟨.stored e.body, 0, none⟩
      else none
    | none => none
  let inFlight (ok : Nat → Write → Bool) : Option Hit :=
    (s.writes.find? (fun p => p.2.key == key && p.2.status == Status.inFlight &&
        p.2.metadata == m && p.2.id == id && ok p.1 p.2)).map
      (fun p => ⟨.write p.1, 0, none⟩)
  match tag with
  | none => stored <|> inFlight (fun _ _ => true)
  | some w =>
    let ofWrite := match find? s.writes w with
      | some wr => wr.metadata.label == m.label
      | none => false
    inFlight (fun n _ => n == w) <|> (if ofWrite then stored else none)

/-- The step a purge's outcome makes: an active target reports the identity
of the stored entry it removed, and whatever stored entry its target names
and the storage reports removed or not found, it serves no more. -/
def purgeStep (s : State) (key : Key) (target : Target) (outcome : Outcome)
    (call : String) : Outcome? :=
  let removed : State := { s with entries := erase s.entries key }
  match outcome with
  | .error => .ok s
  | .notFound =>
    match find? s.entries key with
    | some e => .ok (if target.names e then removed else s)
    | none => .ok s
  | .purged id =>
    match find? s.entries key with
    | some e =>
      if target.names e then
        if target == .active && e.id.isSome && id != e.id then
          .error s!"{call} reports entry {repr id} removed, but removed the \
            stored entry {repr e.id}"
        else .ok removed
      else .ok s
    | none => .ok s
  | .expired => .error s!"{call} reports Expired, which only expire may"

/-- The step one call makes, if the contract allows its result. -/
def step (s : State) : Event → Outcome?
  | .lookup _ _ .miss => .ok s
  | .lookup _ _ .error => .ok s
  | .lookup key tag (.hit h m id) =>
    match lookupHit s key tag m id with
    | some hit => .ok { s with hits := put s.hits h hit }
    | none => .error s!"lookup of {key} returned a hit of metadata {m.label} \
        (fresh: {m.fresh}, entry {repr id}), which is neither the stored \
        entry nor a write in flight for the key"
  | .read h r =>
    match find? s.hits h with
    | none => match r with
      | .error => .ok s
      | _ => .error s!"read of hit {h}, which is not open"
    | some hit =>
      let (body, whole) := hit.content s
      match r with
      | .error => .ok s
      | .chunk data =>
        if (body.drop hit.pos).take data.length == data then
          .ok { s with hits := put s.hits h { hit with pos := hit.pos + data.length } }
        else .error s!"hit {h} read {data.length} bytes at {hit.pos} that are \
          not the body's"
      | .done =>
        if whole && hit.pos == body.length then .ok s
        else .error s!"hit {h} ended at byte {hit.pos}, before the end of its \
          body (whole: {whole}, {body.length} bytes known)"
  | .seek h start stop ok =>
    match find? s.hits h, ok with
    | _, false => .ok s
    | none, true => .error s!"seek on hit {h}, which is not open"
    | some hit, true =>
      let (body, whole) := Hit.content s { hit with stop := none }
      if whole && body.length ≤ start then
        .error s!"seek of hit {h} to {start} succeeded, past its \
          {body.length}-byte body"
      else .ok { s with hits := put s.hits h { hit with pos := start, stop := stop } }
  | .finishHit h => .ok { s with hits := erase s.hits h }
  | .missHandler _ _ none => .ok s
  | .missHandler key m (some (w, id)) =>
    .ok { s with writes := put s.writes w ⟨key, m, [], false, id, .inFlight⟩ }
  | .write w data eof ok =>
    match find? s.writes w, ok with
    | _, false => .ok s
    | none, true => .error s!"write to writer {w}, which was never opened"
    | some wr, true =>
      if wr.status == .inFlight && !wr.eof then
        let wr' := { wr with body := wr.body ++ data, eof := eof }
        .ok { s with writes := put s.writes w wr' }
      else .error s!"write to writer {w} succeeded after its end"
  | .finish w r =>
    match find? s.writes w with
    | none => match r with
      | .error => .ok s
      | _ => .error s!"finish of writer {w}, which was never opened"
    | some wr =>
      match r with
      | .error => .ok { s with writes := put s.writes w { wr with status := .abandoned } }
      | .appended .. => .error s!"finish of writer {w} reports Appended, \
          which this contract does not model"
      | .created _ =>
        if wr.status == .inFlight then
          let e : Entry := ⟨wr.metadata, false, wr.body, wr.id⟩
          let wr' := { wr with status := .finished }
          .ok { s with entries := put s.entries wr.key e, writes := put s.writes w wr' }
        else .error s!"finish of writer {w} succeeded after it ended"
  | .drop w =>
    match find? s.writes w with
    | some wr =>
      if wr.status == .inFlight then
        .ok { s with writes := put s.writes w { wr with status := .abandoned } }
      else .ok s
    | none => .ok s
  | .purge key target _ outcome => purgeStep s key target outcome "purge"
  | .expire key target .expired =>
    match find? s.entries key with
    | some e =>
      if target.names e then
        .ok { s with entries := put s.entries key { e with stale := true } }
      else .error s!"expire of {key} reports Expired, but the stored entry \
        {repr e.id} is not its target {repr target}"
    | none => .error s!"expire of {key} reports Expired where none is stored"
  | .expire key target outcome => purgeStep s key target outcome "expire"
  | .updateMeta _ _ none => .ok s
  | .updateMeta _ _ (some false) => .ok s
  | .updateMeta key m (some true) =>
    match find? s.entries key with
    | some e => .ok { s with entries := put s.entries key { e with metadata := m, stale := false } }
    | none => .error s!"update_meta of {key} reports success where no entry \
        is stored"

/-- Check a whole trace from the empty state: the state it leaves, or the
index of the first call whose result the contract does not allow, and why. -/
def check (events : List Event) : Except (Nat × String) State :=
  go 0 {} events
where
  go (i : Nat) (s : State) : List Event → Except (Nat × String) State
    | [] => .ok s
    | e :: rest => match step s e with
      | .ok s' => go (i + 1) s' rest
      | .error why => .error (i, why)

end HttpCacheStorage
