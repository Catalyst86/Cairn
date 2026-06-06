//! Kernel "capability space" owning the global ObjectTable + root CapTable.
//! Implements `cap_invoke` dispatch for the live keystone kernel, delegating
//! all authority, revocation, and rights checks to the *formally verified*
//! `cap_core` (Kani proofs in crates/cap-core).
//!
//! This module is the wiring layer only; cap-core itself is never modified here.

use cap_core::capability::{ObjectKind, Rights};
use cap_core::table::{CapTable, ObjectTable, Status};
use spin::{Mutex, Once};

/// Method IDs for the Memory object kind (v0).
pub const M_ALLOC: u16 = 1;
pub const M_FREE: u16 = 2;

/// Method id used to validate a TimeSlice capability at admission (kernel-side,
/// like M_ALLOC — not part of cap-core).
pub const M_GRANT_TIME: u16 = 1;

/// Global ObjectTable (lazily initialized because ObjectTable::new is not const).
static OBJECTS: Once<Mutex<ObjectTable>> = Once::new();

/// Global root CapTable for the initial kernel domain.
static ROOT_CAPS: Once<Mutex<CapTable>> = Once::new();

/// Helper returning the static ObjectTable behind a spin Mutex (call_once ensures
/// a single initialization even though new() is not const).
#[inline]
fn objects() -> &'static Mutex<ObjectTable> {
    OBJECTS.call_once(|| Mutex::new(ObjectTable::new()))
}

/// Helper for the root capability table.
#[inline]
fn root_caps() -> &'static Mutex<CapTable> {
    ROOT_CAPS.call_once(|| Mutex::new(CapTable::new()))
}

/// Initialize the global tables, create a Memory object, and mint a fully-powered
/// capability for it in the root CapTable (rights = READ|WRITE|INVOKE|MAP, badge=0).
/// Returns the CPtr (slot index) on success.
pub fn init_root() -> Option<u16> {
    let objs = objects();
    let caps = root_caps();

    let mut objects_g = objs.lock();
    let mut caps_g = caps.lock();

    let object_id = objects_g.create_object(ObjectKind::Memory)?;

    let rights = Rights::READ | Rights::WRITE | Rights::INVOKE | Rights::MAP;
    caps_g.mint(&*objects_g, object_id, rights, 0).ok()
}

/// Invoke a method on the capability referenced by `cptr`.
///
/// 1. Acquires both OBJECTS and ROOT_CAPS locks.
/// 2. Calls the *verified* `CapTable::invoke` (and `lookup`) which enforces
///    epoch, type match, presence, and required INVOKE right. Any failure
///    (including post-revoke ErrRevoked) is returned immediately.
/// 3. **Crucially drops the cap locks before dispatching** to avoid holding
///    the cap tables' Mutex while calling into `crate::memory`, which takes
///    its own `FRAME_ALLOCATOR: Mutex<...>`. This eliminates any lock-order
///    inversion risk. (We are still early-boot single-CPU with IRQs off, but
///    the discipline is correct for future.)
/// 4. Dispatches on (type_tag, method) to the real kernel memory ops.
pub fn cap_invoke(cptr: u16, method: u16, arg: u64) -> (Status, u64) {
    // Extract the object kind (and rely on prior invoke+lookup checks) under the
    // verified tables' locks, then release before any kernel allocator call.
    let kind = {
        let objects_g = objects().lock();
        let caps_g = root_caps().lock();

        let st = caps_g.invoke(&*objects_g, cptr, method, Rights::INVOKE);
        if st != Status::Ok {
            return (st, 0);
        }

        match caps_g.lookup(cptr, &*objects_g) {
            Ok(entry) => entry.type_tag,
            Err(st) => return (st, 0),
        }
        // MutexGuards for OBJECTS and ROOT_CAPS are dropped at the end of this
        // block. The subsequent match arm may call crate::memory::allocate_frame
        // / deallocate_frame which lock FRAME_ALLOCATOR independently.
        // No simultaneous hold => no lock-ordering hazard.
    };

    match (kind, method) {
        (ObjectKind::Memory, M_ALLOC) => match crate::memory::allocate_frame() {
            Some(frame) => (Status::Ok, frame),
            None => (Status::ErrRights, 0), // OOM surfaced as ErrRights per spec
        },
        // M_FREE is intentionally NOT honored in v0. `arg` is an untrusted frame
        // number and there is no per-frame ownership tracking yet, so freeing it
        // would let a caller return a frame it never owned (or double-free) into the
        // allocator and later re-`M_ALLOC` a kernel-owned frame straight into its own
        // address space — a privilege escalation. Reachable from ring 3 today (the
        // syscall path forwards method+arg0 verbatim), so we refuse rather than defer
        // silently. A safe free requires M_ALLOC to hand back a Memory *capability*
        // (not a raw frame number) the kernel can ownership-check at free time — the
        // "return a Memory CPtr, not a raw frame" roadmap item.
        (ObjectKind::Memory, M_FREE) => {
            let _ = arg; // untrusted; deliberately not passed to the allocator
            (Status::ErrMethod, 0)
        }
        _ => (Status::ErrMethod, 0),
    }
}

/// Demonstrate the verified revocation invariant at runtime:
/// Look up the cap (to obtain its object_id), call the verified
/// `ObjectTable::revoke` (which bumps the epoch for that object), then
/// perform a cap_invoke which must now fail with ErrRevoked because the
/// live epoch no longer matches the one captured in the minted CapEntry.
pub fn demo_revoke_then_invoke(cptr: u16) -> Status {
    // Snapshot the object_id while the tables are consistent (under locks).
    let object_id = {
        let objects_g = objects().lock();
        let caps_g = root_caps().lock();

        match caps_g.lookup(cptr, &*objects_g) {
            Ok(entry) => entry.object_id,
            Err(_) => return Status::ErrBadCPtr,
        }
        // guards drop here
    };

    // Revoke via the verified path (epoch bump). Only objects needs &mut.
    {
        let mut objects_g = objects().lock();
        let _ = objects_g.revoke(object_id);
    }

    // The original cptr is now invalid per the epoch check inside verified invoke/lookup.
    let (st, _) = cap_invoke(cptr, M_ALLOC, 0);
    st
}

/// Create a TimeSlice object and mint an INVOKE capability for it in ROOT_CAPS.
/// Returns the CPtr. "Time is a capability": holding this cap is what admits a task
/// to the EDF run queue (see sched::admit). Deadline/period numbers are NOT stored in
/// the cap (the 128-bit layout is frozen) — they pass as plain args to sched::admit.
pub fn mint_timeslice() -> Option<u16> {
    let objs = objects();
    let caps = root_caps();
    let mut objects_g = objs.lock();
    let mut caps_g = caps.lock();
    let object_id = objects_g.create_object(ObjectKind::TimeSlice)?;
    caps_g.mint(&*objects_g, object_id, Rights::INVOKE, 0).ok()
}

/// Admission gate: the cap at `cptr` must validate via the VERIFIED CapTable::invoke
/// (type == TimeSlice, live epoch, INVOKE right). A revoked cap fails with ErrRevoked.
pub fn admit_check(cptr: u16) -> Status {
    let objects_g = objects().lock();
    let caps_g = root_caps().lock();
    caps_g.invoke(&*objects_g, cptr, M_GRANT_TIME, Rights::INVOKE)
}

/// Revoke a TimeSlice cap's object (epoch bump) — for the live revocation demo.
pub fn revoke_timeslice(cptr: u16) -> Status {
    let object_id = {
        let objects_g = objects().lock();
        let caps_g = root_caps().lock();
        match caps_g.lookup(cptr, &*objects_g) {
            Ok(entry) => entry.object_id,
            Err(st) => return st,
        }
    };
    let mut objects_g = objects().lock();
    objects_g.revoke(object_id)
}
