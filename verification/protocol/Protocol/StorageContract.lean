-- SPDX-FileCopyrightText: 2026 The durable-cache authors
-- SPDX-License-Identifier: Apache-2.0

-- IMP-TC.VERIFICATION.CONTRACT-01: satisfy TC-VERIFICATION.CONTRACT-01

import HttpCacheStorage
import Protocol.Axioms

/-!
TC-VERIFICATION.CONTRACT-01, step 1: the storage contract of the
`HttpCacheStorage` library allows every trace of its reference
implementation, for any calls in any order, so that the contract the traces
of steps 2 and 3 are checked against is one a storage can keep.
-/

open HttpCacheStorage in
theorem tc_verification_contract_01_reference_conforms (calls : List Reference.Call) :
    ∃ s, check (Reference.run {} calls) = .ok s :=
  Reference.conforms calls

#assert_axioms tc_verification_contract_01_reference_conforms
