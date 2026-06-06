//! Kernel "capability space": the global ObjectTable + **per-domain CapTables**,
//! and the live `cap_invoke` dispatch for the keystone kernel. All authority,
//! revocation, type, and rights checks are delegated to the *formally verified*
//! `cap_core` (Kani proofs in crates/cap-core); this module is wiring only and
//! never modifies cap-core.
//!
//! ## Per-domain capability tables (Phase 2)
//! Each protection domain (a scheduler task) owns its own [`CapTable`], so a CPtr
//! names different authority in different domains and one domain cannot reference
//! another's caps — the seL4-style per-domain CSpace from `docs/CAP_ABI.md` §1.
//! Domain 0 is the root/kernel domain (the boot self-tests run here). A domain
//! receives a capability either by **mint** (root) or by **delegate** from another
//! domain with a rights subset (verified I3: never amplifies). The currently-running
//! domain is tracked in [`CURRENT_DOMAIN`], kept current by the scheduler on every
//! switch, so a ring-3 `syscall` resolves its CPtr against *its own* table.
//!
//! ## Object payloads live here, not in cap-core
//! cap-core's `ObjectMeta` is `{kind, epoch, present}` only (it is frozen), so object
//! *state* sits next to the table: the frame allocator backs `Memory`, and [`NOTIFY`]
//! (one pending-bits word per object id) backs `Notification`. cap-core checks
//! authority; the kernel holds the object's contents.
//!
//! ## Concurrency
//! Single-CPU. Every cap path runs with interrupts off (boot self-tests are
//! pre-`sti`; ring-3 syscalls run with IF=0 via SFMASK; admission runs pre-`sti`).
//! `DOMAINS`/`NOTIFY` are therefore plain `static mut` with that discipline (mirrors
//! `sched::SCHED`), const-initialized so there is **no by-value construction on the
//! boot stack** (the original boot-stack-overflow bug came from exactly such a
//! cascade building the 8 KiB `OBJECTS` — see `RESUME.md`/`paging.rs`). `OBJECTS`
//! keeps its proven `Once<Mutex<…>>`. SMP will need per-CPU run queues + real locks.

use cap_core::capability::{ObjectKind, Rights};
use cap_core::table::{CapTable, ObjectTable, Status, CAP_TABLE_SLOTS, OBJECT_TABLE_SIZE};
use core::sync::atomic::{AtomicU16, Ordering};
use spin::{Mutex, Once};

/// Method IDs for the Memory object kind (v0).
pub const M_ALLOC: u16 = 1;
pub const M_FREE: u16 = 2;

/// Method IDs for the Notification object kind (v0 async IPC).
/// `N_SIGNAL` OR-s `arg` bits into the object's pending word (needs INVOKE|WRITE);
/// `N_POLL` atomically reads+clears that word, returning it (needs INVOKE|READ).
/// Blocking wait (`N_WAIT`) arrives with the scheduler block/wake primitive in 4b.
pub const N_SIGNAL: u16 = 1;
pub const N_POLL: u16 = 2;

/// Method id used to validate a TimeSlice capability at admission (kernel-side,
/// like M_ALLOC — not part of cap-core).
pub const M_GRANT_TIME: u16 = 1;

/// Number of protection domains (one CapTable each). Matches `sched::MAX_TASKS`
/// for now (task index == domain id); domain 0 is the root/kernel domain.
pub const MAX_DOMAINS: usize = 4;

/// Global ObjectTable (shared object namespace; lazily initialized — proven path).
static OBJECTS: Once<Mutex<ObjectTable>> = Once::new();

/// Per-domain capability tables. `tables[0]` is the root domain.
struct Domains {
    tables: [CapTable; MAX_DOMAINS],
}

/// An empty CapTable as a `const` (CapTable.slots is public; `Option<CapEntry>` is
/// Copy). Used to const-initialize `DOMAINS` with zero runtime/stack cost.
const EMPTY_TABLE: CapTable = CapTable {
    slots: [None; CAP_TABLE_SLOTS],
};

/// The per-domain tables. **const static-init** (no boot-stack temporary).
/// SAFETY contract: single-CPU, accessed only with interrupts off (see module docs).
static mut DOMAINS: Domains = Domains {
    tables: [EMPTY_TABLE; MAX_DOMAINS],
};

/// Notification pending-bits payload, indexed by `object_id`. cap-core carries no
/// payload, so notification state lives here. const static-init (no stack temporary).
static mut NOTIFY: [u64; OBJECT_TABLE_SIZE] = [0; OBJECT_TABLE_SIZE];

/// The domain whose caps a bare [`cap_invoke`] resolves against — i.e. the running
/// task's domain. The scheduler writes this on every context switch; defaults to 0
/// (root) for the boot self-tests, which run before any task is scheduled.
static CURRENT_DOMAIN: AtomicU16 = AtomicU16::new(0);

#[inline]
fn objects() -> &'static Mutex<ObjectTable> {
    OBJECTS.call_once(|| Mutex::new(ObjectTable::new()))
}

/// Scheduler hook: record the domain of the task about to run, so a subsequent
/// ring-3 `syscall` routes [`cap_invoke`] to the caller's own CapTable.
#[inline]
pub fn set_current_domain(domain: u16) {
    CURRENT_DOMAIN.store(domain, Ordering::Relaxed);
}

#[inline]
fn current_domain() -> usize {
    (CURRENT_DOMAIN.load(Ordering::Relaxed) as usize).min(MAX_DOMAINS - 1)
}

/// Create a Memory object and mint a fully-powered capability for it in the ROOT
/// domain (domain 0): rights = READ|WRITE|INVOKE|MAP|DELEGATE, badge 0. DELEGATE is
/// included so root can hand a rights-subset copy to another domain
/// ([`delegate_from_root`]). Returns the root-domain CPtr.
pub fn init_root() -> Option<u16> {
    let mut objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    let object_id = objs_g.create_object(ObjectKind::Memory)?;
    let rights = Rights::READ | Rights::WRITE | Rights::INVOKE | Rights::MAP | Rights::DELEGATE;
    doms.tables[0].mint(&*objs_g, object_id, rights, 0).ok()
}

/// Create a Notification object and mint a fully-powered cap in the ROOT domain:
/// rights = INVOKE|READ|WRITE|DELEGATE (signal + poll + hand-off). Returns the root CPtr.
pub fn create_notification() -> Option<u16> {
    let mut objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    let oid = objs_g.create_object(ObjectKind::Notification)?;
    let rights = Rights::INVOKE | Rights::READ | Rights::WRITE | Rights::DELEGATE;
    doms.tables[0].mint(&*objs_g, oid, rights, 0).ok()
}

/// Delegate the capability at root-domain CPtr `src_cptr` into `dst_domain` with
/// `child.rights = src.rights & mask` (verified I3 — cannot amplify). Returns the
/// destination-domain CPtr. `dst_domain` must be a non-root domain (1..MAX_DOMAINS).
pub fn delegate_from_root(dst_domain: usize, src_cptr: u16, mask: Rights, badge: u16) -> Option<u16> {
    if dst_domain == 0 || dst_domain >= MAX_DOMAINS {
        return None;
    }
    let objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    // Split so root (index 0, in `head`) is borrowed immutably while the destination
    // (first of `tail`) is borrowed mutably — `dst_domain >= 1`, so `head` is non-empty.
    let (head, tail) = doms.tables.split_at_mut(dst_domain);
    head[0]
        .delegate(&*objs_g, src_cptr, mask, badge, &mut tail[0])
        .ok()
}

/// Invoke a method on the capability `cptr` in the CURRENT domain's table (the
/// running task's domain). This is the entry point the ring-3 `syscall` path uses.
pub fn cap_invoke(cptr: u16, method: u16, arg: u64) -> (Status, u64) {
    cap_invoke_in(current_domain(), cptr, method, arg)
}

/// Invoke a method on `cptr` in a specific `domain`'s table.
///
/// The verified [`CapTable::invoke`] runs in the live path (presence, live epoch,
/// type match, INVOKE right — I1/I2/I4). We then layer any *method-specific* right
/// (Notification SIGNAL→WRITE, POLL→READ) and dispatch on `(kind, method)`. The
/// cap-table view is dropped before touching object payloads (frame allocator /
/// NOTIFY) so there is no lock-order inversion against `FRAME_ALLOCATOR`.
pub fn cap_invoke_in(domain: usize, cptr: u16, method: u16, arg: u64) -> (Status, u64) {
    let domain = domain.min(MAX_DOMAINS - 1);

    let (kind, oid, rights) = {
        let objs_g = objects().lock();
        // SAFETY: single-CPU, IRQs off; shared read of DOMAINS.
        let doms = unsafe { &*core::ptr::addr_of!(DOMAINS) };
        let tbl = &doms.tables[domain];

        let st = tbl.invoke(&*objs_g, cptr, method, Rights::INVOKE);
        if st != Status::Ok {
            return (st, 0);
        }
        match tbl.lookup(cptr, &*objs_g) {
            Ok(e) => (e.type_tag, e.object_id, e.rights),
            Err(st) => return (st, 0),
        }
        // OBJECTS guard + DOMAINS view drop here, before any allocator / NOTIFY access.
    };

    // Method-specific right beyond the INVOKE already enforced above.
    let extra = match (kind, method) {
        (ObjectKind::Notification, N_SIGNAL) => Rights::WRITE,
        (ObjectKind::Notification, N_POLL) => Rights::READ,
        _ => Rights::empty(),
    };
    if !rights.contains(extra) {
        return (Status::ErrRights, 0);
    }

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
        // Notification async IPC: SIGNAL OR-s bits into the pending word; POLL
        // reads+clears it. Cross-domain: a domain holding only a SIGNAL (INVOKE|WRITE)
        // cap can raise bits another domain (INVOKE|READ) later observes.
        (ObjectKind::Notification, N_SIGNAL) => {
            notify_signal(oid, arg);
            (Status::Ok, 0)
        }
        (ObjectKind::Notification, N_POLL) => (Status::Ok, notify_take(oid)),
        _ => (Status::ErrMethod, 0),
    }
}

/// OR `bits` into the Notification object's pending word.
fn notify_signal(oid: u64, bits: u64) {
    let i = oid as usize;
    if i < OBJECT_TABLE_SIZE {
        // SAFETY: single-CPU, IRQs off; `i` bounds-checked.
        unsafe {
            (*core::ptr::addr_of_mut!(NOTIFY))[i] |= bits;
        }
    }
}

/// Read and clear the Notification object's pending word (returns the bits).
fn notify_take(oid: u64) -> u64 {
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return 0;
    }
    // SAFETY: single-CPU, IRQs off; `i` bounds-checked.
    unsafe {
        let n = &mut *core::ptr::addr_of_mut!(NOTIFY);
        let v = n[i];
        n[i] = 0;
        v
    }
}

/// Demonstrate the verified revocation invariant at runtime against the ROOT domain:
/// look up the cap (to obtain its object_id), call the verified `ObjectTable::revoke`
/// (epoch bump), then re-invoke — which must now fail with `ErrRevoked` because the
/// live epoch no longer matches the one captured in the minted CapEntry (I2).
pub fn demo_revoke_then_invoke(cptr: u16) -> Status {
    let object_id = {
        let objs_g = objects().lock();
        // SAFETY: single-CPU, IRQs off; shared read of DOMAINS.
        let doms = unsafe { &*core::ptr::addr_of!(DOMAINS) };
        match doms.tables[0].lookup(cptr, &*objs_g) {
            Ok(entry) => entry.object_id,
            Err(_) => return Status::ErrBadCPtr,
        }
    };

    {
        let mut objs_g = objects().lock();
        let _ = objs_g.revoke(object_id);
    }

    let (st, _) = cap_invoke_in(0, cptr, M_ALLOC, 0);
    st
}

/// Create a TimeSlice object and mint an INVOKE capability for it in the ROOT domain.
/// "Time is a capability": holding this cap is what admits a task to the EDF run
/// queue (see `sched::admit`). Deadline/period numbers are NOT stored in the cap (the
/// 128-bit layout is frozen) — they pass as plain args to `sched::admit`.
pub fn mint_timeslice() -> Option<u16> {
    let mut objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    let object_id = objs_g.create_object(ObjectKind::TimeSlice)?;
    doms.tables[0].mint(&*objs_g, object_id, Rights::INVOKE, 0).ok()
}

/// Admission gate: the ROOT-domain cap at `cptr` must validate via the VERIFIED
/// `CapTable::invoke` (type == TimeSlice, live epoch, INVOKE right). A revoked cap
/// fails with `ErrRevoked`. (Admission is a kernel operation performed on behalf of
/// task creation, so the TimeSlice cap lives in the root domain.)
pub fn admit_check(cptr: u16) -> Status {
    let objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; shared read of DOMAINS.
    let doms = unsafe { &*core::ptr::addr_of!(DOMAINS) };
    doms.tables[0].invoke(&*objs_g, cptr, M_GRANT_TIME, Rights::INVOKE)
}

/// Revoke a ROOT-domain TimeSlice cap's object (epoch bump) — for the live revocation demo.
pub fn revoke_timeslice(cptr: u16) -> Status {
    let object_id = {
        let objs_g = objects().lock();
        // SAFETY: single-CPU, IRQs off; shared read of DOMAINS.
        let doms = unsafe { &*core::ptr::addr_of!(DOMAINS) };
        match doms.tables[0].lookup(cptr, &*objs_g) {
            Ok(entry) => entry.object_id,
            Err(st) => return st,
        }
    };
    let mut objs_g = objects().lock();
    objs_g.revoke(object_id)
}
