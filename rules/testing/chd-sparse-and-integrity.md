# CHD sparse images and disk integrity

Blank `chdman createhd -c none` images have no SHA-1. MAME cannot reopen a
parent-dependent diff when the parent's checksum is zero. Do not invent a
checksum or interpret an unidentified diff as a valid standalone disk.

For these parents, IRIS copies the CHD container into the overlay and stores a
BLAKE3 digest of the parent container in an `IRIS` metadata record. This copies
the allocated CHD container, not the virtual disk. Commit copies this standalone
image and removes the private metadata; it does not expand unallocated hunks.
Normal checksummed parents continue to use MAME parent-dependent diffs.

An old failed overlay without a parent identity is rejected and preserved.
Inspect it offline and retain a backup before replacing it. Never delete an
unknown diff merely because opening it failed.

`libchdman-rs` 0.289's streaming compressor turns both read errors and premature
EOF into zero padding. `CheckedReader` latches errors independently; merge
must check that latch before installing output. Cancellation and source errors
must preserve the original base and diff.

Replacing a base and removing its diff are two operations. `.sync.json` records
container hashes before replacement; reopening finishes cleanup only if the
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
