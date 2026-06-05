//! Unit tests (no Kani) that mirror the key properties proved in verification.rs.
//! These run under `cargo test -p frame-alloc`.

use crate::BitmapFrameAllocator;

#[test]
fn basic_alloc_free_roundtrip() {
    let mut a: BitmapFrameAllocator<32> = BitmapFrameAllocator::new();
    assert_eq!(a.free_count(), 0);
    assert!(!a.is_free(0));

    a.mark_free(5);
    a.mark_free(0);
    assert!(a.is_free(0) && a.is_free(5));
    assert_eq!(a.free_count(), 2);

    // lowest-first
    let f0 = a.alloc().unwrap();
    assert_eq!(f0, 0);
    assert!(!a.is_free(0));
    assert_eq!(a.free_count(), 1);

    let f1 = a.alloc().unwrap();
    assert_eq!(f1, 5);
    assert!(a.alloc().is_none());

    a.free(0);
    assert!(a.is_free(0));
    assert_eq!(a.free_count(), 1);

    let f2 = a.alloc().unwrap();
    assert_eq!(f2, 0);
}

#[test]
fn out_of_bounds_marks_ignored() {
    let mut a: BitmapFrameAllocator<8> = BitmapFrameAllocator::new();
    a.mark_free(100);
    a.mark_free(3);
    a.mark_used(200);
    assert_eq!(a.free_count(), 1);
    assert!(!a.is_free(100));
    assert!(!a.is_free(200));

    let f = a.alloc().unwrap();
    assert_eq!(f, 3);
    assert!(a.alloc().is_none());
}

#[test]
fn idempotent_mark_and_no_double_alloc() {
    let mut a: BitmapFrameAllocator<16> = BitmapFrameAllocator::new();
    a.mark_free(2);
    a.mark_free(2);
    assert_eq!(a.free_count(), 1);

    let f = a.alloc().unwrap();
    assert_eq!(f, 2);
    // second alloc must not succeed (would be double-alloc of 2)
    assert!(a.alloc().is_none());
}

#[test]
fn alloc_never_exceeds_bounds() {
    let mut a: BitmapFrameAllocator<4> = BitmapFrameAllocator::new();
    a.mark_free(0);
    a.mark_free(1);
    a.mark_free(2);
    a.mark_free(3);
    for _ in 0..4 {
        let f = a.alloc().unwrap();
        assert!(f < 4, "frame {} out of 0..4", f);
    }
    assert!(a.alloc().is_none());
}

#[test]
fn free_count_tracks_exact() {
    let mut a: BitmapFrameAllocator<64> = BitmapFrameAllocator::new();
    for i in (0..64).step_by(3) {
        a.mark_free(i);
    }
    // 22 frames (0,3,...,63)
    assert_eq!(a.free_count(), 22);
    let _ = a.alloc();
    let _ = a.alloc();
    assert_eq!(a.free_count(), 20);
}
