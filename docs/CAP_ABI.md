# Cairn Capability ABI (v0)

The entire Cairn system call surface is **one primitive**: `cap_invoke`. Every
operation — allocating memory, sending a message, scheduling, mapping a device
queue, reading a stored object — is a *method invocation on a capability*. There
are no files, file descriptors, UIDs, `open`/`read`/`write`, or `ioctl`.

This document specifies, for v0: the capability format, the capability table,
epoch-based revocation, the `cap_invoke` calling convention, and the safety
invariants we will **formally prove** (see `VERIFICATION.md`).

---

## 1. Model

Authority lives in **per-domain capability tables** held by the kernel (the
`keystone` core). User code never sees raw capability bits; it references a
capability by a small integer **CPtr** (capability pointer = a slot index into
its own table), much like a file descriptor but for *any* authority. Because the
table is kernel-protected, capabilities are **unforgeable**: a domain cannot
fabricate authority it was never granted.

Objects (memory regions, endpoints, domains, time-slices, storage extents, …)
live in a kernel **object table**. Each object carries a current **epoch**.

```
 Domain A                         keystone (kernel)
 ┌──────────────┐    CPtr 7   ┌───────────────── CapTable[A] ─────────────────┐
 │ cap_invoke(7)│ ──────────▶ │ slot 7: {obj_id, rights, epoch, type}         │
 └──────────────┘             └───────────────────────────────────────────────┘
                                          │ obj_id
                                          ▼
                              ┌──────── ObjectTable ────────┐
                              │ obj: {kind, epoch, payload} │
                              └─────────────────────────────┘
```

## 2. Capability entry format (128 bits)

The value stored in a capability-table **slot** (kernel-side). v0 layout:

| Bits      | Field        | Width | Meaning                                            |
|-----------|--------------|-------|----------------------------------------------------|
| `[0,48)`  | `object_id`  | 48    | Index into the kernel object table (≤ 2⁴⁸ objects) |
| `[48,64)` | `rights`     | 16    | Rights bitflags (below)                            |
| `[64,96)` | `epoch`      | 32    | Epoch captured at mint time (for revocation)       |
| `[96,112)`| `type_tag`   | 16    | Object kind the cap is typed to                    |
| `[112,128)`| `badge`     | 16    | Caller-set badge / provenance discriminator        |

`badge` lets a server distinguish callers without extra state (seL4-style
badging). `type_tag` makes invocations **type-checked** at the kernel boundary.

### Rights (16-bit `rights` bitflags)

```
READ      = 1 << 0   // observe object state
WRITE     = 1 << 1   // mutate object state
INVOKE    = 1 << 2   // call methods / send on an endpoint
DELEGATE  = 1 << 3   // grant a (sub-)copy to another domain
REVOKE    = 1 << 4   // revoke this object's outstanding caps
MAP       = 1 << 5   // map a memory/extent object into an address space
GRANT_CAP = 1 << 6   // transfer capabilities through this endpoint
SEAL      = 1 << 7   // create/forbid further derivation (monotonic)
//  bits 8..16 reserved for object-kind-specific rights
```

### Object kinds (`type_tag`)

`Null, Untyped, Memory, AddressSpace, Domain, Endpoint, Notification, TimeSlice,
Extent (persistent store), DeviceQueue, CapTable, IrqHandler`. New hardware
(CXL pools, accelerators) is added as new kinds — the core need not understand
their semantics, only multiplex them.

## 3. Epoch-based revocation (O(1))

Each object holds `current_epoch`. A capability entry stores the `epoch` it was
minted at. On every invocation the core checks:

```
valid(slot) ⇔ slot.type_tag == object[slot.object_id].kind
            ∧ slot.epoch    == object[slot.object_id].current_epoch
```

`revoke(obj)` simply does `object[obj].current_epoch += 1`. This **instantly and
completely** invalidates every outstanding capability to that object across all
domains — the property that lets us yank authority from a compromised service in
constant time. (32-bit epochs; wrap is handled by retiring the object id.)

## 4. Derivation: mint, delegate, seal

- `mint(untyped, kind, rights) -> CPtr` — carve a new typed object from an
  Untyped region; the new cap gets `rights ⊆` what the Untyped grant allowed.
- `delegate(src CPtr, mask, badge, dst Domain) -> CPtr` — copy authority to
  another domain with `child.rights = src.rights & mask`. **Never amplifies**:
  `child.rights ⊆ src.rights`. Requires `DELEGATE` on `src`.
- `seal(CPtr)` — clears `DELEGATE` monotonically, freezing further spread.

## 5. The `cap_invoke` calling convention (x86-64)

A single syscall (`syscall`/`sysret`). Register ABI:

| Reg   | In                              | Out                         |
|-------|---------------------------------|-----------------------------|
| `rax` | syscall number `SYS_CAP_INVOKE` | status code                 |
| `rdi` | target **CPtr**                 | —                           |
| `rsi` | method id                       | reply value / reply length  |
| `rdx` | arg0                            | reply CPtr (if any)         |
| `r10` | arg1                            | —                           |
| `r8`  | arg2                            | —                           |
| `r9`  | transfer **CPtr** (or `CPTR_NULL`) | —                        |

Semantics: validate `rdi` against the caller's CapTable (§3); dispatch `method`
to the object named by `object_id`, checked against `type_tag` + `rights`;
optionally move/copy the capability in `r9` to the callee (zero-copy authority
transfer). Blocking vs. non-blocking is a property of the target (Endpoint vs.
Notification). **Data** moves by shared memory granted via Memory/Extent caps —
the core is never in the bulk data path.

### Status codes (v0)

`OK=0, ErrBadCPtr, ErrRevoked, ErrType, ErrRights, ErrMethod, ErrWouldBlock,
ErrNoReceiver, ErrFault`.

## 6. Invariants we prove (see VERIFICATION.md)

- **I1 — Unforgeability / confinement.** `invoke` succeeds only for a slot whose
  entry is `valid` (correct type + live epoch); no sequence of user operations
  yields authority not derivable from an initially granted cap.
- **I2 — Revocation completeness.** After `revoke(o)`, every pre-existing cap to
  `o` fails with `ErrRevoked` until re-minted.
- **I3 — No rights amplification.** For all `delegate`: `child.rights ⊆
  parent.rights`; for all `seal`: rights are monotonically non-increasing.
- **I4 — Type safety.** A method dispatches only if `slot.type_tag` matches the
  object kind and the method is permitted by `rights`.

These are stated as Kani proof harnesses against `crates/cap-core` from the very
first commit.

## 7. Open questions (v0 → v1)

- 48-bit object ids vs. a generational `(index, gen)` pair to retire epoch wrap.
- Partitioned (table/CPtr) vs. sparse (MAC'd 128-bit token) caps for cross-node
  / persisted capabilities — likely **both**: CPtr in-core, sealed sparse tokens
  for the persistent store and future multi-node fabric.
- Badge allocation policy and per-endpoint badge namespaces.
