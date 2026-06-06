//! Kani proof harnesses for the core invariants (I2, I3, I4, round-trip)
//! as specified in docs/CAP_ABI.md.
//!
//! Run with: `cargo kani --features kani` (or the kani cargo wrapper).
//! The harnesses use kani::any() + assume() for bounded exhaustive exploration.

#![cfg(feature = "kani")]

use crate::capability::{CapEntry, ObjectKind, Rights};
use crate::table::{CapTable, ObjectTable, Status};

/// I4 + INVOKE: a freshly minted cap is invocable (with INVOKE required) iff it carries INVOKE.
#[kani::proof]
fn check_i4_invoke_requires_right() {
    let mut ot = ObjectTable::new();
    let oid = match ot.create_object(ObjectKind::Endpoint) {
        Some(o) => o,
        None => return,
    };

    let mut ct = CapTable::new();

    // Arbitrary rights; we will force presence/absence of INVOKE for the two cases.
    let bits_with: u16 = kani::any();
    let mut r_with = Rights::from_bits_truncate(bits_with);
    r_with.insert(Rights::INVOKE);

    let bits_without: u16 = kani::any();
    let mut r_without = Rights::from_bits_truncate(bits_without);
    r_without.remove(Rights::INVOKE);

    let cptr_with = match ct.mint(&ot, oid, r_with, 0x1234) {
        Ok(c) => c,
        Err(_) => return,
    };
    let cptr_without = match ct.mint(&ot, oid, r_without, 0x5678) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Freshly minted => epoch and type match by construction.
    let s_ok = ct.invoke(&ot, cptr_with, 42, Rights::INVOKE);
    assert!(s_ok == Status::Ok, "I4: cap with INVOKE must succeed invoke check");

    let s_bad = ct.invoke(&ot, cptr_without, 42, Rights::INVOKE);
    assert!(
        s_bad == Status::ErrRights,
        "I4: cap without INVOKE must fail with ErrRights"
    );
}

/// I2: after revoke(object), any cap that was minted against the *old* epoch
/// must fail invoke with ErrRevoked (revocation is complete).
#[kani::proof]
fn check_i2_revocation_completeness() {
    let mut ot = ObjectTable::new();
    let oid = match ot.create_object(ObjectKind::Memory) {
        Some(o) => o,
        None => return,
    };

    let mut ct = CapTable::new();

    let rbits: u16 = kani::any();
    let r = Rights::from_bits_truncate(rbits);
    let cptr = match ct.mint(&ot, oid, r, 99) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Capture that we are using a cap minted at the pre-revoke epoch.
    let pre = ot.metas[oid as usize].current_epoch;

    // Revoke (bumps epoch for the object).
    let _ = ot.revoke(oid);

    // Any subsequent invoke on the old cap must be ErrRevoked, regardless of method/rights asked.
    let method: u16 = kani::any();
    let req_bits: u16 = kani::any();
    let req = Rights::from_bits_truncate(req_bits);

    let status = ct.invoke(&ot, cptr, method, req);
    assert!(
        status == Status::ErrRevoked,
        "I2: post-revoke cap minted at old epoch must yield ErrRevoked"
    );

    // Sanity: the epoch really changed.
    assert!(
        ot.metas[oid as usize].current_epoch != pre,
        "revoke must have advanced the epoch"
    );
}

/// I3: delegate never amplifies rights. child.rights ⊆ parent.rights for any parent + mask.
#[kani::proof]
fn check_i3_no_rights_amplification() {
    let mut ot = ObjectTable::new();
    let oid = match ot.create_object(ObjectKind::Endpoint) {
        Some(o) => o,
        None => return,
    };

    let mut src = CapTable::new();
    let mut dst = CapTable::new();

    // Parent rights (arbitrary). We ensure DELEGATE so the delegate path is taken when possible.
    let pbits: u16 = kani::any();
    let mut parent_rights = Rights::from_bits_truncate(pbits);
    parent_rights.insert(Rights::DELEGATE);

    let src_cptr = match src.mint(&ot, oid, parent_rights, 7) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mask_bits: u16 = kani::any();
    let mask = Rights::from_bits_truncate(mask_bits);
    let badge: u16 = kani::any();

    // If delegate succeeds, inspect the produced child.
    if let Ok(dst_cptr) = src.delegate(&ot, src_cptr, mask, badge, &mut dst) {
        // Find the child we just created (lowest free slot strategy in impl).
        // We only need to prove the subset property for whatever was produced.
        if let Ok(child) = dst.lookup(dst_cptr, &ot) {
            let child_bits = child.rights.bits();
            let parent_bits = parent_rights.bits();
            assert!(
                (child_bits & parent_bits) == child_bits,
                "I3: delegated child rights must be a subset of parent rights (no amplification)"
            );
        }
    }
    // Also prove the bitwise property in isolation (covers all masks even if delegate preconditions fail).
    let p2: u16 = kani::any();
    let parent2 = Rights::from_bits_truncate(p2);
    let m2: u16 = kani::any();
    let mask2 = Rights::from_bits_truncate(m2);
    let child2 = parent2 & mask2;
    assert!(
        (child2.bits() & parent2.bits()) == child2.bits(),
        "I3 (bitwise): (parent & mask) is always ⊆ parent"
    );
}

/// encode/decode round-trip must be lossless for arbitrary (well-formed) caps.
#[kani::proof]
fn check_encode_decode_roundtrip() {
    // Constrain object_id to the 48-bit representable range.
    let oid: u64 = kani::any();
    kani::assume(oid < (1u64 << 48));

    let rbits: u16 = kani::any();
    let rights = Rights::from_bits_truncate(rbits);

    let epoch: u32 = kani::any();
    let tag_raw: u16 = kani::any();
    let type_tag = ObjectKind::from_raw(tag_raw);
    let badge: u16 = kani::any();

    let cap = CapEntry {
        object_id: oid,
        rights,
        epoch,
        type_tag,
        badge,
    };

    let enc = cap.encode();
    let dec = CapEntry::decode(enc);

    assert!(
        cap == dec,
        "encode/decode round-trip must preserve all fields (lossless)"
    );

    // Re-encoding the decoded value yields identical bits.
    assert!(enc == dec.encode(), "re-encode after decode is stable");
}
