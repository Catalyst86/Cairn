//! Minimal PCI bus enumeration (Phase 3, increment 1 — the L0 of the I/O stack).
//!
//! Legacy `0xCF8`/`0xCFC` config-space access — sufficient on the QEMU q35 root bus
//! (bus 0), where the single virtio-blk device lives. (ECAM/MMCONFIG via ACPI MCFG is a
//! Phase-4 generalization; multi-bus / behind-bridge scanning is deferred too.)
//!
//! Run ONCE at boot with interrupts OFF: the BAR-size probe writes all-ones to a BAR,
//! reads back the size mask, and restores the original — that write/read/restore must not
//! be interleaved with any other config access (single-CPU, pre-`sti` satisfies this).
//!
//! This is pure hardware discovery — cap-core is not involved. It locates the virtio-blk
//! (vendor 0x1AF4, class 0x01 mass-storage) and decodes its BARs (the MMIO config window
//! the virtio-blk driver will map in increment 2).

use x86_64::instructions::port::Port;

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// virtio PCI vendor id; PCI mass-storage class code.
const VIRTIO_VENDOR: u16 = 0x1AF4;
const CLASS_STORAGE: u8 = 0x01;

/// Build the CF8 config address for (bus, dev, func, dword-aligned `off`).
#[inline]
fn cfg_addr(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((off as u32) & 0xFC)
}

/// Read a 32-bit dword from PCI config space.
///
/// SAFETY: standard PCI legacy config mechanism (CF8 address latch + CFC data port).
/// Called single-CPU with interrupts off, so the address/data pair is never interleaved.
fn cfg_read32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    unsafe {
        let mut a: Port<u32> = Port::new(CONFIG_ADDR);
        let mut d: Port<u32> = Port::new(CONFIG_DATA);
        a.write(cfg_addr(bus, dev, func, off));
        d.read()
    }
}

/// Write a 32-bit dword to PCI config space (used to size BARs).
/// SAFETY: as [`cfg_read32`]; single-CPU, IRQs off.
fn cfg_write32(bus: u8, dev: u8, func: u8, off: u8, val: u32) {
    unsafe {
        let mut a: Port<u32> = Port::new(CONFIG_ADDR);
        let mut d: Port<u32> = Port::new(CONFIG_DATA);
        a.write(cfg_addr(bus, dev, func, off));
        d.write(val);
    }
}

/// Decoded BAR. `size == 0` means the BAR is unimplemented (skip it).
struct Bar {
    base: u64,
    size: u64,
    is_io: bool,
    is_64: bool,
}

/// Decode + size BAR `i` (0..6) of a header-type-0 device via the standard
/// write-all-ones / read-back / restore probe. Must run with IRQs off (atomic probe).
/// A 64-bit memory BAR consumes BARs `i` and `i+1`; the caller advances by 2.
fn bar_info(bus: u8, dev: u8, func: u8, i: u8) -> Bar {
    let off = 0x10 + i * 4;
    let raw = cfg_read32(bus, dev, func, off);
    if raw == 0 {
        return Bar { base: 0, size: 0, is_io: false, is_64: false };
    }

    if raw & 0x1 != 0 {
        // I/O BAR: bit0=1, address in bits[31:2].
        cfg_write32(bus, dev, func, off, 0xFFFF_FFFF);
        let readback = cfg_read32(bus, dev, func, off);
        cfg_write32(bus, dev, func, off, raw); // restore immediately
        let size = (!(readback & 0xFFFF_FFFC)).wrapping_add(1);
        return Bar {
            base: (raw & 0xFFFF_FFFC) as u64,
            size: size as u64,
            is_io: true,
            is_64: false,
        };
    }

    // Memory BAR: bits[2:1] type (0=32-bit, 2=64-bit), bit3 prefetch, address in [31:4].
    let is_64 = ((raw >> 1) & 0x3) == 0x2;
    let mut base = (raw & 0xFFFF_FFF0) as u64;
    cfg_write32(bus, dev, func, off, 0xFFFF_FFFF);
    let lo = cfg_read32(bus, dev, func, off);
    cfg_write32(bus, dev, func, off, raw); // restore immediately

    let size = if is_64 {
        // 64-bit BAR: the size mask spans this dword + BAR i+1.
        let off_hi = off + 4;
        let raw_hi = cfg_read32(bus, dev, func, off_hi);
        base |= (raw_hi as u64) << 32;
        cfg_write32(bus, dev, func, off_hi, 0xFFFF_FFFF);
        let hi = cfg_read32(bus, dev, func, off_hi);
        cfg_write32(bus, dev, func, off_hi, raw_hi);
        let size_mask = ((lo & 0xFFFF_FFF0) as u64) | ((hi as u64) << 32);
        (!size_mask).wrapping_add(1)
    } else {
        // 32-bit BAR: compute in u32 — zero-extending to u64 before the bitwise-NOT
        // would wrongly set the upper 32 bits (the bug this replaces).
        (!(lo & 0xFFFF_FFF0)).wrapping_add(1) as u64
    };
    Bar { base, size, is_io: false, is_64 }
}

/// Scan PCI bus 0: print every present function (vendor/device/class/header), each
/// device's non-empty BARs, and flag the virtio-blk. Run once at boot with IRQs off.
pub fn scan() {
    crate::serial_println!("pci: scanning bus 0 (legacy 0xCF8/0xCFC)");
    for dev in 0u8..32 {
        for func in 0u8..8 {
            let id = cfg_read32(0, dev, func, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor == 0xFFFF {
                if func == 0 {
                    break; // function 0 absent => empty slot; skip remaining functions
                }
                continue;
            }
            let device = (id >> 16) as u16;
            let cls = cfg_read32(0, dev, func, 0x08);
            let class = ((cls >> 24) & 0xFF) as u8;
            let sub = ((cls >> 16) & 0xFF) as u8;
            let hdr = ((cfg_read32(0, dev, func, 0x0C) >> 16) & 0xFF) as u8;

            crate::serial_println!(
                "pci 00:{:02x}.{} vendor={:#06x} device={:#06x} class={:02x}:{:02x} hdr={:#04x}",
                dev, func, vendor, device, class, sub, hdr
            );

            // BARs only exist in header type 0 (general device).
            if hdr & 0x7F == 0 {
                let mut i = 0u8;
                while i < 6 {
                    let b = bar_info(0, dev, func, i);
                    if b.size != 0 {
                        crate::serial_println!(
                            "pci   bar{} {}{} base={:#x} size={:#x}",
                            i,
                            if b.is_io { "io" } else { "mmio" },
                            if b.is_64 { "64" } else { "32" },
                            b.base,
                            b.size
                        );
                    }
                    i += if b.is_64 { 2 } else { 1 };
                }
            }

            if vendor == VIRTIO_VENDOR && class == CLASS_STORAGE {
                crate::serial_println!(
                    "pci:   ^ virtio-blk (device={:#06x}: 0x1042=modern, 0x1001=transitional)",
                    device
                );
            }

            // Probe functions 1..8 only if function 0 declares itself multifunction.
            if func == 0 && (hdr & 0x80) == 0 {
                break;
            }
        }
    }
    crate::serial_println!("pci: scan complete");
}
