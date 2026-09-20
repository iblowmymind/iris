# Where storage state actually becomes durable, per backend

A sweep of the storage paths turned up the same mistake in four places: code
that reports success for a durability guarantee it does not provide. Each entry
below is the boundary that really exists, and what depends on it.

## A raw COW overlay's dirty list is the overlay

`CowDisk` keeps its dirty-sector set in memory and writes it to a `.dirty`
sidecar on flush or drop. **The overlay file's bytes are meaningless without
that list** — "dirty" deliberately means "the host finished writing this
sector", not "the file has bytes here", because a sparse file can hold a
partial write from an interrupted run.

That makes the sidecar the thing to protect, and it has an ordering rule:

- `commit` and `reset_overlay` **persist the empty set before truncating the
  overlay**. In the other order, a crash in between leaves a sidecar naming
  sectors the truncated overlay no longer holds, and every read of one fails at
  end-of-file. `commit` used to truncate with no sidecar write at all.
- A sidecar that cannot be what we wrote is a **hard error**, not an empty set.
  Silently starting clean hides every write a previous session made while the
  overlay's bytes sit there unreferenced. Validate the header count against the
  file length *before* using it to reserve anything: a corrupt count of
  `u64::MAX` reaches `HashSet::with_capacity` and aborts the process.
- Range-check every LBA against the base's sector count. An entry past the end
  sends reads to an offset the base does not have, and `commit` writes it back,
  extending the base.

`commit` also has to use the **configured** base path. It used to reconstruct
one by stripping a `.overlay` suffix from the overlay's name, which fails
outright for any other naming and would target the wrong file for a base that
merely ends that way.

## SYNCHRONIZE CACHE has to do something

MODE SENSE page 8 advertises a write-back cache, so IRIX is entitled to assume
the command that empties it means something. It was a bare `status: 0x00` with
no flush. For the COW backend that is the only thing that persists the dirty
map, so an abrupt exit lost the *entire* overlay, not just recent writes.

`DiskBackend::flush` now covers all three backends, and WRITE(10) with FUA set
flushes too. What "flush" costs differs per backend — see
`rules/scsi/chd-copy-on-write-overlays.md` for why the CHD one has to close the
file.

## NFS WRITE claims FILE_SYNC

Both WRITE handlers answer `committed = 2` (FILE_SYNC), which tells the client
the data is on stable storage — it may drop the page and skip COMMIT. The
implementation called `File::flush()`, which is a no-op for a `File`. Now
`sync_all`, on WRITE and on the SETATTR truncate.

## Snapshots can only carry what a backend can export

`ScsiDevice::is_cow()` is true for a CHD with an overlay, so
`export_overlays` called `cow_export` on one — which matched only the raw COW
backend and returned an empty list, creating no file. The snapshot recorded an
entry claiming an overlay with no dirty sectors, and restore's `cow_import` was
likewise a no-op. RAM rewound; the CHD kept every write made after the save.
Nothing about the result looks wrong until IRIX starts finding damage.

Two rules came out of it:

- Ask **before stopping the machine**. `Machine::snapshot_disk_blocker` runs
  ahead of `stop()` in both `save_snapshot` and `load_snapshot_inner`, so an
  unsupported disk aborts with the machine still running, rather than after it
  is halted or — far worse — after RAM has been rewound. A backend advertises
  its own limitation through `ScsiDevice::snapshot_blocker`.
- On restore, a missing overlay file with a **non-empty** dirty list is an
  error. Adopting the list anyway points the disk at data that is not there and
  every read of those sectors fails. Missing plus genuinely empty is fine —
  that is a clean disk.

CHD snapshot support is still unimplemented; it needs a closed, durable overlay
captured together with the exact base identity, and a transactional install on
restore.

## A content-addressed store has to check the content

`ChunkStore` named chunks by their BLAKE3 digest and then trusted the name.
`get` returned bytes without hashing them, and `put` skipped the write entirely
if a file with that name already existed. It also skips per-chunk `fsync` on
purpose (4096 of them cost ~20 s on APFS), so a chunk whose bytes never reached
the platter is a real shape — and it would be adopted by every later snapshot
that hashed to it, then loaded back as RAM.

Both reuse and every read now re-hash. That is free in the sense that matters:
the store is content-addressed, so the check is the same data it already needs.

The temp filename was also a pure function of the hash, so two concurrent puts
of the same chunk shared one path — one could truncate the other's partial write
and then publish a short chunk under the final name. Temp names now carry pid
and a counter, and `gc` sweeps the leftovers.

## SCSI request decoding

Two things in this area are not durability but belong with the sweep:

- **A truncated CDB was a panic.** `request()` read `cdb[1]` after only
  checking `is_empty()`, and the 10-byte decoders read as far as `cdb[8]`. A
  guest that arms a transfer count of one and issues TRANSFER_INFO hands the
  controller a one-byte "CDB" — reachable from the guest, and it took the host
  process down. Both the controller and `request()` now check the length the
  opcode's group implies.
- **Writes were not bounds-checked**, though reads were. An out-of-range WRITE
  extended a raw image (and a COW overlay past its base), so the disk grew
  behind IRIX's back and the sectors past the advertised capacity became
  unreachable again after the next READ CAPACITY. Also: a zero-block WRITE(10)
  is legal and arrives with no data-out phase, so check the count before the
  data or it fails as a length mismatch.
