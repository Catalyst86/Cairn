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
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use spin::{Mutex, Once};

/// Method IDs for the Memory object kind (v0).
pub const M_ALLOC: u16 = 1;
pub const M_FREE: u16 = 2;

/// Method IDs for the Notification object kind (v0 async IPC).
/// `N_SIGNAL` OR-s `arg` bits into the object's pending word (needs INVOKE|WRITE);
/// `N_POLL` atomically reads+clears that word, returning it (needs INVOKE|READ).
/// A blocking wait (`N_WAIT`) is a natural extension of the 4b block/wake primitive,
/// deferred to 4c.
pub const N_SIGNAL: u16 = 1;
pub const N_POLL: u16 = 2;

/// Method IDs for the Endpoint object kind (v0 synchronous IPC, portal step 4b).
/// `E_SEND` (needs INVOKE; GRANT_CAP additionally to transfer a cap) and `E_RECV`
/// (needs INVOKE) perform a single-waiter rendezvous: if the partner is already
/// parked it is a fast handoff, else the caller BLOCKS until the partner arrives.
pub const E_SEND: u16 = 1;
pub const E_RECV: u16 = 2;

/// Method IDs for the Extent object kind (Phase 3 content-addressed store, INC5).
/// `X_READ` (needs INVOKE|READ) returns the extent's metadata — in the register ABI, the
/// content hash (the durable name); the full `{lba,len,hash}` triple is read via
/// [`extent_metadata`]. Bytes themselves reach a domain via Extent **MAP** (INC7), never a
/// register (CAP_ABI §5: the core is not in the bulk data path). `X_WRITE`/`X_COMMIT` (need
/// INVOKE|WRITE) mint a NEW content-addressed extent — their bulk-data path is wired with
/// Extent MAP in INC7, so in INC5 a rights-valid X_WRITE/X_COMMIT returns `ErrMethod` (the
/// rights gate is live; the write path is not yet).
pub const X_READ: u16 = 1;
pub const X_WRITE: u16 = 2;
pub const X_COMMIT: u16 = 3;

/// Method IDs for the DeviceQueue object kind (Phase 3 INC7 zero-kernel I/O — the namesake
/// pillar-4 milestone). `DQ_INFO` (needs INVOKE|READ) echoes the device queue size;
/// `DQ_MAP` (needs INVOKE|**MAP** — the FIRST live use of `Rights::MAP`) maps the virtqueue
/// rings + a NO_CACHE doorbell page contiguously into the calling (trusted driver) domain at
/// [`DQ_BASE`] and returns that base USER VA; `DQ_REPORT` (needs INVOKE|WRITE) is the v0
/// proof channel — a ring-3 driver reports its zero-syscall I/O result and the kernel
/// validates it against the seeded expectation (NOT a kernel-mediated submit — that v1
/// fallback is deferred, see docs/PHASE3.md). Methods are per-kind, so 1/2/3 reuse is fine.
pub const DQ_INFO: u16 = 1;
pub const DQ_MAP: u16 = 2;
pub const DQ_REPORT: u16 = 3;

/// Fixed USER virtual-address base of the DeviceQueue mapping window (INC7). Five contiguous
/// pages from here: +0x0000 desc (RW), +0x1000 avail (RW), +0x2000 used (RO — the device
/// writes it via bus-master DMA, not the CPU PTE), +0x3000 request buffer (RW;
/// hdr@+0/data@+512/status@+1024, params baked at +2048), +0x4000 doorbell (RW, NO_CACHE).
/// Chosen well above the demo's code/stack region [0x40_0000, 0x80_0000). The ring-3 driver
/// blob hardcodes this same base — keep them in sync.
pub const DQ_BASE: u64 = 0x100_0000;

/// The sole "no capability" sentinel for the IPC `xfer` (inbound) and `reply_cptr`
/// (outbound) slots — equals the userspace `syscall::CPTR_NULL` (0xffff). A real CPtr
/// is `0..CAP_TABLE_SLOTS`, so 0 is an ordinary, transferable slot — NOT a second null.
pub const CPTR_NONE: u16 = crate::syscall::CPTR_NULL as u16;

/// Method id used to validate a TimeSlice capability at admission (kernel-side,
/// like M_ALLOC — not part of cap-core).
pub const M_GRANT_TIME: u16 = 1;

/// Number of protection domains (one CapTable each). Domain 0 is the root/kernel
/// domain; the rest are assigned to ring-3 tasks (domain id is independent of task
/// index). Fully used by the current demo: 0=root, 1=client, 2=server, 3=transient
/// GRANT_CAP-negative-test domain, 4=crash-only faulter, 5=trusted virtio driver (INC7).
/// Bump when adding domains.
pub const MAX_DOMAINS: usize = 6;

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

/// Endpoint rendezvous state (one waiter per direction in v0), indexed by `object_id`.
/// Mirrors the NOTIFY pattern: cap-core holds only the cap; the channel state lives here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EpState {
    Idle,
    SendWait, // a sender is parked, message (and maybe a cap) staged
    RecvWait, // a receiver is parked, waiting for a message
}

#[derive(Clone, Copy)]
struct EpSlot {
    state: EpState,
    peer_task: u16,   // the parked partner's task index (waker target)
    peer_domain: u16, // the parked partner's protection domain
    msg0: u64,        // staged message word (sender side)
    xfer_cptr: u16,   // staged cap to transfer (sender side; CPTR_NULL if none)
}
const EP_EMPTY: EpSlot = EpSlot {
    state: EpState::Idle,
    peer_task: 0,
    peer_domain: 0,
    msg0: 0,
    xfer_cptr: 0,
};
/// Per-endpoint rendezvous state. const static-init (no boot-stack temporary).
/// SAFETY contract: single-CPU, accessed only with interrupts off (see module docs).
static mut ENDPOINTS: [EpSlot; OBJECT_TABLE_SIZE] = [EP_EMPTY; OBJECT_TABLE_SIZE];

/// Per-Extent metadata payload, indexed by `object_id` (Phase 3 INC5). cap-core's
/// `ObjectMeta` is frozen, so the extent's on-disk location/length/content-hash live here,
/// mirroring [`NOTIFY`]/[`ENDPOINTS`]: cap-core holds the cap (authority/type/epoch), the
/// kernel holds the object's contents. The `hash` IS the durable content name.
#[derive(Clone, Copy)]
struct ExtentMeta {
    lba: u64,
    len: u32,
    hash: u64,
}
const EXT_EMPTY: ExtentMeta = ExtentMeta { lba: 0, len: 0, hash: 0 };
/// Per-Extent side-table. const static-init (no boot-stack temporary).
/// SAFETY contract: single-CPU, accessed only with interrupts off (see module docs).
static mut EXTENTS: [ExtentMeta; OBJECT_TABLE_SIZE] = [EXT_EMPTY; OBJECT_TABLE_SIZE];

/// Per-DeviceQueue metadata payload, indexed by `object_id` (Phase 3 INC7). cap-core's
/// `ObjectMeta` is frozen, so the virtqueue's RAW guest-physical ring/buffer/doorbell
/// addresses (what `DQ_MAP` maps and what a ring-3 driver writes into descriptor `addr`
/// fields), the negotiated queue size, and the v0 proof-validation state (`magic` expected at
/// `scratch_lba`) all live here — mirroring [`EXTENTS`]/[`NOTIFY`]/[`ENDPOINTS`].
#[derive(Clone, Copy)]
struct DqMeta {
    desc_phys: u64,
    avail_phys: u64,
    used_phys: u64,
    buf_phys: u64,
    doorbell_phys: u64, // absolute MMIO doorbell phys; page = & !0xfff, sub-page offset = & 0xfff
    qsize: u16,
    magic: u64,      // value the kernel seeded at `scratch_lba` (DQ_REPORT validation)
    scratch_lba: u64, // the sector the driver reads for the zero-syscall I/O proof
}
const DQ_EMPTY: DqMeta = DqMeta {
    desc_phys: 0,
    avail_phys: 0,
    used_phys: 0,
    buf_phys: 0,
    doorbell_phys: 0,
    qsize: 0,
    magic: 0,
    scratch_lba: 0,
};
/// Per-DeviceQueue side-table. const static-init (no boot-stack temporary).
/// SAFETY contract: single-CPU, accessed only with interrupts off (see module docs).
static mut DEVQUEUES: [DqMeta; OBJECT_TABLE_SIZE] = [DQ_EMPTY; OBJECT_TABLE_SIZE];

/// Throttle for endpoint proof prints.
static EP_REPORTS: AtomicU64 = AtomicU64::new(0);

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

/// Create an Endpoint object and mint a fully-powered cap in the ROOT domain:
/// rights = INVOKE|READ|WRITE|GRANT_CAP|DELEGATE (send/recv + may grant caps over it +
/// may be delegated). Returns the root CPtr. Channel state lives in `ENDPOINTS[oid]`.
pub fn create_endpoint() -> Option<u16> {
    let mut objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    let oid = objs_g.create_object(ObjectKind::Endpoint)?;
    let rights =
        Rights::INVOKE | Rights::READ | Rights::WRITE | Rights::GRANT_CAP | Rights::DELEGATE;
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

    dispatch_method(kind, oid, rights, method, arg)
}

/// Post-validation dispatch on (object kind, method) for the non-blocking object kinds
/// (Memory, Notification). Endpoints are handled in [`cap_invoke_ipc`] instead because
/// they may block and/or transfer a capability. `rights` is the already-validated cap's
/// rights; INVOKE was enforced by the caller, here we layer any method-specific right.
fn dispatch_method(kind: ObjectKind, oid: u64, rights: Rights, method: u16, arg: u64) -> (Status, u64) {
    let extra = match (kind, method) {
        (ObjectKind::Notification, N_SIGNAL) => Rights::WRITE,
        (ObjectKind::Notification, N_POLL) => Rights::READ,
        (ObjectKind::Extent, X_READ) => Rights::READ,
        (ObjectKind::Extent, X_WRITE) | (ObjectKind::Extent, X_COMMIT) => Rights::WRITE,
        (ObjectKind::DeviceQueue, DQ_INFO) => Rights::READ,
        (ObjectKind::DeviceQueue, DQ_MAP) => Rights::MAP, // FIRST live use of Rights::MAP
        (ObjectKind::DeviceQueue, DQ_REPORT) => Rights::WRITE,
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
        // Extent X_READ: metadata-only — return the content hash (the durable name) in the
        // single reply register. lba/len are read via `extent_metadata`; bytes via Extent
        // MAP (INC7), never a register. READ was enforced by the `extra` gate above.
        (ObjectKind::Extent, X_READ) => {
            let i = oid as usize;
            let h = if i < OBJECT_TABLE_SIZE {
                // SAFETY: single-CPU, IRQs off; `i` bounds-checked; shared read of EXTENTS.
                unsafe { (*core::ptr::addr_of!(EXTENTS))[i].hash }
            } else {
                0
            };
            (Status::Ok, h)
        }
        // DeviceQueue (INC7 zero-kernel I/O): INFO echoes qsize; MAP maps the rings+doorbell
        // into the trusted driver domain (first live Rights::MAP); REPORT validates the driver's
        // zero-syscall I/O result. Rights enforced by the `extra` gate above.
        (ObjectKind::DeviceQueue, DQ_INFO) => (Status::Ok, devqueue_qsize(oid)),
        (ObjectKind::DeviceQueue, DQ_MAP) => devqueue_map(oid),
        (ObjectKind::DeviceQueue, DQ_REPORT) => devqueue_report(oid, arg),
        // Extent X_WRITE/X_COMMIT: WRITE was enforced above, but the content-addressed write
        // path needs the bytes, which arrive via Extent MAP in INC7. So a rights-valid write
        // falls through to ErrMethod here (the rights gate is proven; the write path is not
        // yet wired) — distinct from the ErrRights a cap lacking WRITE gets at the gate.
        _ => (Status::ErrMethod, 0),
    }
}

/// The IPC-capable invoke entry: resolves the cap in the CURRENT domain (verified
/// invoke + lookup), then dispatches. Endpoints (`E_SEND`/`E_RECV`) may block and/or
/// transfer the cap named by `xfer`; everything else routes to [`dispatch_method`]
/// (and a stray `xfer` on a non-endpoint is refused). Returns
/// `(status, reply, reply_cptr)` — `reply_cptr` is a transferred capability's
/// destination slot in the caller's table, or [`CPTR_NONE`] if none. This is what the
/// ring-3 `syscall_dispatch` calls.
pub fn cap_invoke_ipc(cptr: u16, method: u16, arg0: u64, xfer: u16) -> (u64, u64, u16) {
    let domain = current_domain();

    let (kind, oid, rights) = {
        let objs_g = objects().lock();
        // SAFETY: single-CPU, IRQs off; shared read of DOMAINS.
        let doms = unsafe { &*core::ptr::addr_of!(DOMAINS) };
        let tbl = &doms.tables[domain];

        let st = tbl.invoke(&*objs_g, cptr, method, Rights::INVOKE);
        if st != Status::Ok {
            return (st as u64, 0, CPTR_NONE);
        }
        match tbl.lookup(cptr, &*objs_g) {
            Ok(e) => (e.type_tag, e.object_id, e.rights),
            Err(st) => return (st as u64, 0, CPTR_NONE),
        }
        // Cap-table view dropped here, BEFORE any block_current (whose woken partner
        // re-locks these) or object-payload access.
    };

    match (kind, method) {
        (ObjectKind::Endpoint, E_SEND) => ep_send(domain, oid, arg0, xfer, rights),
        (ObjectKind::Endpoint, E_RECV) => ep_recv(domain, oid),
        _ => {
            // Capability transfer is only defined for endpoints; refuse a stray xfer.
            if xfer != CPTR_NONE {
                return (Status::ErrMethod as u64, 0, CPTR_NONE);
            }
            let (st, reply) = dispatch_method(kind, oid, rights, method, arg0);
            (st as u64, reply, CPTR_NONE)
        }
    }
}

/// Endpoint send. If a receiver is parked, hand off the message (+ optional cap) and
/// wake it (fastpath); otherwise park as the sender until a receiver arrives. `rights`
/// is the endpoint cap's rights (GRANT_CAP gates capability transfer). Returns
/// `(status, reply=0, reply_cptr=CPTR_NONE)` — send is one-way in v0.
///
/// Cap transfer is ATOMIC with the rendezvous: if a transfer was requested but the
/// `delegate` fails (no DELEGATE on the moved cap, revoked, bad slot, dst table full),
/// the rendezvous is ABORTED — both parties get the failure status, no cap moves, no
/// message is delivered. A failed grant is never reported as `Ok`.
///
/// SAFETY discipline: `ENDPOINTS[oid]` is touched only via raw pointer, never with a
/// reference held across `block_current`; no cap/object lock is held across a block.
fn ep_send(send_dom: usize, oid: u64, msg0: u64, xfer: u16, rights: Rights) -> (u64, u64, u16) {
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return (Status::ErrBadCPtr as u64, 0, CPTR_NONE);
    }
    let want_xfer = xfer != CPTR_NONE;
    // Validate a transfer request BEFORE any park: GRANT_CAP on the endpoint (the
    // channel-grant gate) AND an in-range source slot. So a bad request never blocks.
    if want_xfer {
        if !rights.contains(Rights::GRANT_CAP) {
            return (Status::ErrRights as u64, 0, CPTR_NONE);
        }
        if xfer as usize >= CAP_TABLE_SLOTS {
            return (Status::ErrBadCPtr as u64, 0, CPTR_NONE);
        }
    }

    // SAFETY: single-CPU, IRQs off; raw read of ENDPOINTS state.
    let state = unsafe { (*core::ptr::addr_of!(ENDPOINTS))[i].state };
    match state {
        EpState::RecvWait => {
            // Fastpath: a receiver is parked. Hand off message + optional cap, wake it.
            let (peer_task, peer_dom) = unsafe {
                let ep = &(*core::ptr::addr_of!(ENDPOINTS))[i];
                (ep.peer_task, ep.peer_domain)
            };
            // SAFETY: single-CPU, IRQs off; reset the channel before waking the partner.
            unsafe {
                (*core::ptr::addr_of_mut!(ENDPOINTS))[i].state = EpState::Idle;
            }
            let xfer_res = if want_xfer {
                do_cap_transfer(send_dom, xfer, peer_dom as usize)
            } else {
                Ok(CPTR_NONE)
            };
            match xfer_res {
                Ok(dst_cptr) => {
                    crate::sched::unblock(peer_task as usize, Status::Ok as u64, msg0, dst_cptr);
                    if EP_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                        crate::serial_println!(
                            "ep: domain{} E_SEND msg={:#x} xfer={} (grant={}) => rendezvous, woke recv task={} recv_cptr={}",
                            send_dom, msg0, xfer, rights.contains(Rights::GRANT_CAP), peer_task, dst_cptr
                        );
                    }
                    (Status::Ok as u64, 0, CPTR_NONE)
                }
                Err(e) => {
                    // Atomic abort: the grant failed, so the rendezvous fails for both.
                    crate::sched::unblock(peer_task as usize, e as u64, 0, CPTR_NONE);
                    if EP_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                        crate::serial_println!(
                            "ep: domain{} E_SEND cap-transfer failed ({:?}); rendezvous aborted, woke recv task={}",
                            send_dom, e, peer_task
                        );
                    }
                    (e as u64, 0, CPTR_NONE)
                }
            }
        }
        EpState::SendWait => {
            // v0 single-waiter: another sender is already parked.
            (Status::ErrRights as u64, 0, CPTR_NONE)
        }
        EpState::Idle => {
            // Slowpath: park as sender, staging the message (+ optional cap to transfer).
            let cur = crate::sched::current_task();
            // SAFETY: single-CPU, IRQs off; publish the SendWait state before yielding.
            unsafe {
                let ep = &mut (*core::ptr::addr_of_mut!(ENDPOINTS))[i];
                ep.state = EpState::SendWait;
                ep.peer_task = cur as u16;
                ep.peer_domain = send_dom as u16;
                ep.msg0 = msg0;
                ep.xfer_cptr = if want_xfer { xfer } else { CPTR_NONE };
            }
            if EP_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                crate::serial_println!(
                    "ep: domain{} E_SEND parked (no receiver) task={} oid={}",
                    send_dom, cur, oid
                );
            }
            crate::sched::block_current(crate::sched::BLOCK_SEND, oid as u16);
            // Resumed by a receiver's fastpath (which staged our result via unblock).
            crate::sched::take_ipc_result(cur)
        }
    }
}

/// Endpoint receive. If a sender is parked, take its message (+ optional cap) and wake
/// it (fastpath); otherwise park as the receiver until a sender arrives. Returns
/// `(status, reply=message, reply_cptr=received-cap-slot or CPTR_NONE)`. A failed cap
/// transfer aborts the rendezvous for both parties (see `ep_send`).
fn ep_recv(recv_dom: usize, oid: u64) -> (u64, u64, u16) {
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return (Status::ErrBadCPtr as u64, 0, CPTR_NONE);
    }

    // SAFETY: single-CPU, IRQs off; raw read of ENDPOINTS state.
    let state = unsafe { (*core::ptr::addr_of!(ENDPOINTS))[i].state };
    match state {
        EpState::SendWait => {
            // Fastpath: a sender is parked. Take its message + optional cap, wake it.
            let (peer_task, peer_dom, msg, xfer) = unsafe {
                let ep = &(*core::ptr::addr_of!(ENDPOINTS))[i];
                (ep.peer_task, ep.peer_domain, ep.msg0, ep.xfer_cptr)
            };
            // SAFETY: single-CPU, IRQs off; reset the channel before waking the partner.
            unsafe {
                (*core::ptr::addr_of_mut!(ENDPOINTS))[i].state = EpState::Idle;
            }
            let xfer_res = if xfer != CPTR_NONE {
                do_cap_transfer(peer_dom as usize, xfer, recv_dom)
            } else {
                Ok(CPTR_NONE)
            };
            match xfer_res {
                Ok(dst_cptr) => {
                    crate::sched::unblock(peer_task as usize, Status::Ok as u64, 0, CPTR_NONE);
                    if EP_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                        crate::serial_println!(
                            "ep: domain{} E_RECV took parked sender msg={:#x} recv_cptr={} => woke send task={}",
                            recv_dom, msg, dst_cptr, peer_task
                        );
                    }
                    (Status::Ok as u64, msg, dst_cptr)
                }
                Err(e) => {
                    // Atomic abort: the sender's grant failed, so both fail.
                    crate::sched::unblock(peer_task as usize, e as u64, 0, CPTR_NONE);
                    if EP_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                        crate::serial_println!(
                            "ep: domain{} E_RECV sender's cap-transfer failed ({:?}); aborted, woke send task={}",
                            recv_dom, e, peer_task
                        );
                    }
                    (e as u64, 0, CPTR_NONE)
                }
            }
        }
        EpState::RecvWait => {
            // v0 single-waiter: another receiver is already parked.
            (Status::ErrRights as u64, 0, CPTR_NONE)
        }
        EpState::Idle => {
            // Slowpath: park as receiver.
            let cur = crate::sched::current_task();
            // SAFETY: single-CPU, IRQs off; publish RecvWait before yielding.
            unsafe {
                let ep = &mut (*core::ptr::addr_of_mut!(ENDPOINTS))[i];
                ep.state = EpState::RecvWait;
                ep.peer_task = cur as u16;
                ep.peer_domain = recv_dom as u16;
            }
            if EP_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                crate::serial_println!(
                    "ep: domain{} E_RECV parked (no sender) task={} oid={}",
                    recv_dom, cur, oid
                );
            }
            crate::sched::block_current(crate::sched::BLOCK_RECV, oid as u16);
            let (st, reply, rcptr) = crate::sched::take_ipc_result(cur);
            if EP_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                crate::serial_println!(
                    "ep: domain{} E_RECV resumed => status={} msg={:#x} recv_cptr={}",
                    recv_dom, st, reply, rcptr
                );
            }
            (st, reply, rcptr)
        }
    }
}

/// MOVE the capability at `src_cptr` in `src_dom`'s table into `dst_dom`'s table:
/// copy via the verified `delegate` (mask = all rights → child = src ⊆ src, requires
/// DELEGATE on the moved cap, re-validates epoch/type) then clear the source slot.
/// Returns `Ok(dst_cptr)` (a valid `0..CAP_TABLE_SLOTS` slot) on success, or `Err(status)`
/// on any failure — and on failure makes NO mutation (the source slot is NOT cleared and
/// no cap lands in the destination), so the MOVE is atomic. cap-core is unchanged.
///
/// SAFETY: single-CPU, IRQs off; called only on an endpoint fastpath (never across a
/// block). Drops the OBJECTS guard before returning.
fn do_cap_transfer(src_dom: usize, src_cptr: u16, dst_dom: usize) -> Result<u16, Status> {
    if src_dom == dst_dom || src_dom >= MAX_DOMAINS || dst_dom >= MAX_DOMAINS {
        return Err(Status::ErrBadCPtr);
    }
    let objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };

    // delegate borrows src immutably and dst mutably; split the array by the larger index.
    let result = if src_dom < dst_dom {
        let (head, tail) = doms.tables.split_at_mut(dst_dom);
        head[src_dom].delegate(&*objs_g, src_cptr, Rights::all(), 0, &mut tail[0])
    } else {
        let (head, tail) = doms.tables.split_at_mut(src_dom);
        // src is tail[0]; dst is head[dst_dom].
        tail[0].delegate(&*objs_g, src_cptr, Rights::all(), 0, &mut head[dst_dom])
    };

    match result {
        Ok(dst_cptr) => {
            // MOVE semantics: clear the source slot only on success (wiring-layer write on
            // the public `slots` field — the same field EMPTY_TABLE const-initializes).
            doms.tables[src_dom].slots[src_cptr as usize] = None;
            Ok(dst_cptr)
        }
        Err(e) => Err(e), // no mutation occurred (delegate is all-or-nothing)
    }
}

/// Reap a terminated domain (crash-only supervision): **revoke its authority** by
/// clearing its CapTable — every CPtr it held vanishes (the underlying objects persist
/// for whoever else holds caps to them, so this revokes only the dead domain, not the
/// shared object) — and **scrub every endpoint** where the dead task was the parked
/// peer, so a surviving partner never rendezvous-wakes a dead task. Called from
/// `sched::terminate_current` with interrupts off on a single CPU.
///
/// NOTE (v0 liveness gap): the scrub is one-directional — it clears slots the DEAD task
/// was parked on. A SURVIVOR already parked *waiting for* the dead task (its slot's
/// `peer_task` is the survivor's own index) is NOT woken and would block until a (now
/// impossible) partner arrives. Closing this needs endpoint peer-tracking / IPC timeouts
/// / domain restart — deferred to 4c (see docs/CRASH_ONLY.md). The current demo's faulter
/// is no domain's endpoint partner, so it is not exercised.
pub fn reap_domain(domain: u16, task_idx: u16) {
    let d = domain as usize;
    // SAFETY: single-CPU, IRQs off; raw access to DOMAINS/ENDPOINTS, no lock held.
    unsafe {
        if d < MAX_DOMAINS {
            (*core::ptr::addr_of_mut!(DOMAINS)).tables[d] = EMPTY_TABLE;
        }
        let eps = &mut *core::ptr::addr_of_mut!(ENDPOINTS);
        for slot in eps.iter_mut() {
            if slot.state != EpState::Idle && slot.peer_task == task_idx {
                *slot = EP_EMPTY;
            }
        }
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

/// Create an Extent object naming a committed content-addressed store extent (Phase 3 INC5)
/// and mint a fully-powered root cap for it in the ROOT domain: rights =
/// INVOKE|READ|WRITE|MAP|DELEGATE. INVOKE is the universal "may call methods" right the
/// verified `CapTable::invoke` requires; READ gates `X_READ`, WRITE gates `X_WRITE`/`X_COMMIT`,
/// MAP is the INC7 zero-kernel Extent-map right (defined now, first used then), DELEGATE lets
/// the cap (or a rights-subset) be handed to another domain. The `{lba,len,hash}` (from
/// [`crate::objstore::put`]) lives in `EXTENTS[oid]` because cap-core's `ObjectMeta` is frozen.
/// Returns the root-domain CPtr.
pub fn mint_extent(lba: u64, len: u32, hash: u64) -> Option<u16> {
    let mut objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    let oid = objs_g.create_object(ObjectKind::Extent)?;
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return None;
    }
    // SAFETY: single-CPU, IRQs off; sole writer of EXTENTS[oid] at mint.
    unsafe {
        (*core::ptr::addr_of_mut!(EXTENTS))[i] = ExtentMeta { lba, len, hash };
    }
    let rights = Rights::INVOKE | Rights::READ | Rights::WRITE | Rights::MAP | Rights::DELEGATE;
    doms.tables[0].mint(&*objs_g, oid, rights, 0).ok()
}

/// Read an Extent cap's full metadata `{lba, len, hash}` after a verified `invoke`
/// (INVOKE) + type==Extent + READ-right check in `domain`. This is the `X_READ`
/// "returns metadata" path with all three fields — the register-ABI [`cap_invoke`] can
/// only return the content hash, so callers needing `lba`/`len` (the proof's read-back,
/// and INC7's Extent MAP) use this. Returns `None` if the cap is absent/revoked, the wrong
/// type, or lacks READ.
pub fn extent_metadata(domain: usize, cptr: u16) -> Option<(u64, u32, u64)> {
    let domain = domain.min(MAX_DOMAINS - 1);
    let oid = {
        let objs_g = objects().lock();
        // SAFETY: single-CPU, IRQs off; shared read of DOMAINS.
        let doms = unsafe { &*core::ptr::addr_of!(DOMAINS) };
        let tbl = &doms.tables[domain];
        if tbl.invoke(&*objs_g, cptr, X_READ, Rights::INVOKE) != Status::Ok {
            return None;
        }
        match tbl.lookup(cptr, &*objs_g) {
            Ok(e) if e.type_tag == ObjectKind::Extent && e.rights.contains(Rights::READ) => {
                e.object_id
            }
            _ => return None,
        }
    };
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return None;
    }
    // SAFETY: single-CPU, IRQs off; shared read of EXTENTS.
    let m = unsafe { (*core::ptr::addr_of!(EXTENTS))[i] };
    Some((m.lba, m.len, m.hash))
}

/// Create a DeviceQueue object naming virtio-blk queue 0 (Phase 3 INC7) and mint a root cap:
/// rights = INVOKE|READ|WRITE|MAP|DELEGATE. The queue's RAW guest-phys ring/buffer/doorbell
/// addresses + negotiated size come from `virtio_blk::device_queue_desc`; `magic`/`scratch_lba`
/// are the kernel-seeded proof expectation the `DQ_REPORT` handler validates. Returns the root
/// CPtr, or `None` if the device isn't ready or the object table is full.
pub fn create_device_queue(magic: u64, scratch_lba: u64) -> Option<u16> {
    let dqd = crate::virtio_blk::device_queue_desc()?;
    let mut objs_g = objects().lock();
    // SAFETY: single-CPU, IRQs off; sole accessor of DOMAINS in this region.
    let doms = unsafe { &mut *core::ptr::addr_of_mut!(DOMAINS) };
    let oid = objs_g.create_object(ObjectKind::DeviceQueue)?;
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return None;
    }
    // SAFETY: single-CPU, IRQs off; sole writer of DEVQUEUES[oid] at mint.
    unsafe {
        (*core::ptr::addr_of_mut!(DEVQUEUES))[i] = DqMeta {
            desc_phys: dqd.desc_phys,
            avail_phys: dqd.avail_phys,
            used_phys: dqd.used_phys,
            buf_phys: dqd.buf_phys,
            doorbell_phys: dqd.doorbell_phys,
            qsize: dqd.qsize,
            magic,
            scratch_lba,
        };
    }
    let rights = Rights::INVOKE | Rights::READ | Rights::WRITE | Rights::MAP | Rights::DELEGATE;
    doms.tables[0].mint(&*objs_g, oid, rights, 0).ok()
}

/// Read a DeviceQueue's negotiated queue size (the `DQ_INFO` reply).
fn devqueue_qsize(oid: u64) -> u64 {
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return 0;
    }
    // SAFETY: single-CPU, IRQs off; `i` bounds-checked; shared read of DEVQUEUES.
    unsafe { (*core::ptr::addr_of!(DEVQUEUES))[i].qsize as u64 }
}

/// `DQ_MAP` work arm (FIRST live use of `Rights::MAP`, gated upstream): map the virtqueue rings
/// + request buffer + NO_CACHE doorbell page contiguously at [`DQ_BASE`] into the single shared
/// address space, USER-accessible, and return the base VA. desc/avail/buf are RW; `used` is RO
/// (the device writes it via bus-master DMA to `used_phys`, independent of the CPU PTE); the
/// doorbell is RW + NO_CACHE. All five pages are NX (pure data).
///
/// SECURITY (no IOMMU + one shared address space): a writable mapped ring frame is
/// write-anywhere DMA AND reachable by every ring-3 task — this is granted ONLY to a trusted
/// driver domain, enforced by capability distribution, not the MMU (see docs/PHASE3.md).
fn devqueue_map(oid: u64) -> (Status, u64) {
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return (Status::ErrBadCPtr, 0);
    }
    // SAFETY: single-CPU, IRQs off; `i` bounds-checked; shared read of DEVQUEUES.
    let m = unsafe { (*core::ptr::addr_of!(DEVQUEUES))[i] };
    if m.buf_phys == 0 {
        return (Status::ErrMethod, 0); // not a seeded device queue
    }
    let hhdm = crate::memory::hhdm_offset();
    let ok = crate::paging::map_user_phys(hhdm, DQ_BASE, m.desc_phys, true, false, false)
        && crate::paging::map_user_phys(hhdm, DQ_BASE + 0x1000, m.avail_phys, true, false, false)
        && crate::paging::map_user_phys(hhdm, DQ_BASE + 0x2000, m.used_phys, false, false, false)
        && crate::paging::map_user_phys(hhdm, DQ_BASE + 0x3000, m.buf_phys, true, false, false)
        && crate::paging::map_user_phys(hhdm, DQ_BASE + 0x4000, m.doorbell_phys, true, false, true);
    if ok {
        (Status::Ok, DQ_BASE)
    } else {
        (Status::ErrMethod, 0)
    }
}

/// `DQ_REPORT` work arm (v0 proof channel): a ring-3 driver reports the first 8 bytes it read
/// from `scratch_lba` via its ZERO-syscall virtqueue I/O. The kernel compares against the magic
/// it seeded there and prints the headline T1 proof line. `reported` is the driver's `arg0`.
fn devqueue_report(oid: u64, reported: u64) -> (Status, u64) {
    let i = oid as usize;
    if i >= OBJECT_TABLE_SIZE {
        return (Status::ErrBadCPtr, 0);
    }
    // SAFETY: single-CPU, IRQs off; `i` bounds-checked; shared read of DEVQUEUES.
    let m = unsafe { (*core::ptr::addr_of!(DEVQUEUES))[i] };
    let matched = reported == m.magic;
    crate::serial_println!(
        "devqueue: ring3 driver completed virtio READ of LBA {} with ZERO syscalls; reported magic={:#x} kernel-seeded={:#x} match={}",
        m.scratch_lba, reported, m.magic, matched
    );
    (Status::Ok, matched as u64)
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
