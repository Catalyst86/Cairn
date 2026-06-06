# Cairn — Session Resume / Handoff

**Read this first, then `DESIGN.md`, `docs/CAP_ABI.md`, and the `cairn-os` memory.**
Cairn is a from-scratch, capability-based OS for James's HPE ProLiant x86-64 server,
built as a **Claude Code × Grok Build** collaboration. Repo: `C:\Users\danie\Desktop\Cairn`.

## Status (milestone: cap_invoke is LIVE)
- ✅ **keystone boots cleanly in QEMU** (Phase 0/1): serial → GDT/IDT → **1 MiB Limine
  boot stack** → frame allocator → 1 MB kernel heap → #BP exception recovery.
- ✅ **cap-core formally verified** — 4 Kani proofs, 343 checks, 0 failures
  (I2 revocation, I3 no-amplification, I4 invoke-requires-right, encode round-trip).
- ✅ **`cap_invoke` is LIVE** (`kernel/src/capspace.rs` driving the verified cap-core).
  Boot log: `init_root => cptr=0`, `cap_invoke(ALLOC,0) => Ok frame=0x100000` then
  `0x101000`, `demo_revoke_then_invoke => ErrRevoked` — the verified I2 epoch-revocation
  invariant running in the real kernel.
- ✅ **The paging "map-then-unmap" bug is FIXED** (root cause + fix below).
- ⏳ `frame-alloc` Kani proofs were still running when we paused (cap-core's 4 passed).

## THE PAGING BUG — SOLVED (was: "map-then-unmap")
**The previous diagnosis was WRONG.** It was NOT an unmapped `.bss` tail and NOT
frame-allocator aliasing of a live page table — both were red herrings.

**Real root cause: a kernel-stack overflow into Limine's page tables.** Limine's default
~64 KiB boot stack sits immediately above its own page tables (in QEMU: stack top
~`0x1ff92790`; tables at phys `0x1ff7f000`–`0x1ff82fff`). In debug builds the by-value
construction of the 8 KiB `ObjectTable` static (capspace `OBJECTS`) cascades several
full-struct `memcpy`s (~90 KiB of stack), overflowing the boot stack DOWN through
L1/L2/L3/L4 and zeroing them. The first write to `OBJECTS` (not yet cached in the TLB)
then page-faulted **not-present**, while the CPU kept executing off stale TLB entries —
which is exactly why the #PF handler saw "L4 reads 0": the table genuinely was zero.

**How it was proven (GDB stub, as planned in the old handoff):**
- Normal-context page-table walk: Limine maps ALL of `.bss`; `OBJECTS` was present the
  whole time; **zero frames** were allocated before the fault → both old hypotheses dead.
- QEMU `monitor xp` (physical read, bypasses the broken translation) at the fault: the
  entire page-table region L1–L4 read back as zeros.
- GDB **hardware watchpoint** on the OBJECTS L1 PTE caught the clobber: `compiler_builtins`
  `memcpy`, write dest `0x1ff7f1b8` (inside the tables), RSP `0x1ff7c3f0` (below all four).

**The fix:** request a 1 MiB boot stack via Limine `StackSizeRequest::new(1 MiB)` — a
`.requests` static in `kernel/src/main.rs`; `kmain` reports whether Limine honored it
(`Boot stack: 1024 KiB (Limine request honored)`). Removed `premap_bss` (it was a no-op
built on the false premise). The `#PF` `manual_map` mapper stays only as a defensive
backstop. Building keystone's own page tables is now OPTIONAL polish, not a blocker.

**Debug tooling now in place (reusable, in `C:\WSL\`):** `gdb` installed in WSL1.
- `cairn-gdb.sh [cmds]` — boots the EXISTING `cairn.iso` under a frozen QEMU
  (`-S -gdb tcp::1234`), runs `gdb -batch -x <cmds>` (default `cairn-gdb.cmds`), then
  dumps serial. Reuses the iso so addresses match the last `cairn-go-kernel.sh` run.
- `cairn-gdb.cmds` — break at the #PF handler (by raw address), dump CR2/CR3 + page
  tables physically via `monitor xp`.
- `cairn-gdb-wp.cmds` — `set language c`, then a HW `watch` on a PTE to catch the clobber.
- Gotchas: with the kernel ELF loaded, gdb is in **Rust mode** → `set language c` before
  any C-typed `watch`/`x`; break by **raw address** (`break *0xADDR` from `nm`) since Rust
  symbols are mangled; use `monitor xp /Ngx <phys>` to read tables when translation is dead.

## Dev environment (CRITICAL — WSL1, not WSL2)
- **WSL2 is BROKEN on this Windows host** (VM rootfs extraction hangs). We use **WSL1**:
  Ubuntu 24.04 imported as `--version 1` to `C:\WSL\UbuntuWSL1`. Runs as root. apt works
  (shares Windows networking). Good for Rust + QEMU (TCG, no KVM) + Kani + gdb.
- **Invoke WSL from the PowerShell tool**, e.g. `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/<script>.sh`.
  Do NOT use the Bash tool for this — git-bash mangles `/mnt/c` paths into `C:/Program Files/Git/mnt/...`.
- **PowerShell 5.1 mangles embedded double-quotes** passed to native exes (git, grok).
  Use `--prompt-file` for grok and here-strings WITHOUT `"` for git commit messages.
- Toolchain: rustup nightly + `rust-src`, `qemu-system-x86` 8.2.2, `ovmf`, `xorriso`,
  Kani 0.67 (`cargo kani`), `gdb` 15.1. Limine 9.6.7 binary at `/root/limine`.
- Repo lives at `C:\Users\danie\Desktop\Cairn` (canonical — EDIT HERE). Scripts rsync it
  to `~/cairn` in WSL, excluding `target/` so incremental builds persist.

## Build / boot / verify commands
- **Build + boot (fast loop):** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-go-kernel.sh`
  (rsyncs `kernel/` only, force-relinks, builds Limine BIOS ISO, runs QEMU 20 s, serial →
  `/root/cairn-serial.log` and stdout).
- **GDB debug:** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-gdb.sh [/mnt/c/WSL/<cmds>]`
  (reuses the last-built `cairn.iso`; gdb output via `set logging` → `/root/cairn-gdb*.log`).
- **Full rebuild (kernel + crates):** `.../cairn-rebuild.sh` ; **whole pipeline:** `.../cairn-go.sh`.
- **Kani proofs:** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/kani-proofs.sh` (slow — cap-core
  took ~26 min; runs `cargo kani -p cap-core --features kani` and `-p frame-alloc`).
- Filter serial output in PowerShell with `... | Select-String -Pattern "..."` (no `grep`).
- Helper scripts in `C:\WSL\`: cairn-go-kernel.sh, cairn-rebuild.sh, cairn-go.sh,
  kani-proofs.sh, kani-setup.sh, limine-setup.sh, cairn-gdb.sh, cairn-gdb.cmds,
  cairn-gdb-wp.cmds, investigate-fault.sh, nm-sections.sh, api-dump*.sh.

## Kernel build specifics (so you don't rediscover them)
- Target: built-in **`x86_64-unknown-none`** + rustflags in `kernel/.cargo/config.toml`
  (`code-model=kernel`, `relocation-model=static`, `link-arg=-Tlinker.ld`). NOT a custom
  `.json` target (Rust gates those behind `-Zjson-target-spec` and the format keeps changing).
- Needs `#![feature(abi_x86_interrupt)]` (kernel) and `generic_const_exprs` (frame-alloc).
- Deps: `limine = 0.6.3` (0.3/0.4 are YANKED), `x86_64 = 0.15.4`, `spin`, `linked_list_allocator`,
  `bitflags`. limine 0.6 API: `MemmapRequest`, `StackSizeRequest::new(size)`, `.response()`,
  markers at crate root, `memmap::Entry.type_` / `MEMMAP_USABLE`. x86_64 0.15:
  `GlobalDescriptorTable::append`, `Segment` trait for `CS::set_reg`, `Cr2::read()` → Result.
- `linker.ld`: `.got` placed in `.data` BEFORE `.bss`; `.bss` page-aligned and LAST, with
  `__bss_start`/`__bss_end` symbols. Segment now has FileSiz `0xc0` ≪ MemSiz (clean NOBITS
  `.bss` tail) and Limine maps the whole thing — confirmed by page-table walk. Limine base
  revision negotiates to 3 (crate requests 6 → `is_supported()` false; made non-fatal).
  Limine config uses v9 syntax (`/Entry` + `kernel_path: boot():/boot/keystone`). cargo does
  NOT track `linker.ld`, so the boot script `touch`es a source file to force a relink.
- GDT must reload SS/DS/ES to our data segment (Limine leaves stale selectors → #GP on iretq).
- **Boot stack: request it explicitly** via `StackSizeRequest` — Limine's default boot stack
  is small (~64 KiB) and adjacent to its page tables; deep call chains overflow it (see bug above).

## Collaboration model (Claude × Grok)
- **Grok** (xAI CLI at `C:\Users\danie\.grok\bin\grok.exe`) writes greenfield Rust into the
  repo: `& "$env:USERPROFILE\.grok\bin\grok.exe" --prompt-file <path> --cwd "$env:USERPROFILE\Desktop\Cairn" --always-approve --permission-mode bypassPermissions --disable-web-search --max-turns N`.
  Model `grok-build` does NOT support `--effort`; needs `--max-turns >= 8`.
- **Claude** orchestrates, reviews Grok's `unsafe`, drives the build/boot/verify loop and
  (later) the real-hardware loop, keeps proofs green, builds the management-plane UI.

## Roadmap after the cap_invoke milestone
APIC timer + EDF scheduler (time-caps) → `syscall`/`sysret` + ring 3 + first userspace
domain doing a real cap_invoke → portal IPC → Phase 3 (zero-kernel I/O + object store) →
Phase 4 (network-boot onto the real HPE ProLiant via James's existing iPXE server; see the
`studio-server-access` memory) → Phase 5 (confidential boot + beautiful management plane).
Keep adding Kani proofs per component; finish the `frame-alloc` proofs. Building keystone's
own page tables is now optional hardening, no longer blocking.

## Server (not needed until Phase 4)
HPE ProLiant, currently OFF. iLO `192.168.99.2` (web, user Administrator, reachable only with
laptop Ethernet in the POE switch). OS over Wi-Fi: `ssh james@studio.local` (key-only,
`~/.ssh/id_ed25519`). Details in the `studio-server-access` memory.
