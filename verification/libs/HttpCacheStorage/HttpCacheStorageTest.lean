-- SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
-- SPDX-License-Identifier: Apache-2.0

import HttpCacheStorage

/-!
The checker's tests, run by `lake test`: short traces, each of a storage that
behaves as the contract allows, or that makes one mistake the contract
forbids, which the checker must reject. Each `#guard` fails the build when
its trace is not judged as it says.
-/

namespace HttpCacheStorage.Test

open Event

def m (label : String) (fresh := true) : Meta := ⟨label, fresh⟩

def allowed (events : List Event) : Bool :=
  match check events with
  | .ok _ => true
  | .error _ => false

/-- Writer 1 stores `[1, 2, 3]` for key `k` under metadata `a`. -/
def store : List Event :=
  [missHandler "k" (m "a") (some (1, none)),
   write 1 [1, 2] false true,
   write 1 [3] true true,
   finish 1 (.created 3)]

/-- A hit of the stored entry, read whole in two chunks. -/
def hitRead : List Event :=
  [lookup "k" none (.hit 1 (m "a") none),
   read 1 (.chunk [1, 2]), read 1 (.chunk [3]), read 1 .done]

#guard allowed (store ++ hitRead)
-- A miss, an error, at any time.
#guard allowed (store ++ [lookup "k" none .miss, lookup "k" none .error] ++ hitRead)
-- Bytes that are not the body's.
#guard !allowed (store ++ [lookup "k" none (.hit 1 (m "a") none), read 1 (.chunk [1, 9])])
-- An end before the end of the body.
#guard !allowed (store ++ [lookup "k" none (.hit 1 (m "a") none), read 1 (.chunk [1, 2]),
  read 1 .done])
-- A hit of metadata never stored.
#guard !allowed (store ++ [lookup "k" none (.hit 1 (m "b") none)])
-- A hit of another key's entry.
#guard !allowed (store ++ [lookup "j" none (.hit 1 (m "a") none)])

-- Purged, then served.
#guard !allowed (store ++ [purge "k" .active .invalidation (.purged none)] ++ hitRead)
#guard !allowed (store ++ [purge "k" .active .eviction .notFound] ++ hitRead)
-- A purge whose exact target names another identity leaves the entry.
#guard allowed (store ++ [purge "k" (.exact (some 7)) .invalidation .notFound] ++ hitRead)
-- An active purge reports the identity of the entry it removed.
#guard !allowed ([missHandler "k" (m "a") (some (1, some 5)), write 1 [] true true,
  finish 1 (.created 0), purge "k" .active .invalidation (.purged (some 6))])
#guard allowed ([missHandler "k" (m "a") (some (1, some 5)), write 1 [] true true,
  finish 1 (.created 0), purge "k" .active .invalidation (.purged (some 5))])
-- A hit reports its entry's identity.
#guard !allowed ([missHandler "k" (m "a") (some (1, some 5)), write 1 [] true true,
  finish 1 (.created 0), lookup "k" none (.hit 1 (m "a") (some 6))])

-- Expired, then served fresh; served stale is allowed.
#guard !allowed (store ++ [expire "k" .active .expired, lookup "k" none (.hit 1 (m "a") none)])
#guard allowed (store ++ [expire "k" .active .expired,
  lookup "k" none (.hit 1 (m "a" false) none)])
-- Expired where nothing is stored.
#guard !allowed [expire "k" .active .expired]
-- Only expire reports Expired.
#guard !allowed (store ++ [purge "k" .active .invalidation .expired])

-- New metadata replaces the old, and is fresh again.
#guard allowed (store ++ [expire "k" .active .expired, updateMeta "k" (m "b") (some true),
  lookup "k" none (.hit 1 (m "b") none)])
#guard !allowed (store ++ [updateMeta "k" (m "b") (some true),
  lookup "k" none (.hit 1 (m "a") none)])
-- Success where nothing is stored.
#guard !allowed [updateMeta "k" (m "b") (some true)]

-- A write in flight: a hit reads what it has written, and ends only once its
-- last chunk is written.
def flying : List Event :=
  [missHandler "k" (m "a") (some (1, none)), write 1 [1, 2] false true,
   lookup "k" none (.hit 1 (m "a") none), read 1 (.chunk [1, 2])]

#guard allowed (flying ++ [write 1 [3] true true, read 1 (.chunk [3]), read 1 .done])
#guard !allowed (flying ++ [read 1 .done])
-- Dropped before its end: the hit may fail, not end.
#guard allowed (flying ++ [drop 1, read 1 .error])
#guard !allowed (flying ++ [drop 1, read 1 .done])
-- Bytes not yet written.
#guard !allowed (flying ++ [read 1 (.chunk [3])])
-- By the write's tag, the hit is of that write.
#guard allowed ([missHandler "k" (m "a") (some (1, none)),
  lookup "k" (some 1) (.hit 1 (m "a") none)])
#guard !allowed (store ++ [missHandler "k" (m "b") (some (2, none)),
  lookup "k" (some 2) (.hit 1 (m "a") none)])
-- A write that lands after a purge is served: the contract allows it.
#guard allowed ([missHandler "k" (m "a") (some (1, none)),
  purge "k" .active .invalidation .notFound, write 1 [1] true true, finish 1 (.created 1),
  lookup "k" none (.hit 1 (m "a") none), read 1 (.chunk [1]), read 1 .done])
-- No write after its end, nor finish twice.
#guard !allowed (store ++ [write 1 [4] true true])
#guard !allowed (store ++ [finish 1 (.created 3)])

-- A seek reads its range, and may not succeed past the body.
#guard allowed (store ++ [lookup "k" none (.hit 1 (m "a") none), seek 1 1 (some 2) true,
  read 1 (.chunk [2]), read 1 .done, seek 1 0 none true, read 1 (.chunk [1, 2, 3]),
  read 1 .done])
#guard !allowed (store ++ [lookup "k" none (.hit 1 (m "a") none), seek 1 3 none true])
#guard allowed (store ++ [lookup "k" none (.hit 1 (m "a") none), seek 1 3 none false])
-- A range past the body ends at the body's end.
#guard allowed (store ++ [lookup "k" none (.hit 1 (m "a") none), seek 1 2 (some 9) true,
  read 1 (.chunk [3]), read 1 .done])

-- A hit keeps the body it was opened on, though a later store replaces it.
#guard allowed (store ++ [lookup "k" none (.hit 1 (m "a") none),
  missHandler "k" (m "b") (some (2, none)), write 2 [7] true true, finish 2 (.created 1),
  read 1 (.chunk [1, 2, 3]), read 1 .done])

end HttpCacheStorage.Test
