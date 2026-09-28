# Restored guest owner-product verifier

This is a separate, credential-free executable intended to be uploaded into a
fresh Render Sandbox restored from a bound filesystem snapshot. It is **not**
part of the four-entry owner-product tree it measures. It accepts one canonical,
bounded controller challenge and emits one canonical JSON measurement (followed
by one newline): the exact tree manifest, owner executable digest, installed
controller public root, and challenge digest. It does not accept expected
content hashes and does not claim a provider snapshot or Sandbox identity.

This source alone does not qualify a snapshot. The controller still must:

1. Admit and pin this verifier's exact executable bytes separately from the
   retained owner product and snapshot upload.
2. Allocate a fresh denied-network Sandbox from the exact bound snapshot and
   retain the provider's authenticated occurrence identity.
3. Deliver this verifier by an exact, bounded upload; run it once through the
   authenticated provider route with a fresh challenge bound to that occurrence,
   snapshot locator and operation.
4. Retain the exact run identity, complete bounded output, exit and provider
   response evidence. A lost run stream remains uncertain, not a successful
   measurement or permission to relaunch blindly.
5. Independently join the measurement to the captured product manifest,
   controller root, owner executable, durable snapshot locator and authenticated
   restored-Sandbox execution before accepting a product qualification.
6. Prove whole-guest settlement/writer exclusion where the qualification
   requires it. A point measurement is not perpetual immutability.

No provider contact, verifier upload, installed qualification or activation is
performed by this crate alone. The Render activation gate remains closed.
