//! Fixed-size ObjectTable + CapTable implementing the v0 capability model
//! from docs/CAP_ABI.md (epoch revocation, mint/delegate/seal/invoke/lookup).
//!
//! Small bounded sizes for Kani and no-heap operation.

use crate::capability::{CapEntry, ObjectKind, Rights};

/// Small fixed capacities (per CAP_ABI.md guidance for verification).
pub const CAP_TABLE_SLOTS: usize = 256;
pub const OBJECT_TABLE_SIZE: usize = 1024;

/// Per-object metadata in the global object table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    pub kind: ObjectKind,
    pub current_epoch: u32,
    pub present: bool,
}

/// Global object table. Objects are identified by small indices (0..OBJECT_TABLE_SIZE)
/// even though the CapEntry stores a 48-bit object_id space.
pub struct ObjectTable {
    pub metas: [ObjectMeta; OBJECT_TABLE_SIZE],
}

impl ObjectTable {
    pub fn new() -> Self {
        Self {
            metas: [ObjectMeta {
                kind: ObjectKind::Null,
                current_epoch: 0,
                present: false,
            }; OBJECT_TABLE_SIZE],
        }
    }

    /// Allocate a fresh object id of the given kind (simple linear scan).
    /// Returns None if the table is full.
    pub fn create_object(&mut self, kind: ObjectKind) -> Option<u64> {
        for (i, meta) in self.metas.iter_mut().enumerate() {
            if !meta.present {
                meta.kind = kind;
                meta.current_epoch = 0;
                meta.present = true;
                return Some(i as u64);
            }
        }
        None
    }

    /// Epoch bump revocation (O(1) for all outstanding caps to this object).
    /// Returns ErrBadCPtr if the id is out of range or not present.
    pub fn revoke(&mut self, object_id: u64) -> Status {
        let idx = object_id as usize;
        if idx >= OBJECT_TABLE_SIZE || !self.metas[idx].present {
            return Status::ErrBadCPtr;
        }
        self.metas[idx].current_epoch = self.metas[idx].current_epoch.wrapping_add(1);
        Status::Ok
    }

    /// Mark an object as absent (for model cleanup in tests/harnesses).
    pub fn destroy_object(&mut self, object_id: u64) {
        let idx = object_id as usize;
        if idx < OBJECT_TABLE_SIZE {
            self.metas[idx].present = false;
            self.metas[idx].kind = ObjectKind::Null;
        }
    }
}

/// Status codes returned by table operations (subset matching CAP_ABI v0 model).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    ErrBadCPtr,
    ErrRevoked,
    ErrType,
    ErrRights,
    ErrMethod,
}

/// Per-domain (or model) capability table: CPtrs are just indices into this array.
pub struct CapTable {
    pub slots: [Option<CapEntry>; CAP_TABLE_SLOTS],
}

impl CapTable {
    pub fn new() -> Self {
        Self {
            slots: [None; CAP_TABLE_SLOTS],
        }
    }

    /// Insert a capability for an existing object, capturing its current epoch.
    /// The rights passed here are what the new cap will hold (caller is responsible
    /// for subset checks from any parent untyped grant, outside this model).
    pub fn mint(
        &mut self,
        ot: &ObjectTable,
        object_id: u64,
        rights: Rights,
        badge: u16,
    ) -> Result<u16, Status> {
        let slot = self
            .slots
            .iter()
            .position(|s| s.is_none())
            .ok_or(Status::ErrBadCPtr)?;

        let idx = object_id as usize;
        if idx >= OBJECT_TABLE_SIZE || !ot.metas[idx].present {
            return Err(Status::ErrBadCPtr);
        }
        let meta = &ot.metas[idx];

        let entry = CapEntry {
            object_id,
            rights,
            epoch: meta.current_epoch,
            type_tag: meta.kind,
            badge,
        };
        self.slots[slot] = Some(entry);
        Ok(slot as u16)
    }

    /// Validate and "invoke" (the core check performed by keystone on cap_invoke).
    /// required_rights must be present on the cap (e.g. INVOKE for methods).
    pub fn invoke(
        &self,
        ot: &ObjectTable,
        cptr: u16,
        _method: u16,
        required_rights: Rights,
    ) -> Status {
        let idx = cptr as usize;
        if idx >= CAP_TABLE_SLOTS {
            return Status::ErrBadCPtr;
        }
        let entry = match self.slots[idx] {
            Some(e) => e,
            None => return Status::ErrBadCPtr,
        };

        let oid = entry.object_id as usize;
        if oid >= OBJECT_TABLE_SIZE {
            return Status::ErrBadCPtr;
        }
        let meta = &ot.metas[oid];

        // I1/I2/I4 core validity: type match + live epoch
        if !meta.present || entry.type_tag != meta.kind || entry.epoch != meta.current_epoch {
            if entry.epoch != meta.current_epoch {
                return Status::ErrRevoked;
            }
            return Status::ErrType;
        }

        if !entry.rights.contains(required_rights) {
            return Status::ErrRights;
        }

        // Method dispatch would happen here in the real kernel; model returns Ok.
        Status::Ok
    }

    /// delegate: copy a subset of rights to dst_table.
    /// Requires DELEGATE on src. Child rights = src.rights & mask (never amplifies).
    /// Returns the dst CPtr on success.
    pub fn delegate(
        &self,
        ot: &ObjectTable,
        src_cptr: u16,
        mask: Rights,
        badge: u16,
        dst_table: &mut CapTable,
    ) -> Result<u16, Status> {
        let src_idx = src_cptr as usize;
        if src_idx >= CAP_TABLE_SLOTS {
            return Err(Status::ErrBadCPtr);
        }
        let entry = match self.slots[src_idx] {
            Some(e) => e,
            None => return Err(Status::ErrBadCPtr),
        };

        if !entry.rights.contains(Rights::DELEGATE) {
            return Err(Status::ErrRights);
        }

        // Re-validate liveness at delegation time (epoch/type)
        let oid = entry.object_id as usize;
        if oid >= OBJECT_TABLE_SIZE {
            return Err(Status::ErrBadCPtr);
        }
        let meta = &ot.metas[oid];
        if !meta.present || entry.type_tag != meta.kind || entry.epoch != meta.current_epoch {
            if entry.epoch != meta.current_epoch {
                return Err(Status::ErrRevoked);
            }
            return Err(Status::ErrType);
        }

        let child_rights = entry.rights & mask; // I3: cannot amplify

        let child = CapEntry {
            object_id: entry.object_id,
            rights: child_rights,
            epoch: entry.epoch,
            type_tag: entry.type_tag,
            badge,
        };

        let dst_slot = dst_table
            .slots
            .iter()
            .position(|s| s.is_none())
            .ok_or(Status::ErrBadCPtr)?;

        dst_table.slots[dst_slot] = Some(child);
        Ok(dst_slot as u16)
    }

    /// seal: monotonically clears DELEGATE (and only DELEGATE) on the cap.
    pub fn seal(&mut self, cptr: u16) -> Status {
        let idx = cptr as usize;
        if idx >= CAP_TABLE_SLOTS {
            return Status::ErrBadCPtr;
        }
        if let Some(entry) = &mut self.slots[idx] {
            entry.rights.remove(Rights::DELEGATE);
            Status::Ok
        } else {
            Status::ErrBadCPtr
        }
    }

    /// Pure lookup + validation (returns the entry if still valid).
    pub fn lookup(&self, cptr: u16, ot: &ObjectTable) -> Result<CapEntry, Status> {
        let idx = cptr as usize;
        if idx >= CAP_TABLE_SLOTS {
            return Err(Status::ErrBadCPtr);
        }
        let entry = match self.slots[idx] {
            Some(e) => e,
            None => return Err(Status::ErrBadCPtr),
        };

        let oid = entry.object_id as usize;
        if oid >= OBJECT_TABLE_SIZE {
            return Err(Status::ErrBadCPtr);
        }
        let meta = &ot.metas[oid];

        if !meta.present || entry.type_tag != meta.kind || entry.epoch != meta.current_epoch {
            if entry.epoch != meta.current_epoch {
                return Err(Status::ErrRevoked);
            }
            return Err(Status::ErrType);
        }

        Ok(entry)
    }
}
