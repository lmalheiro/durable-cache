-- SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
-- SPDX-License-Identifier: Apache-2.0

import HttpCacheStorage

/-!
`check-trace FILE...`: check each trace against the storage contract. Prints
one line for each, and exits with 1 if any cannot be read or holds a call
whose result the contract does not allow.
-/

open HttpCacheStorage

def checkFile (path : String) : IO Bool := do
  let text ← IO.FS.readFile path
  match Trace.parse text with
  | .error why =>
    IO.eprintln s!"{path}: not a trace: {why}"
    return false
  | .ok events =>
    match check events with
    | .ok _ =>
      IO.println s!"{path}: {events.length} calls, each allowed by the contract"
      return true
    | .error (i, why) =>
      IO.eprintln s!"{path}: call {i + 1} of {events.length} (line {i + 1}) is not allowed: {why}"
      return false

def main (args : List String) : IO UInt32 := do
  if args.isEmpty then
    IO.eprintln "usage: check-trace FILE..."
    return 2
  let mut ok := true
  for path in args do
    ok := (← checkFile path) && ok
  return if ok then 0 else 1
