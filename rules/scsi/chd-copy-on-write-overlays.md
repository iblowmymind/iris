# Copy-on-write for CHD disks — what the CHD format forces on us

Everything below was measured against `libchdman-rs` 0.289 (MAME 0.289), and
every claim has a test pinning it in `src/chd_disk.rs`. Read this before
touching the overlay code: three of these behaviours are not documented
anywhere in the library, and each one silently corrupts guest data if you
guess wrong.

## An uncompressed CHD cannot be a CHD parent

MAME stores the parent relationship *as the parent's SHA-1*, in the child's
header. chdman only computes a SHA-1 while compressing, so **every
uncompressed CHD has an all-zero SHA-1**, and the pair behaves like this:

```
Chd::create_with_parent(diff, .., &parent)   → Ok
diff.clone_all_metadata(&parent)             → Ok
Chd::open(diff, writeable, Some(&parent))    → Err(InvalidFile)   ← here
```

`HdImage::open_with_diff` does all three, so it fails on the last one — and it
fails *after* creating the diff file. That left an unusable `.diff.chd` beside
the base, and since every later open takes the "a diff exists" branch, the disk
then refused to open at all, with or without COW. One attempt at COW on an
uncompressed CHD bricked the disk configuration until the user deleted the file
by hand.

Two consequences for anyone reading the header of such a file:

- `ChdInfo::has_parent` is **not** the header field. It reports whether *this
  handle* was opened with a parent, so it is false for any diff you open on its
  own. Use `parent_sha1 != [0; 20]` to ask what the file records.
- A diff created against an uncompressed parent therefore claims no parent at
  all, and is indistinguishable from a parentless CHD except that it holds
  nothing. `classify_overlay` treats that shape as the legacy artefact and
  clears it — after proving every hunk is unallocated, because deleting a file
  that *did* hold writes would throw away a session.

So an uncompressed base gets a **sparse overlay** instead: a parentless CHD of
the same geometry holding only the hunks the guest wrote, with the rest read
from the base. A compressed base keeps the MAME-native parented diff, which
works and stays chdman-compatible.

## MAME deallocates an all-zero hunk, so the hunk map is not a presence bitmap

An uncompressed CHD's hunk map already records which hunks exist, which is
exactly the "does the overlay own this hunk" question — `hunk_info(h).compbytes`
is `hunk_bytes` for a present hunk and `0` for an absent one. Free, in the same
file, updated by MAME as part of the same write. Very tempting.

It does not work, because:

| what happens | map afterwards |
|---|---|
| write a non-zero hunk | present |
| write an **all-zero** hunk that was never allocated | **absent** |
| overwrite a present hunk with zeros | present (stays allocated) |

Only the middle row is a problem, and it is a bad one: in an overlay, "absent"
means "read through to the base", so a guest zeroing a hunk that was non-zero
in the base would read back the base's old data on the next open. Silent
resurrection of erased data.

Those hunks — and only those — are recorded in a bitmap in the overlay's own
`IRCW` metadata record. Presence is the union of the map and the bitmap. The
record is fixed-length from creation so MAME rewrites it in place, and it is
only rewritten when a hunk first becomes all-zero, so the ordinary write path
costs one `write_hunk` and no metadata I/O.

## Writes are durable only when the file is closed

MAME writes hunk *data* through to the file as it goes, but keeps the hunk map
and the metadata index in memory until the CHD is closed. A second handle
opened on the same path sees neither:

```
w.write_hunk(3, data);  fsync(path);
Chd::open(path, false, None).hunk_info(3).compbytes   → 0
drop(w);
Chd::open(path, false, None).hunk_info(3).compbytes   → 4096
```

So `fsync` on the path flushes bytes that nothing points at — present on the
platter and unreachable, which is the same as losing the write. There is no
explicit flush in the library. `ChdHd::flush` therefore **closes and reopens**
the CHD, which is the only thing that commits the map, and SCSI SYNCHRONIZE
CACHE and FUA writes go through it. Measured cost of the reopen (it re-reads
the hunk map):

| disk | map size | close + reopen |
|---|---|---|
| 2 GB | 2 MB | 0.5 ms |
| 18 GB | 19 MB | 3.4 ms |
| 240 GB | 234 MB | 34 ms |

Affordable for a barrier, so the flush is skipped entirely unless something has
been written since the last one.

One side effect worth knowing: `write_metadata` rewrites the file header, which
pushes the hunk map out as a side effect. It does not make its own record
visible. Do not read that as "metadata writes are durable".

## An uncompressed CHD's floor is its hunk map

A 240 GB uncompressed CHD is ~234 MB on disk with no data in it at all:
58,593,792 hunks × 4 bytes of map. An overlay for that disk starts at the same
234 MB. That is the format, not a copy of the base — the original bug report
was an overlay the size of the *whole disk*, which is a different order of
magnitude. `sparse_overlay_on_a_240gb_disk` (ignored by default) holds the line
at 300 MB.

Related: build such a disk with `Chd::create` plus a `GDDD` record, the way
`chdman createhd -c none` does. Going through `create_from_reader` streams the
full logical size through the compressor — 240 GB of zeros, tens of minutes —
for a disk that has nothing in it.

## Commit is not atomic, so it is journalled

Folding a sparse overlay back writes its hunks into the base in place, which
cannot be done atomically. `commit_sparse_overlay` drops an `.apply` marker
first; the overlay still holds every hunk, so replaying is idempotent, and
`ChdHd::open` finishes an interrupted commit before doing anything else. A
stale marker next to something that is not a sparse overlay is cleared with a
warning rather than treated as an error — failing closed there would brick the
disk, which is the failure mode this whole path exists to avoid.

Folding a *compressed* base's diff is a different operation: the base is
rebuilt through the compressor (`flatten_diff`). Use `commit_overlay`, which
dispatches on what is actually there. Calling `flatten_diff` on a sparse
overlay fails, because it tries to open a parentless file as a parent's child.

## The compressor zero-pads read errors, so the caller must check

`StreamingSource::read_data` maps both a read error and an early EOF to
"zero-pad the rest", and says so in its own comment: it has no error channel
back to MAME and leaves the caller to compare a SHA-1 if it cares.
`flatten_diff` replaces the user's base CHD with its output, so it cares very
much — without a check, one unreadable hunk in the base or diff becomes a base
full of zeros there, and the original is already gone. `CheckedReader` latches
the first error and counts the bytes produced; the rename is refused unless the
source ran clean and complete.

`flatten_diff` also has to carry the base's metadata across. It used to pass
`ident: None`, so folding a diff back **silently stripped the IDNT record**.
GDDD and IDNT are now preserved explicitly, and any other record makes the fold
refuse rather than quietly drop it.
