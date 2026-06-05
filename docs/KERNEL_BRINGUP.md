# keystone Bring-up Sequence

What the kernel does from power-on to idle, and what is formally verified at each
layer. Status as of the Phase 1 build-ahead (written before first compile — see
the "compile-pending" notes).

## Boot order (`kernel/src/main.rs::kmain`)

1. **Limine handoff** → `kmain` (higher-half, `0xffffffff80000000`). Base revision
   asserted supported. Requests: base revision, memory map, HHDM.
2. **Serial** (`serial::init`) — 16550 COM1, 38400 8N1. All early output.
   Banner: `Cairn keystone v0 — the core is alive`.
3. **GDT** (`gdt::init`) — kernel code/data segments + TSS with a dedicated
   **IST0 stack for #DF** (so a stack overflow in a handler still has a good stack).
4. **IDT** (`interrupts::init_idt`) — CPU **exceptions only** (no PIC/APIC yet):
   `#BP` (recoverable), `#DF` (IST, fatal), `#PF` (reports CR2 + error code),
   `#GP`, `#UD`.
5. **Physical frame allocator** (`memory::init`) — walks Limine `Usable` entries
   into a sorted, coalescing range list; hands out the lowest free 4 KiB frame.
6. **Kernel heap** (`memory::init_heap`) — a 1 MiB **static** region given to
   `linked_list_allocator` as the `#[global_allocator]`. `Box`/`Vec` work with no
   paging yet (a frame-/page-backed heap comes later).
7. **Self-tests** — fire `int3` and confirm "recovered from #BP"; allocate a
   `Vec<u64>`; print the free-frame count. Then `hlt` forever.

## What is formally verified (Kani)

| Component | Crate | Proven invariants |
|-----------|-------|-------------------|
| Capability table | `crates/cap-core` | I2 revocation completeness · I3 no rights amplification · I4 invoke-requires-right · capability encode/decode round-trip |
| Physical frame allocator (model) | `crates/frame-alloc` | no double-allocation · distinct back-to-back allocs · free/alloc round-trip · returned index always `< FRAMES` |

The kernel's runtime range-list allocator (`memory.rs`) mirrors the *lowest-first*
strategy proved in `frame-alloc`; a later revision can hand the verified type a
bounded sub-range or move to a runtime-sized bitmap so the running allocator is
backed directly by a proof.

## Not yet present (next milestones)

- Hardware interrupts (APIC/x2APIC timer) → preemptive scheduling (Phase 2).
- The real `cap_invoke` syscall path (`syscall`/`sysret`, ring-3) wiring
  `cap-core` into a per-domain CapSpace (Phase 1→2 boundary).
- Paging-backed/demand heap; SMP AP bring-up; NUMA-aware allocation (Phase 4).

## Compile-pending notes (to resolve on first `cargo build`)

- Exact API surfaces of `limine 0.3`, `x86_64 0.15`, `linked_list_allocator 0.10`.
- `#[unsafe(link_section=…)]` / `#[unsafe(no_mangle)]` are accepted on the pinned
  nightly (required in edition 2024, allowed in 2021); revert to bare forms if the
  toolchain is older.
- `kani::assert!` may need to be plain `assert!` inside `#[kani::proof]` harnesses.
- Clippy `-D warnings`: `ObjectKind` consts (non-UPPER_CASE) and `new()` without
  `Default` in `cap-core` will need `#[allow(...)]` or small fixes.
