-- SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
-- SPDX-License-Identifier: Apache-2.0

import Lean.Data.Json
import HttpCacheStorage.Contract

/-!
# Traces

A trace is a file of JSON lines, one call per line, in the order a driver
made them. Bytes are lowercase hexadecimal strings; an absent identity or
range end is `null`. The calls, with the forms of their results:

```
{"op":"lookup","key":K,"tag":W|null,"result":"miss"|"error"|{"hit":H,"meta":M,"id":I}}
{"op":"read","hit":H,"result":"done"|"error"|{"chunk":BYTES}}
{"op":"seek","hit":H,"start":N,"stop":N|null,"ok":BOOL}
{"op":"finish_hit","hit":H}
{"op":"miss_handler","key":K,"meta":M,"result":"error"|{"writer":W,"id":I}}
{"op":"write","writer":W,"data":BYTES,"eof":BOOL,"ok":BOOL}
{"op":"finish","writer":W,"result":"error"|{"created":N}|{"appended":[N,N|null]}}
{"op":"drop","writer":W}
{"op":"purge","key":K,"target":T,"kind":"eviction"|"invalidation","outcome":O}
{"op":"expire","key":K,"target":T,"outcome":O}
{"op":"update_meta","key":K,"meta":M,"result":true|false|"error"}
```

where `M` is `{"label":STRING,"fresh":BOOL}`, `I` a number or `null`, `T`
is `"active"` or `{"exact":I}`, and `O` is `"not_found"`, `"expired"`,
`"error"` or `{"purged":I}`.
-/

namespace HttpCacheStorage.Trace

open Lean (Json)

/-- The value of one hexadecimal digit. -/
def hexDigit (c : Char) : Except String UInt8 :=
  if '0' ≤ c && c ≤ '9' then .ok (c.toNat - '0'.toNat).toUInt8
  else if 'a' ≤ c && c ≤ 'f' then .ok (c.toNat - 'a'.toNat + 10).toUInt8
  else .error s!"'{c}' is not a lowercase hexadecimal digit"

/-- The bytes a hexadecimal string spells. -/
def hexBytes (s : String) : Except String Bytes :=
  go s.toList []
where
  go : List Char → List UInt8 → Except String Bytes
    | [], acc => .ok acc.reverse
    | [_], _ => .error "an odd number of hexadecimal digits"
    | hi :: lo :: rest, acc => do
      let h ← hexDigit hi
      let l ← hexDigit lo
      go rest ((h <<< 4 ||| l) :: acc)

def field (j : Json) (name : String) : Except String Json :=
  j.getObjVal? name

def natField (j : Json) (name : String) : Except String Nat := do
  (← field j name).getNat?

def strField (j : Json) (name : String) : Except String String := do
  (← field j name).getStr?

def boolField (j : Json) (name : String) : Except String Bool := do
  (← field j name).getBool?

/-- A number, or `none` for `null`. -/
def optNat (j : Json) : Except String (Option Nat) :=
  if j.isNull then .ok none else some <$> j.getNat?

def optNatField (j : Json) (name : String) : Except String (Option Nat) := do
  optNat (← field j name)

def metaOf (j : Json) : Except String Meta := do
  return { label := ← strField j "label", fresh := ← boolField j "fresh" }

/-- A result that is one of a few strings or an object with one field. -/
def tagged (j : Json) : Except String (String × Json) :=
  match j with
  | .str s => .ok (s, .null)
  | .obj kvs => match kvs.toList with
    | [(k, v)] => .ok (k, v)
    | _ => .error s!"expected an object with one field, got {j.compress}"
  | _ => .error s!"expected a string or an object, got {j.compress}"

def targetOf (j : Json) : Except String Target := do
  match ← tagged j with
  | ("active", _) => return .active
  | ("exact", id) => return .exact (← optNat id)
  | (t, _) => throw s!"unknown target {t}"

def outcomeOf (j : Json) : Except String Outcome := do
  match ← tagged j with
  | ("not_found", _) => return .notFound
  | ("expired", _) => return .expired
  | ("error", _) => return .error
  | ("purged", id) => return .purged (← optNat id)
  | (o, _) => throw s!"unknown outcome {o}"

/-- The call one line of a trace records. -/
def eventOf (j : Json) : Except String Event := do
  match ← strField j "op" with
  | "lookup" =>
    let key ← strField j "key"
    let tag ← optNatField j "tag"
    let result ← match ← field j "result" with
      | .str "miss" => pure LookupResult.miss
      | .str "error" => pure LookupResult.error
      | r@(.obj _) =>
        pure (LookupResult.hit (← natField r "hit") (← metaOf (← field r "meta"))
          (← optNatField r "id"))
      | r => throw s!"unknown lookup result {r.compress}"
    return .lookup key tag result
  | "read" =>
    let hit ← natField j "hit"
    let result ← match ← tagged (← field j "result") with
      | ("done", _) => pure ReadResult.done
      | ("error", _) => pure ReadResult.error
      | ("chunk", data) => pure (ReadResult.chunk (← hexBytes (← data.getStr?)))
      | (r, _) => throw s!"unknown read result {r}"
    return .read hit result
  | "seek" =>
    return .seek (← natField j "hit") (← natField j "start") (← optNatField j "stop")
      (← boolField j "ok")
  | "finish_hit" => return .finishHit (← natField j "hit")
  | "miss_handler" =>
    let key ← strField j "key"
    let m ← metaOf (← field j "meta")
    let result ← match ← field j "result" with
      | .str "error" => pure none
      | r@(.obj _) => pure (some (← natField r "writer", ← optNatField r "id"))
      | r => throw s!"unknown miss_handler result {r.compress}"
    return .missHandler key m result
  | "write" =>
    return .write (← natField j "writer") (← hexBytes (← strField j "data"))
      (← boolField j "eof") (← boolField j "ok")
  | "finish" =>
    let writer ← natField j "writer"
    let result ← match ← tagged (← field j "result") with
      | ("error", _) => pure FinishResult.error
      | ("created", n) => pure (FinishResult.created (← n.getNat?))
      | ("appended", v) => do
        let size ← v.getArrVal? 0 >>= (·.getNat?)
        let max ← v.getArrVal? 1 >>= optNat
        pure (FinishResult.appended size max)
      | (r, _) => throw s!"unknown finish result {r}"
    return .finish writer result
  | "drop" => return .drop (← natField j "writer")
  | "purge" =>
    let kind ← match ← strField j "kind" with
      | "eviction" => pure PurgeKind.eviction
      | "invalidation" => pure PurgeKind.invalidation
      | k => throw s!"unknown purge kind {k}"
    return .purge (← strField j "key") (← targetOf (← field j "target")) kind
      (← outcomeOf (← field j "outcome"))
  | "expire" =>
    return .expire (← strField j "key") (← targetOf (← field j "target"))
      (← outcomeOf (← field j "outcome"))
  | "update_meta" =>
    let result ← match ← field j "result" with
      | .bool b => pure (some b)
      | .str "error" => pure none
      | r => throw s!"unknown update_meta result {r.compress}"
    return .updateMeta (← strField j "key") (← metaOf (← field j "meta")) result
  | op => throw s!"unknown call {op}"

/-- The calls a trace's text records, or the first line it cannot read. -/
def parse (text : String) : Except String (List Event) :=
  go 1 (text.splitOn "\n") []
where
  go (n : Nat) : List String → List Event → Except String (List Event)
    | [], acc => .ok acc.reverse
    | line :: rest, acc =>
      if line.trimAscii.isEmpty then go (n + 1) rest acc
      else match Json.parse line >>= eventOf with
        | .ok e => go (n + 1) rest (e :: acc)
        | .error why => .error s!"line {n}: {why}"

end HttpCacheStorage.Trace
