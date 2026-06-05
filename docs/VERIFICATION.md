# Cairn Verification Strategy — "verify from the start"

Decision (2026-06-05): we write **formal proofs alongside the code**, seL4-style
in ambition, from the first commit. This document is honest about what that means
on a 2026 Rust/x86 stack — what we can truly prove now, what is aspirational, and
how it gates CI.

## 1. What "verify from the start" realistically means here

seL4's end-to-end proof (abstract spec → C → binary) took ~20+ person-years for
~9k LoC, in Isabelle/HOL. We are *not* claiming that on day one. Instead we adopt
the **discipline** now and grow the proof with the code:

1. **Tiny, isolating TCB by construction.** Authority logic lives in
   `crates/cap-core` (no `unsafe`, fixed-size data structures) so it is
   *tractable* to verify and so a bug there can't be hidden by the rest of the
   system. This is the single most important "verification" decision — it is
   architectural, not a tool.
2. **Machine-checked proofs of the core invariants** (I1–I4 in `CAP_ABI.md`)
   from commit one, using tools that run on our stack today.
3. **A documented refinement target**: the long-range aspiration is a
   refinement proof from the `CAP_ABI.md` abstract model down to the
   implementation. We track the gap explicitly rather than pretend it's closed.

## 2. The toolchain (Linux / WSL2)

| Tool | Role | What it gives us | Maturity on our stack |
|------|------|------------------|-----------------------|
| **Kani** (model checker, CBMC-backed) | Primary | Bounded proofs over *real* Rust incl. `unsafe`; memory safety + assertions + our I1–I4 harnesses | Strong; Linux/macOS. Our day-one prover. |
| **Creusot** (deductive, Why3/SMT) | Functional correctness | Unbounded proofs of functional contracts (`#[ensures]`/`#[requires]`) on the cap algebra | Adopt as the core stabilizes |
| **Miri** | UB detector | Catches undefined behavior in `unsafe` kernel code under an interpreter | Runs now (incl. Windows); cheap to keep on |
| **proptest** | Fast feedback | Randomized property tests mirroring every Kani harness, run on every `cargo test` | Now |
| **`#[deny(unsafe_code)]`** | Static | Forbids `unsafe` in `cap-core`; kernel `unsafe` is allow-listed + reviewed | Now |

Rationale for Kani-first: it verifies the *actual* compiled Rust (no separate
model to drift), handles `unsafe`, and the I1–I4 properties are naturally
bounded (fixed-capacity tables), which is exactly Kani's sweet spot.

## 3. What we prove, in order

- **Now (Phase 0/1):** I1–I4 on `cap-core` via Kani; proptest mirrors; Miri on
  any `unsafe`. Every PR must pass `cargo kani -p cap-core`.
- **Phase 1:** verified bounds on the frame allocator (no double-free, no alias
  of a live frame) and the cap-table indexing (no out-of-bounds slot).
- **Phase 2:** scheduler admission safety (a domain never runs without a live
  TimeSlice cap); IPC channel protocol does not deadlock the core.
- **Phase 3+:** crash-consistency of the object store (Crash-Hoare-style, à la
  FSCQ) for the persistence log; this is research-grade and tracked as such.

## 4. CI gating

The kernel build, host unit tests, Kani proofs, and `clippy -D warnings` all run
per-commit. **A failing or weakened proof blocks merge.** Proof harnesses live
next to the code under `#[cfg(kani)]` so they cannot rot silently.

## 5. Honest limits

- Kani is **bounded**: proofs hold up to configured sizes (e.g. table capacity,
  loop unwinds). We pick bounds ≥ the real fixed capacities so the bound *is* the
  whole state space where possible; where it isn't, we say so.
- We do **not** yet verify the compiler, the bootloader, or hardware behavior.
  Confidential boot (Phase 5) addresses the hardware-trust dimension separately.
- A full seL4-class binary-level proof is a multi-year track; we are building the
  *architecture* that makes it possible, and closing the gap incrementally.
