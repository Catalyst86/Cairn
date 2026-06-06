# Cairn — Session Resume / Handoff

**Read this first, then `DESIGN.md`, `docs/CAP_ABI.md`, and the `cairn-os` memory.**
Cairn is a from-scratch, capability-based OS for James's HPE ProLiant x86-64 server,
built as a **Claude Code × Grok Build** collaboration. Repo: `C:\Users\danie\Desktop\Cairn`.

## Status at pause (git `1d5e27e`)
- ✅ **keystone boots cleanly in QEMU** (Phase 0/1): serial → GDT/IDT → 504 MB frame
  allocator → 1 MB kernel heap → #BP exception recovery → clean halt.
- ✅ **cap-core formally verified** — 4 Kani proofs, 343 checks, 0 failures
  (I2 revocation, I3 no-amplification, I4 invoke-requires-right, encode round-trip).
- ✅ **cap_invoke wiring written + reviewed** (`kernel/src/capspace.rs`), compiles.
- ⛔ **BLOCKER:** the live `cap_invoke` boot demo is DISABLED because of a kernel
  paging bug (below). Everything else boots clean.
- ⏳ `frame-alloc` Kani proofs were still running when we paused (cap-core's 4 passed).

## THE BUG TO SOLVE (this is the immediate task)
**Symptom:** when the cap_invoke self-test runs, the first write to a large `.bss`
static (`OBJECTS`, the kernel ObjectTable) page-faults *not-present*.

**What we proved:**
- Limine leaves the *first* part of the kernel NOBITS `.bss` unmapped (e.g. `OBJECTS`
  / `ROOT_CAPS` ~16 KB), but maps `HEAP_SPACE` onward (the heap works).
- `paging::premap_bss()` maps the **entire** `.bss` in **normal context** and reports
  `all_ok=true` — and it *zeroes every page without faulting*, so the mapping genuinely
  takes and the CPU sees it.
- **But** capspace still faults on the first `.bss` write afterward → pages are being
  **silently UN-MAPPED between `premap_bss` and capspace** (during `init_heap` / the Vec
  test). This is the "map-then-unmap" signature.
- In the **#PF handler (interrupt context)**, reading the *active* `L4[511]` through the
  HHDM returns `0x0` — impossible, since the CPU is executing from that L4 entry. The
  same read in normal context (`dump_walk`) returns `0x1ff81027`. So interrupt-context
  HHDM table reads are unreliable here; on-demand mapping from `#PF` does NOT work.

**Leading hypothesis:** the **frame allocator hands out a physical frame that is a live
page table** (or other in-use memory), and a write (heap init / Vec, or `premap_bss`
zeroing a freshly-mapped page) corrupts it, clearing L1 entries. Frame allocator
currently only `skip`s the first 1 MiB; that's insufficient.

**Hard facts from page-table walks (QEMU, 512 MB RAM):**
- CR3 phys = `0x1ff82000`. Kernel: `L4[511] → L3[510] → L2[0] → L1` (4 KiB pages).
- HHDM base = `0xffff800000000000`, at `L4[256]`, 4 KiB pages, covers high RAM.
- Kernel physical base ≈ `0x1f98e000` (virt `0x...80035000` → phys `0x1f9c3000`).
- Limine's page tables live at high phys `~0x1ff7b000–0x1ff82000` (just under 512 MB).
- Frame allocator gives lowest-first frames (≈1 MB+), so its frames are LOW — they
  should NOT overlap Limine's high page tables... yet the corruption pattern persists,
  so verify this assumption with the debugger.

**NEXT STEP (do this):** stop print-debugging; use a **debugger**.
1. Boot QEMU with a GDB stub: add `-s -S` to the qemu line in
   `C:\WSL\cairn-go-kernel.sh`, connect `gdb` (or `lldb`) to `:1234`, OR use the QEMU
   monitor (`-monitor unix:...` or `-monitor stdio`) and run `info mem` / `info tlb`.
2. Re-enable the cap_invoke demo (in `kernel/src/main.rs`, the block guarded by the
   "DISABLED pending the page-table rework" comment), break at the fault, and dump the
   page tables + the frame numbers `premap_bss`/the heap allocated. Find which write
   clears the `OBJECTS` L1 entry and what physical frame it aliases.
3. Fix: either (a) exclude the aliased region from the frame allocator
   (`kernel/src/memory.rs` `init`), or (b) have keystone build & load its OWN page
   tables (a self-contained higher-half map + direct map) instead of patching Limine's
   — the robust long-term fix. Then re-enable the demo and confirm the boot log shows
   `cap_invoke(ALLOC,0) => Ok frame=0x...` twice + `demo_revoke_then_invoke => ErrRevoked`.

## Dev environment (CRITICAL — WSL1, not WSL2)
- **WSL2 is BROKEN on this Windows host** (VM rootfs extraction hangs). We use **WSL1**:
  Ubuntu 24.04 imported as `--version 1` to `C:\WSL\UbuntuWSL1`. Runs as root. apt works
  (shares Windows networking). Good for Rust + QEMU (TCG, no KVM) + Kani.
- **Invoke WSL from the PowerShell tool**, e.g. `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/<script>.sh`.
  Do NOT use the Bash tool for this — git-bash mangles `/mnt/c` paths into `C:/Program Files/Git/mnt/...`.
- **PowerShell 5.1 mangles embedded double-quotes** passed to native exes (git, grok).
  Use `--prompt-file` for grok and here-strings WITHOUT `"` for git commit messages.
- Toolchain: rustup nightly + `rust-src`, `qemu-system-x86` 8.2.2, `ovmf`, `xorriso`,
  Kani 0.67 (`cargo kani`). Limine 9.6.7 binary at `/root/limine`.
- Repo lives at `C:\Users\danie\Desktop\Cairn` (canonical — EDIT HERE). Scripts rsync it
  to `~/cairn` in WSL, excluding `target/` so incremental builds persist.

## Build / boot / verify commands
- **Build + boot (fast loop):** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-go-kernel.sh`
  (rsyncs `kernel/` only, force-relinks, builds Limine BIOS ISO, runs QEMU 20 s, serial →
  `/root/cairn-serial.log` and stdout).
- **Full rebuild (kernel + crates):** `.../cairn-rebuild.sh` ; **whole pipeline:** `.../cairn-go.sh`.
- **Kani proofs:** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/kani-proofs.sh` (slow — cap-core
  took ~26 min; runs `cargo kani -p cap-core --features kani` and `-p frame-alloc`).
- Filter serial output in PowerShell with `... | Select-String -Pattern "..."` (no `grep`).
- Helper scripts in `C:\WSL\`: cairn-go-kernel.sh, cairn-rebuild.sh, cairn-go.sh,
  kani-proofs.sh, kani-setup.sh, limine-setup.sh, investigate-fault.sh, nm-sections.sh, api-dump*.sh.

## Kernel build specifics (so you don't rediscover them)
- Target: built-in **`x86_64-unknown-none`** + rustflags in `kernel/.cargo/config.toml`
  (`code-model=kernel`, `relocation-model=static`, `link-arg=-Tlinker.ld`). NOT a custom
  `.json` target (Rust gates those behind `-Zjson-target-spec` and the format keeps changing).
- Needs `#![feature(abi_x86_interrupt)]` (kernel) and `generic_const_exprs` (frame-alloc).
- Deps: `limine = 0.6.3` (0.3/0.4 are YANKED), `x86_64 = 0.15.4`, `spin`, `linked_list_allocator`,
  `bitflags`. limine 0.6 API: `MemmapRequest`, `.response()`, markers at crate root,
  `memmap::Entry.type_` / `MEMMAP_USABLE`. x86_64 0.15: `GlobalDescriptorTable::append`,
  `Segment` trait import for `CS::set_reg`, `Cr2::read()` returns `Result`.
- `linker.ld`: `.got` placed in `.data` BEFORE `.bss`; `.bss` page-aligned and LAST, with
  `__bss_start`/`__bss_end` symbols. Limine base revision negotiates to 3 (crate requests
  6 → `is_supported()` is false; made non-fatal). Limine config uses v9 syntax
  (`/Entry` + `kernel_path: boot():/boot/keystone`). cargo does NOT track `linker.ld`, so
  the boot script `touch`es a source file to force a relink.
- GDT must reload SS/DS/ES to our data segment (Limine leaves stale selectors → #GP on iretq).

## Collaboration model (Claude × Grok)
- **Grok** (xAI CLI at `C:\Users\danie\.grok\bin\grok.exe`) writes greenfield Rust into the
  repo: `& "$env:USERPROFILE\.grok\bin\grok.exe" --prompt-file <path> --cwd "$env:USERPROFILE\Desktop\Cairn" --always-approve --permission-mode bypassPermissions --disable-web-search --max-turns N`.
  Model `grok-build` does NOT support `--effort`; needs `--max-turns >= 8`.
- **Claude** orchestrates, reviews Grok's `unsafe`, drives the build/boot/verify loop and
  (later) the real-hardware loop, keeps proofs green, builds the management-plane UI.

## Roadmap after the paging fix
Wire cap_invoke live (the disabled demo) → APIC timer + EDF scheduler (time-caps) →
`syscall`/`sysret` + ring 3 + first userspace domain doing a real cap_invoke → portal IPC →
Phase 3 (zero-kernel I/O + object store) → Phase 4 (network-boot onto the real HPE ProLiant
via James's existing iPXE server; see the `studio-server-access` memory) → Phase 5
(confidential boot + beautiful management plane). Keep adding Kani proofs per component.

## Server (not needed until Phase 4)
HPE ProLiant, currently OFF. iLO `192.168.99.2` (web, user Administrator, reachable only with
laptop Ethernet in the POE switch). OS over Wi-Fi: `ssh james@studio.local` (key-only,
`~/.ssh/id_ed25519`). Details in the `studio-server-access` memory.
