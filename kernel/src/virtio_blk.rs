//! Minimal polled virtio-blk driver (Phase 3, increment 2 — L1 of the I/O stack).
//!
//! Modern virtio-1.0 over PCI: the device's config structures live in MMIO BAR windows
//! pointed to by vendor-specific PCI capabilities (cap id 0x09). We walk those caps, map
//! the windows (`paging::map_mmio_range`), negotiate `VIRTIO_F_VERSION_1`, stand up ONE
//! split virtqueue, and do a POLLED single-sector read. No interrupts (runs pre-`sti`).
//!
//! DMA DISCIPLINE (the #1 bring-up footgun): every address the DEVICE sees — the
//! queue_desc/driver/device registers and each descriptor `addr` — is a RAW guest-physical
//! address (`frame * 4096`). The KERNEL touches the same memory via the HHDM alias
//! (`phys + hhdm`). Mixing the two yields a silent no-completion. The used-ring poll has a
//! bounded anti-hang guard so a mistake prints a timeout instead of wedging the 20s boot.
//!
//! No IOMMU here: this kernel-side driver is trusted (it programs every descriptor). The
//! zero-kernel DeviceQueue grant to a ring-3 driver domain is increment 7 (see PHASE3.md).

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, Ordering};

// --- virtio PCI capability types (cap.cfg_type) ---
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
const PCI_CAP_ID_VNDR: u8 = 0x09;

// --- common_cfg field offsets (modern virtio 1.0) ---
const CC_DEVICE_FEATURE_SELECT: u64 = 0x00;
const CC_DEVICE_FEATURE: u64 = 0x04;
const CC_DRIVER_FEATURE_SELECT: u64 = 0x08;
const CC_DRIVER_FEATURE: u64 = 0x0C;
const CC_DEVICE_STATUS: u64 = 0x14;
const CC_QUEUE_SELECT: u64 = 0x16;
const CC_QUEUE_SIZE: u64 = 0x18;
const CC_QUEUE_ENABLE: u64 = 0x1C;
const CC_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const CC_QUEUE_DESC: u64 = 0x20;
const CC_QUEUE_DRIVER: u64 = 0x28;
const CC_QUEUE_DEVICE: u64 = 0x30;

// --- device status bits ---
const S_ACK: u8 = 1;
const S_DRIVER: u8 = 2;
const S_DRIVER_OK: u8 = 4;
const S_FEATURES_OK: u8 = 8;

// VIRTIO_F_VERSION_1 = feature bit 32 = bit 0 of the high (select=1) feature dword.
const VIRTIO_F_VERSION_1_HI: u32 = 1 << 0;
// VIRTIO_BLK_F_FLUSH = feature bit 9 (low dword) — a flushable volatile write cache.
const VIRTIO_BLK_F_FLUSH_BIT: u32 = 1 << 9;

// --- virtq descriptor flags ---
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// --- virtio-blk request types ---
const VIRTIO_BLK_T_IN: u32 = 0; // read (device writes the data buffer)
const VIRTIO_BLK_T_OUT: u32 = 1; // write (device reads the data buffer)
const VIRTIO_BLK_T_FLUSH: u32 = 4; // flush the device's volatile write cache (no data)
const SECTOR: u64 = 512;

/// Clamp the queue to keep desc/avail/used each within a single 4 KiB frame
/// (desc = QSIZE*16, used = 6+QSIZE*8 — both fit for QSIZE ≤ 256; 128 is comfortable).
const QSIZE: u16 = 128;

/// Driver state (single device, single queue). Single-CPU, IRQs-off discipline.
struct VirtioBlk {
    present: bool,
    common: u64,      // virt of common_cfg
    notify_base: u64, // virt of the notify BAR window
    notify_mul: u32,  // notify_off_multiplier
    notify_off: u16,  // queue_notify_off for queue 0
    desc: u64,        // virt of descriptor table
    avail: u64,       // virt of avail ring
    used: u64,        // virt of used ring
    hdr_phys: u64,
    data_phys: u64,
    status_phys: u64,
    hdr: u64,    // virt of the 16-byte request header
    data: u64,   // virt of the 512-byte data buffer
    status: u64, // virt of the 1-byte status
    qsize: u16,  // device-negotiated queue size (NOT the QSIZE clamp) — used for ring indexing
    flush_ok: bool, // VIRTIO_BLK_F_FLUSH negotiated (device has a flushable cache)
}

static mut VBLK: VirtioBlk = VirtioBlk {
    present: false,
    common: 0,
    notify_base: 0,
    notify_mul: 0,
    notify_off: 0,
    desc: 0,
    avail: 0,
    used: 0,
    hdr_phys: 0,
    data_phys: 0,
    status_phys: 0,
    hdr: 0,
    data: 0,
    status: 0,
    qsize: 0,
    flush_ok: false,
};

// --- volatile MMIO/RAM accessors ---
#[inline]
unsafe fn r8(va: u64) -> u8 {
    read_volatile(va as *const u8)
}
#[inline]
unsafe fn w8(va: u64, v: u8) {
    write_volatile(va as *mut u8, v)
}
#[inline]
unsafe fn r16(va: u64) -> u16 {
    read_volatile(va as *const u16)
}
#[inline]
unsafe fn w16(va: u64, v: u16) {
    write_volatile(va as *mut u16, v)
}
#[inline]
unsafe fn r32(va: u64) -> u32 {
    read_volatile(va as *const u32)
}
#[inline]
unsafe fn w32(va: u64, v: u32) {
    write_volatile(va as *mut u32, v)
}
#[inline]
unsafe fn w64(va: u64, v: u64) {
    write_volatile(va as *mut u64, v)
}

/// Allocate one zeroed frame; return (phys, virt). None on OOM.
fn alloc_frame(hhdm: u64) -> Option<(u64, u64)> {
    let fnum = crate::memory::allocate_frame()?;
    let phys = fnum * 4096;
    let virt = hhdm + phys;
    // SAFETY: fresh frame, accessed via its HHDM alias.
    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, 4096) };
    Some((phys, virt))
}

/// Discover + initialize the virtio-blk device and its queue 0. Returns true on success.
pub fn init(hhdm: u64) -> bool {
    let (bus, dev, func) = match crate::pci::find_virtio_blk() {
        Some(loc) => loc,
        None => {
            crate::serial_println!("virtio-blk: no device found");
            return false;
        }
    };
    crate::pci::enable_bus_master(bus, dev, func);

    // Walk the PCI capability list for the virtio config windows.
    let mut common = 0u64;
    let mut notify_base = 0u64;
    let mut notify_mul = 0u32;
    let mut device_cfg = 0u64;
    // `cap` is a u16 config-space offset so `cap + 16` never overflows (256-byte space;
    // a u8 sum would panic in debug for a cap placed high, or wrap+misread in release).
    let mut cap = (crate::pci::cfg_read8(bus, dev, func, 0x34) & 0xFC) as u16;
    let mut guard = 0;
    while cap != 0 && guard < 48 {
        guard += 1;
        let vndr = crate::pci::cfg_read8(bus, dev, func, cap);
        let next = crate::pci::cfg_read8(bus, dev, func, cap + 1);
        if vndr == PCI_CAP_ID_VNDR {
            let cfg_type = crate::pci::cfg_read8(bus, dev, func, cap + 3);
            let bar = crate::pci::cfg_read8(bus, dev, func, cap + 4);
            let offset = crate::pci::cfg_read32(bus, dev, func, cap + 8);
            let length = crate::pci::cfg_read32(bus, dev, func, cap + 12);
            let phys = crate::pci::bar_base(bus, dev, func, bar) + offset as u64;
            let va = match crate::paging::map_mmio_range(hhdm, phys, length as u64) {
                Some(v) => v,
                None => {
                    crate::serial_println!("virtio-blk: map_mmio_range failed");
                    return false;
                }
            };
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG => common = va,
                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                    notify_base = va;
                    notify_mul = crate::pci::cfg_read32(bus, dev, func, cap + 16);
                }
                VIRTIO_PCI_CAP_DEVICE_CFG => device_cfg = va,
                _ => {}
            }
        }
        cap = (next & 0xFC) as u16;
    }
    let _ = device_cfg; // not needed for the read smoke test
    if common == 0 || notify_base == 0 {
        crate::serial_println!("virtio-blk: missing common/notify cfg cap");
        return false;
    }

    // SAFETY: single-CPU, IRQs off; sole accessor of the device + VBLK during init.
    unsafe {
        // Reset, then ACKNOWLEDGE + DRIVER. The reset-wait is BOUNDED (a wedged device
        // or a mis-mapped common_cfg must not hang the boot with IRQs off).
        w8(common + CC_DEVICE_STATUS, 0);
        let mut rg: u64 = 0;
        while r8(common + CC_DEVICE_STATUS) != 0 {
            rg += 1;
            if rg > 50_000_000 {
                crate::serial_println!("virtio-blk: reset timeout");
                return false;
            }
            core::hint::spin_loop();
        }
        w8(common + CC_DEVICE_STATUS, S_ACK);
        w8(common + CC_DEVICE_STATUS, S_ACK | S_DRIVER);

        // Feature negotiation: read offered features (low + high dwords), require
        // VIRTIO_F_VERSION_1 (mandatory for modern), and accept VIRTIO_BLK_F_FLUSH if
        // offered (durable commits for the object store).
        w32(common + CC_DEVICE_FEATURE_SELECT, 0);
        let dev_lo = r32(common + CC_DEVICE_FEATURE);
        w32(common + CC_DEVICE_FEATURE_SELECT, 1);
        let dev_hi = r32(common + CC_DEVICE_FEATURE);
        if dev_hi & VIRTIO_F_VERSION_1_HI == 0 {
            crate::serial_println!("virtio-blk: device is not virtio-1.0");
            return false;
        }
        let drv_lo = dev_lo & VIRTIO_BLK_F_FLUSH_BIT;
        let flush_ok = drv_lo != 0;
        w32(common + CC_DRIVER_FEATURE_SELECT, 0);
        w32(common + CC_DRIVER_FEATURE, drv_lo);
        w32(common + CC_DRIVER_FEATURE_SELECT, 1);
        w32(common + CC_DRIVER_FEATURE, VIRTIO_F_VERSION_1_HI);

        // FEATURES_OK and confirm the device kept it.
        w8(common + CC_DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK);
        if r8(common + CC_DEVICE_STATUS) & S_FEATURES_OK == 0 {
            crate::serial_println!("virtio-blk: FEATURES_OK rejected");
            return false;
        }

        // Queue 0 setup.
        w16(common + CC_QUEUE_SELECT, 0);
        let max = r16(common + CC_QUEUE_SIZE);
        if max == 0 {
            crate::serial_println!("virtio-blk: queue 0 unavailable");
            return false;
        }
        let qsize = if max < QSIZE { max } else { QSIZE };
        w16(common + CC_QUEUE_SIZE, qsize);

        // Ring frames (desc/avail/used each in their own zeroed frame) + a request frame.
        let (desc_phys, desc) = match alloc_frame(hhdm) {
            Some(x) => x,
            None => return false,
        };
        let (avail_phys, avail) = match alloc_frame(hhdm) {
            Some(x) => x,
            None => return false,
        };
        let (used_phys, used) = match alloc_frame(hhdm) {
            Some(x) => x,
            None => return false,
        };
        let (buf_phys, buf) = match alloc_frame(hhdm) {
            Some(x) => x,
            None => return false,
        };

        // Program the queue with RAW guest-physical addresses, then enable it.
        w64(common + CC_QUEUE_DESC, desc_phys);
        w64(common + CC_QUEUE_DRIVER, avail_phys);
        w64(common + CC_QUEUE_DEVICE, used_phys);
        let notify_off = r16(common + CC_QUEUE_NOTIFY_OFF);
        w16(common + CC_QUEUE_ENABLE, 1);

        let v = &mut *core::ptr::addr_of_mut!(VBLK);
        v.common = common;
        v.notify_base = notify_base;
        v.notify_mul = notify_mul;
        v.notify_off = notify_off;
        v.desc = desc;
        v.avail = avail;
        v.used = used;
        v.hdr_phys = buf_phys;
        v.data_phys = buf_phys + 512;
        v.status_phys = buf_phys + 1024;
        v.hdr = buf;
        v.data = buf + 512;
        v.status = buf + 1024;
        v.qsize = qsize;
        v.flush_ok = flush_ok;
        v.present = true;

        // DRIVER_OK — device is live.
        w8(common + CC_DEVICE_STATUS, S_ACK | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);
    }

    crate::serial_println!(
        "virtio-blk: ready (00:{:02x}.{} queue0 size={})",
        dev,
        func,
        unsafe { r16(common + CC_QUEUE_SIZE) }
    );
    true
}

/// Submit one block request (header + the shared 512B data buffer + status), notify the
/// device, and poll for completion. `req_type` is VIRTIO_BLK_T_IN/OUT; for a READ the data
/// descriptor is device-written (WRITE flag), for a WRITE the device reads it. Returns true
/// iff the device reported status OK. The caller fills `v.data` before an OUT and copies it
/// out after an IN.
///
/// SAFETY: single-CPU, IRQs off; sole accessor of the queue. Device-facing addresses are
/// raw guest-physical; ring/buffer memory is touched via its HHDM alias.
unsafe fn submit(req_type: u32, lba: u64, has_data: bool) -> bool {
    let v = &mut *core::ptr::addr_of_mut!(VBLK);
    if !v.present {
        return false;
    }
    let data_dev_writes = req_type == VIRTIO_BLK_T_IN;

    // Request header: type, reserved=0, sector=lba.
    w32(v.hdr, req_type);
    w32(v.hdr + 4, 0);
    w64(v.hdr + 8, lba);
    w8(v.status, 0xFF); // device overwrites with 0 = OK

    // Descriptor chain: hdr (device reads) -> [data] -> status (device writes). FLUSH has
    // no data buffer, so it chains hdr -> status directly.
    let d = v.desc;
    w64(d, v.hdr_phys);
    w32(d + 8, 16);
    w16(d + 12, VIRTQ_DESC_F_NEXT);
    w16(d + 14, 1);
    if has_data {
        // desc[1]: data. WRITE flag = "device writes it" — set only for a READ.
        let data_flags = VIRTQ_DESC_F_NEXT | if data_dev_writes { VIRTQ_DESC_F_WRITE } else { 0 };
        w64(d + 16, v.data_phys);
        w32(d + 16 + 8, SECTOR as u32);
        w16(d + 16 + 12, data_flags);
        w16(d + 16 + 14, 2);
        // desc[2]: status.
        w64(d + 32, v.status_phys);
        w32(d + 32 + 8, 1);
        w16(d + 32 + 12, VIRTQ_DESC_F_WRITE);
        w16(d + 32 + 14, 0);
    } else {
        // desc[1]: status (no data descriptor).
        w64(d + 16, v.status_phys);
        w32(d + 16 + 8, 1);
        w16(d + 16 + 12, VIRTQ_DESC_F_WRITE);
        w16(d + 16 + 14, 0);
    }

    // Avail ring: publish head descriptor 0 at the current idx, then bump idx. Slot uses the
    // DEVICE-negotiated queue size (v.qsize), not the QSIZE clamp.
    let avail_idx = r16(v.avail + 2);
    let slot = avail_idx % v.qsize;
    w16(v.avail + 4 + (slot as u64) * 2, 0);
    compiler_fence(Ordering::SeqCst);
    // used.idx must reach this exact count when OUR (sole in-flight) request completes — a
    // precise target, so a late completion after a timeout can't be mis-consumed.
    let target = avail_idx.wrapping_add(1);
    w16(v.avail + 2, target);
    compiler_fence(Ordering::SeqCst);

    // Notify (doorbell): notify_base + notify_off * notify_mul.
    let notify_addr = v.notify_base + (v.notify_off as u64) * (v.notify_mul as u64);
    w16(notify_addr, 0);

    // Poll for completion, bounded. On timeout the request is still in flight, so DISABLE
    // the device — a later read must not break instantly on the stale completion.
    let mut guard: u64 = 0;
    loop {
        compiler_fence(Ordering::SeqCst);
        if r16(v.used + 2) == target {
            break;
        }
        guard += 1;
        if guard > 50_000_000 {
            v.present = false;
            crate::serial_println!("virtio-blk: LBA {} TIMED OUT — device disabled", lba);
            return false;
        }
        core::hint::spin_loop();
    }

    let st = r8(v.status);
    if st != 0 {
        crate::serial_println!("virtio-blk: LBA {} status={:#x} (not OK)", lba, st);
        return false;
    }
    true
}

/// Read one 512-byte sector at `lba` into `dst`. Polled. Returns true on success.
pub fn read_sector(lba: u64, dst: &mut [u8; 512]) -> bool {
    // SAFETY: single-CPU, IRQs off; sole queue accessor.
    unsafe {
        if !submit(VIRTIO_BLK_T_IN, lba, true) {
            return false;
        }
        let v = &*core::ptr::addr_of!(VBLK);
        core::ptr::copy_nonoverlapping(v.data as *const u8, dst.as_mut_ptr(), 512);
        true
    }
}

/// Write one 512-byte sector `src` to `lba`. Polled. Returns true on success.
pub fn write_sector(lba: u64, src: &[u8; 512]) -> bool {
    // SAFETY: single-CPU, IRQs off; sole queue accessor.
    unsafe {
        let v = &mut *core::ptr::addr_of_mut!(VBLK);
        if !v.present {
            return false;
        }
        // Stage the data into the device-facing buffer, then submit an OUT.
        core::ptr::copy_nonoverlapping(src.as_ptr(), v.data as *mut u8, 512);
        submit(VIRTIO_BLK_T_OUT, lba, true)
    }
}

/// Flush the device's volatile write cache to durable storage. Required before a commit
/// is considered durable (serialized completion is ordering, not durability, under QEMU's
/// writeback cache). No-op (returns true) if the device advertised no flushable cache.
pub fn flush() -> bool {
    // SAFETY: single-CPU, IRQs off; sole queue accessor.
    unsafe {
        let v = &*core::ptr::addr_of!(VBLK);
        if !v.present {
            return false;
        }
        if !v.flush_ok {
            return true;
        }
        submit(VIRTIO_BLK_T_FLUSH, 0, false)
    }
}

/// Increment-2 smoke test: init the device and read LBA 0, printing the pre-seeded magic.
pub fn smoke_test(hhdm: u64) {
    if !init(hhdm) {
        return;
    }
    // LBA 0/1 are claimed by the Cairnlog superblock (objstore), so the L2 read+write
    // proof uses LBA 8 — a round-trip exercises both the read and write paths.
    let mut wbuf = [0u8; 512];
    for (i, b) in wbuf.iter_mut().enumerate() {
        *b = (i as u8) ^ 0xA5;
    }
    let mut rbuf = [0u8; 512];
    let rt = write_sector(8, &wbuf) && read_sector(8, &mut rbuf) && wbuf == rbuf;
    // SAFETY: scalar read of the driver flag, single-CPU IRQs off.
    let flush_ok = unsafe { (*core::ptr::addr_of!(VBLK)).flush_ok };
    crate::serial_println!(
        "virtio-blk: wrote+read LBA8 512B match={} (flush negotiated={})",
        rt, flush_ok
    );
}
