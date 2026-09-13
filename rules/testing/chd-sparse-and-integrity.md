# CHD sparse images and disk integrity

`chdman createhd -c none` never writes a SHA-1 — not just for blank images,
for *every* uncompressed CHD, including ones full of data (only the
compressors compute it). MAME links a diff to its parent by that SHA-1 and
cannot reopen a diff whose parent checksum is zero. Do not invent a checksum
or interpret an unidentified diff as a valid standalone disk.

For these parents IRIS creates a **sparse overlay**: a parentless uncompressed
CHD of the same geometry, tagged `IRSO`, holding only the hunks the guest
wrote. `hunk_info` reports an unwritten hunk with compressor 2 (MAME's
CHD_CODEC_PARENT); reads of those come from the base, and the first write to
one copies the whole hunk up from the base before patching it (a parentless
CHD would otherwise zero the rest of the hunk). The overlay costs its hunk map
(4 bytes per hunk) plus the hunks actually written.

An earlier version copied the entire base container into the overlay (tag
`IRIS`), so every COW overlay of an installed uncompressed disk was as large as
the disk. Those overlays still open and commit; they are no longer created.

The `IRSO` record stores the base's size and modification time. Opening checks
them and fails closed if the base changed underneath the overlay, preserving
both files. Normal checksummed (compressed) parents continue to use MAME
parent-dependent diffs.

Committing a sparse overlay into an uncompressed base writes only its stored
hunks, in place. `<base>.apply` marks the commit as in progress: past it the
commit can only be completed, not cancelled, and an interrupted commit is
re-applied (idempotently, from the still-present overlay) by the next open.

An old failed overlay without a parent identity is rejected and preserved.
Inspect it offline and retain a backup before replacing it. Never delete an
unknown diff merely because opening it failed.

`libchdman-rs` 0.289's streaming compressor turns both read errors and premature
EOF into zero padding. `CheckedReader` latches errors independently; merge
must check that latch before installing output. Cancellation and source errors
must preserve the original base and diff.

Rebuilding a compressed base replaces it by rename, and replacing a base and
removing its diff are two operations. `.sync.json` records container hashes
before replacement; reopening finishes cleanup only if the
installed base and remaining diff match. Unexpected contents fail closed.
A damaged or incomplete journal requires offline inspection. Directory syncing
is implemented on Unix; Windows crash durability still needs native testing.

CHD snapshots previously omitted disk data. Save and restore now reject CHD
hard disks before stopping or resetting the machine. Implement verified CHD
snapshot capture, provenance, and restore preflight before removing this guard.

SCSI SYNCHRONIZE CACHE and WRITE(10) FUA must flush the actual backend. Completing
a host write is not the same as persisting it to storage.

Regression tests use temporary disks only:

```sh
cargo test --lib --features chd chd_disk::tests
cargo test --lib --features chd chdman_sparse_large_disk -- --ignored --nocapture
```

The second command requires `chdman` and reproduces
`createhd -ss 512 -chs 128,16,228882 -c none`: 240,000,172,032 logical bytes,
with a roughly 234 MB CHD hunk map rather than a 240 GB disk allocation.
`cow_overlay_on_uncompressed_base_stays_small` is the regression test for the
full-copy overlay.
