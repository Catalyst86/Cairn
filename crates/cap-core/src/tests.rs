//! Ordinary unit tests (no Kani required) that mirror the key properties
//! proved in verification.rs (I2, I3, I4, round-trip). These run under
//! `cargo test -p cap-core`.

use crate::capability::{CapEntry, ObjectKind, Rights};
use crate::table::{CapTable, ObjectTable, Status};

#[test]
fn encode_decode_roundtrip_lossless() {
    let cases: &[(u64, u16, u32, u16, u16)] = &[
        (0, 0, 0, 0, 0),
        (0x0000_FFFF_FFFF_FFFF, 0x00FF, 0xFFFF_FFFF, 11, 0xABCD),
        (1, Rights::INVOKE.bits(), 42, ObjectKind::Endpoint.raw(), 7),
        (123456789, (Rights::READ | Rights::DELEGATE | Rights::SEAL).bits(), 1, ObjectKind::Memory.raw(), 0xFFFF),
    ];

    for &(oid, rbits, ep, tag, bd) in cases {
        let cap = CapEntry {
            object_id: oid,
            rights: Rights::from_bits_truncate(rbits),
            epoch: ep,
            type_tag: ObjectKind::from_raw(tag),
            badge: bd,
        };
        let enc = cap.encode();
        let dec = CapEntry::decode(enc);
        assert_eq!(cap, dec, "roundtrip failed for {:?}", cap);
        assert_eq!(enc, dec.encode());
    }
}

#[test]
fn i2_revocation_completeness() {
    let mut ot = ObjectTable::new();
    let oid = ot.create_object(ObjectKind::Notification).expect("create");

    let mut ct = CapTable::new();
    let cptr = ct
        .mint(&ot, oid, Rights::INVOKE | Rights::READ, 0x42)
        .expect("mint");

    // Pre-revoke invoke works (if we ask for rights the cap has).
    assert_eq!(ct.invoke(&ot, cptr, 1, Rights::INVOKE), Status::Ok);

    // Revoke the object (epoch bump).
    assert_eq!(ot.revoke(oid), Status::Ok);

    // Now the old cap must be revoked for any invoke attempt.
    assert_eq!(
        ct.invoke(&ot, cptr, 1, Rights::INVOKE),
        Status::ErrRevoked
    );
    assert_eq!(
        ct.invoke(&ot, cptr, 99, Rights::READ | Rights::INVOKE),
        Status::ErrRevoked
    );

    // A freshly minted cap after revoke works again (new epoch).
    let cptr2 = ct
        .mint(&ot, oid, Rights::INVOKE, 0x99)
        .expect("mint after revoke");
    assert_eq!(ct.invoke(&ot, cptr2, 1, Rights::INVOKE), Status::Ok);
}

#[test]
fn i3_no_rights_amplification() {
    // Pure bitwise property (covers every possible mask/parent pair).
    for p in 0u16..=0xFFFF {
        for m in 0u16..=0xFFFF {
            let parent = Rights::from_bits_truncate(p);
            let mask = Rights::from_bits_truncate(m);
            let child = parent & mask;
            assert_eq!(
                child.bits() & parent.bits(),
                child.bits(),
                "amplification: parent={:04x} mask={:04x} child={:04x}",
                p,
                m,
                child.bits()
            );
        }
    }

    // End-to-end via delegate (when preconditions allow).
    let mut ot = ObjectTable::new();
    let oid = ot.create_object(ObjectKind::Domain).expect("obj");

    let mut src = CapTable::new();
    let mut dst = CapTable::new();

    let parent = Rights::READ | Rights::WRITE | Rights::DELEGATE | Rights::MAP;
    let src_cptr = src.mint(&ot, oid, parent, 1).expect("mint src");

    // mask tries to add INVOKE (which parent does not have) — must not appear in child.
    let mask = Rights::READ | Rights::INVOKE;
    let dst_cptr = src
        .delegate(&ot, src_cptr, mask, 0xBEEF, &mut dst)
        .expect("delegate");

    let child = dst.lookup(dst_cptr, &ot).expect("child live");
    assert!(!child.rights.contains(Rights::INVOKE));
    assert!(child.rights.contains(Rights::READ));
    assert!(!child.rights.contains(Rights::WRITE)); // not in mask
    assert_eq!(child.badge, 0xBEEF);
    // Subset check
    assert!((child.rights.bits() & parent.bits()) == child.rights.bits());
}

#[test]
fn i4_fresh_mint_invoke_iff_invoke_right() {
    let mut ot = ObjectTable::new();
    let oid = ot.create_object(ObjectKind::Endpoint).expect("obj");

    let mut ct = CapTable::new();

    let with_invoke = Rights::INVOKE | Rights::GRANT_CAP;
    let without = Rights::READ | Rights::WRITE | Rights::DELEGATE;

    let c_ok = ct.mint(&ot, oid, with_invoke, 10).expect("mint ok");
    let c_bad = ct.mint(&ot, oid, without, 11).expect("mint bad");

    assert_eq!(ct.invoke(&ot, c_ok, 0, Rights::INVOKE), Status::Ok);
    assert_eq!(ct.invoke(&ot, c_bad, 0, Rights::INVOKE), Status::ErrRights);

    // Even if cap has INVOKE, asking for extra rights it doesn't hold fails.
    assert_eq!(
        ct.invoke(&ot, c_ok, 0, Rights::INVOKE | Rights::SEAL),
        Status::ErrRights
    );
}

#[test]
fn seal_clears_delegate_monotonically() {
    let mut ot = ObjectTable::new();
    let oid = ot.create_object(ObjectKind::CapTable).expect("obj");

    let mut ct = CapTable::new();
    let cptr = ct
        .mint(&ot, oid, Rights::DELEGATE | Rights::INVOKE, 0)
        .expect("mint");

    assert!(ct.lookup(cptr, &ot).unwrap().rights.contains(Rights::DELEGATE));

    assert_eq!(ct.seal(cptr), Status::Ok);
    let after = ct.lookup(cptr, &ot).unwrap();
    assert!(!after.rights.contains(Rights::DELEGATE));
    assert!(after.rights.contains(Rights::INVOKE)); // other rights preserved

    // Idempotent.
    assert_eq!(ct.seal(cptr), Status::Ok);
}

#[test]
fn bad_cptr_and_type_errors() {
    let mut ot = ObjectTable::new();
    let oid = ot.create_object(ObjectKind::IrqHandler).expect("obj");

    let mut ct = CapTable::new();
    let cptr = ct.mint(&ot, oid, Rights::INVOKE, 0).expect("mint");

    assert_eq!(ct.invoke(&ot, 0xFFFF, 0, Rights::INVOKE), Status::ErrBadCPtr);
    assert_eq!(ct.lookup(0xFFFF, &ot), Err(Status::ErrBadCPtr));

    // Manually corrupt the slot's type (simulating bad kernel state — should never happen).
    if let Some(e) = &mut ct.slots[cptr as usize] {
        e.type_tag = ObjectKind::from_raw(0xFFFF);
    }
    assert_eq!(ct.invoke(&ot, cptr, 0, Rights::INVOKE), Status::ErrType);
}
