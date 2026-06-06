//! Minimal crash-only **supervisor**: restart policy for terminated domains (v0).
//!
//! DESIGN.md pillar 8 ("self-healing"): when a domain is terminated by a fault
//! (`sched::terminate_current` → `on_domain_death`), the supervisor may re-admit a fresh
//! instance — fresh CapTable + re-granted caps, reusing the freed task slot — under a
//! per-domain **restart budget** so a crash-loop is eventually reaped instead of spinning
//! forever. Clients are expected to hold their own checkpoint caps and tolerate a restart.
//!
//! v0 keeps a tiny fixed registry (one restartable domain: the demo faulter). A real
//! supervisor would hold a registry of domain specs + richer policy (backoff, escalation).
//! Called only from the fault path with interrupts off on a single CPU.

use cap_core::capability::Rights;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// The one restartable demo domain (the faulter). Kept in sync with `main.rs`.
pub const FAULT_DOMAIN: u16 = 4;

/// Restartable spec for the faulter: its (already-mapped) entry/stack VAs and a restart
/// budget. The user pages persist across termination, so a restart need not re-map them.
static FAULTER_ENTRY: AtomicU64 = AtomicU64::new(0);
static FAULTER_STACK_TOP: AtomicU64 = AtomicU64::new(0);
static FAULTER_RESTARTS: AtomicU8 = AtomicU8::new(0);

/// Register the demo faulter as restartable with `budget` restarts. Call at boot once its
/// pages are mapped and it has been admitted the first time.
pub fn register_faulter(entry: u64, stack_top: u64, budget: u8) {
    FAULTER_ENTRY.store(entry, Ordering::Relaxed);
    FAULTER_STACK_TOP.store(stack_top, Ordering::Relaxed);
    FAULTER_RESTARTS.store(budget, Ordering::Relaxed);
}

/// Crash-only restart hook, called by `sched::terminate_current` AFTER the dead task's
/// slot is freed and its domain reaped (CapTable cleared, endpoints scrubbed), and BEFORE
/// the scheduler picks the next task. If the domain is restartable and has budget left,
/// re-grant its caps and re-admit a fresh instance (its user pages are still mapped); the
/// new task becomes runnable immediately. Otherwise it stays reaped.
///
/// SAFETY-equivalent: runs with IRQs off on a single CPU (the fault handler context); the
/// caller holds no live `&mut SCHED` across this call, so `admit_user`'s transient borrow
/// is sound.
pub fn on_domain_death(domain: u16) {
    if domain != FAULT_DOMAIN {
        return;
    }
    let left = FAULTER_RESTARTS.load(Ordering::Relaxed);
    if left == 0 {
        crate::serial_println!(
            "supervisor: domain {} reaped — restart budget exhausted (crash-only: stays dead)",
            domain
        );
        return;
    }
    FAULTER_RESTARTS.store(left - 1, Ordering::Relaxed);

    let entry = FAULTER_ENTRY.load(Ordering::Relaxed);
    let stack_top = FAULTER_STACK_TOP.load(Ordering::Relaxed);

    // Fresh Memory cap delegated into the (just-reaped, now-empty) domain table, plus a
    // TimeSlice cap to gate admission ("time is a capability").
    let mem = crate::capspace::init_root()
        .and_then(|m| crate::capspace::delegate_from_root(FAULT_DOMAIN as usize, m, Rights::INVOKE, 0));
    let ts = crate::capspace::mint_timeslice();

    match (mem, ts) {
        (Some(fm), Some(ft)) => {
            match crate::sched::admit_user(
                entry, stack_top, fm, ft, FAULT_DOMAIN, 5_000_000, 5_000_000, 1_000_000,
            ) {
                Some(i) => crate::serial_println!(
                    "supervisor: RESTARTED domain {} as task {} (mem_cptr={}, {} restart(s) left) — self-healing",
                    domain, i, fm, left - 1
                ),
                None => crate::serial_println!(
                    "supervisor: domain {} restart failed (no free task slot)",
                    domain
                ),
            }
        }
        _ => crate::serial_println!("supervisor: domain {} restart failed (cap setup)", domain),
    }
}
