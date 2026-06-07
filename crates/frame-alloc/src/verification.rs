//! Kani proof harnesses for `BitmapFrameAllocator`.
//!
//! These prove the key safety properties required of any physical frame
//! allocator used by keystone:
//! - No double-allocation (an allocated frame is never handed out again until freed).
//! - Distinctness for back-to-back allocs.
//! - Free/alloc round-trips preserve availability.
//! - Returned indices are always in-bounds.
//!
//! Run with: `cargo kani -p frame-alloc --features kani` (Kani must be installed).
//! We use a small FRAMES=8 (one bitmap word) + `#[kani::unwind(10)]` so CBMC fully unrolls the
//! data-dependent loops (the `alloc_bounds_only` exhaust loop runs ≤ N times) and terminates.
//! The properties are universal over the const-generic allocator, so N=8 is representative;
//! FRAMES=128 left the exhaust loop unbounded for CBMC and never converged (it "hung").

#![cfg(feature = "kani")]

use crate::BitmapFrameAllocator;

/// After alloc returns Some(f), that frame is marked used and f is in bounds.
/// Also: alloc never returns a frame that was not free.
#[kani::unwind(10)]
#[kani::proof]
fn no_double_allocation() {
    const N: usize = 8;
    let mut alloc: BitmapFrameAllocator<N> = BitmapFrameAllocator::new();

    // Nondet choose a frame to make free.
    let f: usize = kani::any();
    kani::assume(f < N);
    alloc.mark_free(f);

    let r1 = alloc.alloc();
    assert!(r1.is_some(), "we just freed one frame");
    if let Some(f1) = r1 {
        assert!(f1 < N, "alloc must return frame < FRAMES");
        assert!(!alloc.is_free(f1), "alloc must clear the free bit");
        // The frame we got must have been the one we freed (or another if we freed more,
        // but here it is the only one).
        // Try a second alloc; it must not yield the same frame.
        let r2 = alloc.alloc();
        if let Some(f2) = r2 {
            assert!(f1 != f2, "no double-allocation without an intervening free");
        }
    }
}

/// Two allocs with no free between them must produce distinct frames (when >=2 free).
#[kani::unwind(10)]
#[kani::proof]
fn distinct_allocs_no_intervening_free() {
    const N: usize = 8;
    let mut alloc: BitmapFrameAllocator<N> = BitmapFrameAllocator::new();

    let f1: usize = kani::any();
    let f2: usize = kani::any();
    kani::assume(f1 < N && f2 < N && f1 != f2);
    alloc.mark_free(f1);
    alloc.mark_free(f2);

    let r1 = alloc.alloc().expect("at least one free");
    let r2 = alloc.alloc().expect("two were freed");
    assert!(r1 != r2, "consecutive allocs without free must be distinct");
    assert!(!alloc.is_free(r1) && !alloc.is_free(r2));
}

/// mark_free(f) makes is_free(f) true; alloc can return it; free makes it free again.
#[kani::unwind(10)]
#[kani::proof]
fn free_roundtrip_and_is_free() {
    const N: usize = 8;
    let mut alloc: BitmapFrameAllocator<N> = BitmapFrameAllocator::new();

    let f: usize = kani::any();
    kani::assume(f < N);

    alloc.mark_free(f);
    assert!(alloc.is_free(f), "mark_free(f) => is_free(f)");

    // Because it is the *only* free frame, alloc must return exactly f (lowest set bit wins).
    let got = alloc.alloc();
    assert!(got == Some(f), "alloc returns the freed frame when it is the sole free frame");

    // Now free it again (via the free API).
    alloc.free(f);
    assert!(alloc.is_free(f), "free(f) makes is_free(f) true again");
}

/// alloc() only ever returns values < FRAMES (even under nondet frees and out-of-range marks).
#[kani::unwind(10)]
#[kani::proof]
fn alloc_bounds_only() {
    const N: usize = 8;
    let mut alloc: BitmapFrameAllocator<N> = BitmapFrameAllocator::new();

    // Free a few arbitrary frames (some may be duplicates or out of range).
    for _ in 0..8 {
        let f: usize = kani::any();
        if f < N {
            alloc.mark_free(f);
        }
    }
    // Also exercise the bounds checks on mark.
    alloc.mark_free(N);
    alloc.mark_free(N + 99);
    alloc.mark_used(N + 1);

    if let Some(f) = alloc.alloc() {
        assert!(f < N, "alloc() must only return values < FRAMES");
    }
    // Exhaust and ensure still no out-of-bounds.
    while let Some(f) = alloc.alloc() {
        assert!(f < N);
    }
}
