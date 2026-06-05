//! Capability entry format and rights (exact match to docs/CAP_ABI.md v0).
//!
//! 128-bit layout:
//!   [0,48)   object_id  (u64, only 48 bits used)
//!   [48,64)  rights     (u16 bitflags)
//!   [64,96)  epoch      (u32)
//!   [96,112) type_tag   (u16)
//!   [112,128) badge     (u16)

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Rights: u16 {
        const READ      = 1 << 0;
        const WRITE     = 1 << 1;
        const INVOKE    = 1 << 2;
        const DELEGATE  = 1 << 3;
        const REVOKE    = 1 << 4;
        const MAP       = 1 << 5;
        const GRANT_CAP = 1 << 6;
        const SEAL      = 1 << 7;
        // bits 8..16 reserved for object-kind-specific rights
    }
}

/// Object kind tag (16-bit space, concrete kinds listed in CAP_ABI.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ObjectKind(u16);

impl ObjectKind {
    pub const Null: Self = Self(0);
    pub const Untyped: Self = Self(1);
    pub const Memory: Self = Self(2);
    pub const AddressSpace: Self = Self(3);
    pub const Domain: Self = Self(4);
    pub const Endpoint: Self = Self(5);
    pub const Notification: Self = Self(6);
    pub const TimeSlice: Self = Self(7);
    pub const Extent: Self = Self(8);
    pub const DeviceQueue: Self = Self(9);
    pub const CapTable: Self = Self(10);
    pub const IrqHandler: Self = Self(11);

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn from_raw(v: u16) -> Self {
        Self(v)
    }
}

/// A capability table entry (kernel-side slot value). 128 bits total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CapEntry {
    pub object_id: u64,
    pub rights: Rights,
    pub epoch: u32,
    pub type_tag: ObjectKind,
    pub badge: u16,
}

impl CapEntry {
    /// Encode to the exact 128-bit wire layout specified in CAP_ABI.md.
    #[inline]
    pub fn encode(&self) -> u128 {
        let oid = (self.object_id & 0x0000_FFFF_FFFF_FFFF) as u128;
        let r = (self.rights.bits() as u128) << 48;
        let ep = (self.epoch as u128) << 64;
        let tt = (self.type_tag.raw() as u128) << 96;
        let bd = (self.badge as u128) << 112;
        oid | r | ep | tt | bd
    }

    /// Decode from the 128-bit layout. Round-trip with encode() is lossless
    /// for any values in the representable ranges (object_id masked to 48 bits).
    #[inline]
    pub fn decode(val: u128) -> Self {
        let object_id = val & 0x0000_FFFF_FFFF_FFFF;
        let rights = Rights::from_bits_truncate(((val >> 48) & 0xFFFF) as u16);
        let epoch = ((val >> 64) & 0xFFFF_FFFF) as u32;
        let type_tag = ObjectKind::from_raw(((val >> 96) & 0xFFFF) as u16);
        let badge = ((val >> 112) & 0xFFFF) as u16;
        CapEntry {
            object_id: object_id as u64,
            rights,
            epoch,
            type_tag,
            badge,
        }
    }
}
