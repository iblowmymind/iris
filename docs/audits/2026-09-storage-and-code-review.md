# IRIS code review and Indy roadmap

This review prioritized guest filesystem integrity after the sparse-CHD report.
It also surveyed CPU/JIT, device, networking, snapshot, GUI, and build paths.
It is a source review with automated regression tests, not a claim that every
instruction, device register, or power-loss scenario has been verified.
The running emulator, its disks, and its installed app bundle were not changed.

## Changes implemented

| Area | Confirmed problem | Change |
|---|---|---|
| Sparse CHD + COW | A blank uncompressed parent has a zero SHA-1; creating its diff succeeds but reopening the parent relationship fails. | Use a standalone copy of the CHD container with a verified parent-container digest. Commit preserves unallocated hunks. |
| CHD merge reads | The compression adapter silently zero-pads source read errors and early EOF. | Latch source errors independently and reject output before replacing the base. |
| CHD merge interruption | Replacing the base and removing its old diff were not one atomic operation; cleanup errors were ignored. | Sync a hash journal before replacement. Reopen verifies the installed base and diff before completing cleanup. |
| CHD metadata | Rebuild omitted IDNT and could silently discard other metadata. | Preserve IDNT; reject unsupported additional metadata rather than discard it. Standalone copies retain metadata. |
| CHD writes | A multi-sector out-of-range write could modify earlier sectors, then fail without marking the diff dirty. | Validate the whole range before writing; mark attempted diff writes dirty before I/O. Reject zero-byte sectors. |
| CHD commit/reset | An error could leave a device detached or report a failed discard as successful. | Reopen the backend after the operation and propagate errors. Failed overlay creation uses a temporary file. |
| CHD snapshots | Export/import silently omitted CHD disk state, allowing RAM to rewind against a different disk. | Reject CHD hard-disk snapshot save/restore before stopping or resetting the machine. Full CHD snapshot support remains unimplemented. |
| SCSI durability | SYNCHRONIZE CACHE was a no-op despite advertising write-back caching; WRITE(10) FUA was ignored. | Flush raw, COW, and CHD backends; return CHECK CONDITION on a flush error. |
| SCSI requests | Writes could extend a raw disk; short CDBs could panic. | Reject out-of-range writes and truncated CDBs; allow zero-transfer WRITE(10). |
| Raw COW | Commit inferred the base from the overlay filename. Corrupt dirty lists were ignored, and truncation left stale dirty metadata. | Remember the actual base path, validate dirty-list contents/ranges, and persist an empty dirty set before truncation. |
| Snapshot chunks | Concurrent saves reused one temporary filename; reads did not verify the content hash. | Use distinct temporary files and verify BLAKE3 on reads and reuse. |
| NFS | Existing host symlinks could escape the export. Directory renames broke descendant handles. Writes claimed stability without syncing. | Reject symlink traversal, preserve descendant handles across rename, retire replaced handles, and sync writes. Host-side concurrent path replacement still needs descriptor-relative hardening. |
| REX JIT | A full 256-entry compile queue left rejected keys permanently marked queued. | Remove rejected keys so later draws can retry. Unit tests no longer load the user's warm-up profile. |

Implementation: `src/chd_disk.rs`, `src/scsi.rs`, `src/cow_disk.rs`,
`src/chunk_store.rs`, `src/nfsudp.rs`, `src/rex3_jit/mod.rs`,
`src/wd33c93a.rs`, and `src/machine.rs`.

## Sparse CHD reproduction

The test ran the reported command against a temporary path:

```sh
chdman createhd -ss 512 -chs 128,16,228882 -c none -o disk0.chd
```

It verified 240,000,172,032 bytes of logical capacity, zero reads, a write to the
last sector, unchanged base contents during COW, reopening, and commit. The
resulting CHD remained under 300 MB, rather than expanding to 240 GB. The
smaller regression also checks neighboring sectors and rejects partial
out-of-range writes. This validates the disk backend; no live IRIX installation
or filesystem repair was performed.

If an earlier failed attempt left an unidentified `.diff.chd`, the new code
preserves and rejects it. Inspect it offline and retain a backup before
recreating the overlay. Do not delete a diff belonging to a running VM.
See [CHD integrity notes](../../rules/testing/chd-sparse-and-integrity.md).

## Validation

- Core suite with `lightning,rex-jit,pcap,jitv2,chd`: **895 passed, 14 ignored**.
- GUI suite with `iris/lightning,iris/rex-jit,iris/pcap,iris/jitv2`: **52 passed**.
- Default GUI build check: **passed**.
- Exact-command large sparse CHD integration test: **passed** separately.
- Final SCSI checks after the zero-transfer correction: **2 passed**.
- NFS tests including symlink and directory-rename regressions: **23 passed**.
- Initial socket tests were denied by the sandbox; rerunning with localhost
  socket access passed. No VM connection was used.
- Initial graphics tests reproduced compiler-queue timeouts with a 263-entry
  user profile. After the queue fix and test isolation, the full suite passed.
- Clippy was reviewed, but it is not clean: existing unsafe-aliasing lints,
  constant-expression diagnostics, and style warnings remain. They were not
  suppressed or treated as proof of a CPU defect.
- Native Windows/Linux runtime tests, guest boot/fsck, disk-full injection, and
  physical power-loss testing were not performed. No new bundle was installed.

## Remaining integrity work

1. **CHD snapshot support:** capture a closed, durable overlay and its exact
   parent identity; validate every disk before changing RAM; install restored
   overlays transactionally. Reject old snapshots that omitted CHD disk data.
2. **Raw snapshot transactions:** overlay import and its dirty list remain
   separate files. Add a journal or a single-file overlay format and preflight
   all source data before restoring machine state. A missing dirty sidecar
   still means historical raw-overlay writes cannot be reconstructed reliably.
3. **Crash and failure injection:** exercise disk-full, permission changes,
   interrupted journal writes, changed parent/diff contents, and Windows
   replacement semantics. An incomplete journal currently fails closed for
   offline inspection. Single-writer ownership across separate emulator
   processes is not enforced by these changes.
4. **NFS host-race confinement:** replace pathname validation with directory
   handles and descriptor-relative no-follow operations. The current fix
   blocks existing symlinks, not a hostile host process swapping paths between
   validation and use. Add stable-directory metadata tests.
5. **Unsafe memory ownership:** audit raw-pointer lifetime and aliasing across
   CPU/DMA/render threads with bounded harnesses. The existing unsafe layout is
   architectural; changing it without memory/JIT benchmarks risks regressions.

## Indy hardware roadmap — proposals only

The inventory below separates existing partial devices from absent peripherals.
Hardware availability is grounded in SGI's archived
[Indy configurations](https://archive.irixnet.org/siliconsurf/products/Indy/Indy_Report2.html)
and [XZ technical report](https://archive.irixnet.org/siliconsurf/products/Indy/Indy_Report5.html).
Implementation status is based on the repository, not those hardware documents.

| Priority | Missing or partial behavior | Proposed work and acceptance criterion |
|---|---|---|
| 1 | HAL2 analog recording supplies silence (`src/hal2.rs`, Codec B timer). | Connect host capture to the existing DMA path. Test mono/stereo formats, rates, record/playback, overruns, and snapshot lifecycle in IRIX. |
| 2 | HAL2 digital audio is internal TX/RX loopback, without host digital output. | Define supported host streams, bounded buffering, channel status, and interrupt behavior; validate loopback and independent recording. |
| 3 | XZ graphics is a preview register/FIFO stub (`src/xz.rs`), with no working command execution. | Research real HQ2/GE7 protocol first; implement command transport, geometry, rasterization, display, and interrupt stages. Require IRIX driver initialization, X11, then GL image comparisons. Do not treat placeholder register addresses as specifications. |
| 4 | Parallel-port signaling has an IRQ definition, without a printer/backend implementation. | Implement register/handshake behavior and a file-backed printer target; verify the IRIX printer driver and transfer errors. |
| 5 | No complete ISDN controller/backend was found. | Establish controller register and DMA documentation; add a local simulated endpoint before considering external connectivity. |
| 6 | SCSI targets cover disk/CD and optional DaynaPort, not sequential tape or a dedicated floptical device model. | Add tape semantics—filemarks, rewind, spacing, sense and media state—and test IRIX tar/restore. Evaluate floptical geometry/media behavior separately. |
| 7 | No distinct R4600 CPU model is exposed; R4400 and R5000 paths already exist. | Specify PRId/cache/TLB/FPU differences and reuse common execution paths. Gate on CPU tests and IRIX boot rather than adding a cosmetic model label. |
| 8 | Indy Video/Cosmo Compress and other optional GIO cards are absent from the reviewed wiring. | Select one actual software need, acquire its register/firmware evidence, then implement probe, DMA, IRQ, and useful data flow. |

VINO/camera input, Newport graphics, Ethernet, audio playback, serial, keyboard,
and mouse already have implementations; they are not listed as missing wholesale.
The SCC status-affects-vector TODO and MC narrow-access panic paths need focused
hardware-behavior tests. Empty `clock()` TODOs alone are not evidence of missing
hardware because several devices run through independent timers or threads.
Indigo2 IMPACT/MGRAS is outside this Indy inventory.

## Performance roadmap — proposals only

No speedup percentage is claimed: this review did not run controlled guest
performance measurements alongside the user's live VM.

| Order | Candidate | Why investigate | Measurement and guardrail |
|---|---|---|---|
| 1 | CHD hunk-sized I/O and merged-read buffering | Current runtime and compression reader cross the library boundary per sector. | Measure sequential/random guest I/O and merge time. Compare every output sector; retain error propagation and sparse behavior. |
| 2 | Framebuffer copy/upload reduction | `FrameSink::snapshot` clones a frame; the capture path marks the full image dirty. | Measure copies, upload bytes, frame latency, and host CPU with static and scrolling screens. Verify scaling, palette changes, cursor, resize and both heads. |
| 3 | HAL2 DMA batching | Several timers run at one sample per tick and lock shared state. | Batch bounded sample groups while preserving DMA boundaries, interrupts and drift; measure wakeups and underruns. |
| 4 | CPU/JIT fallback and translation profiling | Hot fallback, TLB/cache checks, and compilation can dominate different workloads. | Use `iris-bench matrix` plus fixed guest tasks. Keep instruction correctness, exception PC/BD, self-modifying code, and endian tests as gates. |
| 5 | REX shader warm-up policy | A large persistent profile can compete with shaders needed immediately. | Prioritize on-demand work and cap background warm-up; measure time to first interactive frame. The queue-loss bug itself is already fixed. |
| 6 | Snapshot throughput with durability | RAM chunk copies and per-file overhead compete with integrity guarantees. | Measure save/restore latency and peak memory; keep digest verification and adopt explicit transaction completion before optimizing fsync frequency. |

Recommended sequence: finish storage fault injection and snapshot transactions,
measure I/O/framebuffer/audio costs, then choose one hardware feature. Analog
recording is the smallest useful missing feature; XZ is a substantially larger
research project. None of these hardware or performance proposals was implemented.
