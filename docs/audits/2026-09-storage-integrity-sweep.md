# Storage integrity sweep — September 2026

A re-verification of the storage paths from scratch, plus copy-on-write for CHD
disks. Every finding below was reproduced against the code on `main` before it
was changed, and every fix has a regression test named in the table. This is a
source review with automated tests: no live IRIX installation was booted, no
guest filesystem was fsck'd, and no power-loss testing was done.

An earlier branch (`storage-fixes`) had listed a set of problems in this area.
Those claims were re-derived independently here rather than carried over; the
last section gives a verdict on each, including the two that were described
inaccurately and the one that was missed.

## Method

Claims about the CHD format were settled by measurement against
`libchdman-rs` 0.289 (MAME 0.289) rather than by reading MAME's behaviour off
its headers, because the C++ source is not shipped with the crate and three of
the behaviours that matter are undocumented. Each measurement is now a test, so
a library upgrade that changes one of them fails loudly:

- `uncompressed_base_cannot_be_a_chd_parent`
- `zeroing_a_hunk_does_not_fall_through_to_the_base`
- `chd_writes_are_only_durable_once_the_file_is_closed`

## Copy-on-write for CHDs

COW on a CHD did not work on `main`, and failing left the disk unusable.

`ChdHd::open(path, cow: true)` called `HdImage::open_with_diff`, which creates a
child CHD naming the base as its parent. MAME stores that relationship *as the
parent's SHA-1*, and chdman computes a SHA-1 only while compressing — so every
uncompressed CHD has an all-zero one and the reopen fails:

```
Chd::create_with_parent(diff, .., &parent)   → Ok
diff.clone_all_metadata(&parent)             → Ok
Chd::open(diff, writeable, Some(&parent))    → Err(InvalidFile)
```

`ChdHd::open` propagated that error, so the disk never attached. Worse, the diff
file had already been created, and every subsequent open takes the "a diff
exists" branch — so the disk then refused to open **with COW off as well**. One
attempt at COW on an uncompressed CHD bricked the disk configuration until the
user deleted the file by hand.

An uncompressed base now gets a **sparse overlay** instead: a parentless CHD of
the same geometry holding only the hunks the guest has written, with everything
else read from the base and a hunk copied up on its first write. A compressed
base keeps the MAME-native parented diff, which works and stays
chdman-compatible.

The hard part is recording which hunks the overlay owns. An uncompressed CHD's
hunk map already answers that, and it is the right place to read it from — MAME
maintains it for free, in the same file, as part of the same write. But **MAME
deallocates a hunk whose content is entirely zeros**, so a first write that
leaves a hunk all-zero is indistinguishable from one that never happened, and
falling through to the base there would hand back the data the guest had just
erased. Those hunks, and only those, are recorded in a bitmap in the overlay's
own `IRCW` metadata record; presence is the union of the two. The record is
fixed-length so MAME rewrites it in place, and it is only touched when a hunk
first becomes all-zero — the ordinary write path costs one `write_hunk` and no
metadata I/O.

The overlay records the base's size and mtime and refuses a base that changed
underneath it. `cow commit` folds either overlay flavour (`commit_overlay`
dispatches); a sparse overlay's hunks are written back in place under an
`.apply` marker, so an interrupted commit is replayed by the next open.

### The original report

The case that started this — `chdman createhd -ss 512 -chs 128,16,228882 -c
none`, 240,000,172,032 bytes — is `sparse_overlay_on_a_240gb_disk` (ignored by
default; it writes ~470 MB). The overlay comes out at ~234 MB and the test holds
the line at 300 MB. That floor is the hunk map itself, 58,593,792 hunks × 4
bytes, not copied data: an empty uncompressed CHD of that size is already
234 MB.

## Findings and fixes

| Area | Confirmed problem | Fix | Regression |
|---|---|---|---|
| CHD COW | An uncompressed base cannot be a CHD parent, so COW failed to attach; the leftover diff then blocked every later open, COW or not. | Sparse parentless overlay; unusable legacy diffs cleared once proven empty. | `sparse_overlay_on_uncompressed_base`, `unusable_legacy_diff_does_not_brick_the_disk` |
| CHD COW | MAME deallocates an all-zero hunk, so the hunk map alone would resurrect the base's erased data. | Presence = hunk map ∪ `IRCW` bitmap. | `zeroing_a_hunk_does_not_fall_through_to_the_base` |
| CHD durability | `flush` was an `fsync` of the path, but MAME holds the hunk map and metadata index in memory until close — the data was present and unreachable. | `flush` closes and reopens; skipped when nothing is pending. | `flush_commits_the_overlay_and_keeps_the_disk_open`, `flush_is_a_no_op_when_nothing_was_written` |
| CHD writes | Written sector-by-sector with no range check, so a multi-sector write whose tail was out of range modified the sectors before it; `dirty` was set only on success. | Validate the whole range first; mark dirty before the I/O. | `out_of_range_writes_are_refused_whole` |
| CHD merge | The compression source zero-pads read errors and early EOF, so one unreadable hunk produced a base full of zeros there — and the original was already gone. | `CheckedReader` latches the error and counts bytes; the rename is refused unless the source ran clean and complete. | `checked_reader_latches_source_errors` |
| CHD metadata | Folding a diff back passed `ident: None`, silently stripping IDNT. | GDDD and IDNT preserved; any other record makes the fold refuse rather than drop it. | `flatten_preserves_ident_metadata` |
| CHD commit | An interrupted in-place fold left the base half-updated with nothing recording it. | `.apply` marker; the next open replays it (idempotent). A stale marker beside a non-sparse overlay is cleared with a warning rather than failing closed. | `interrupted_commit_is_resumed_on_open` |
| CHD commit/reset | Both set the backend to `None` and returned early on error, leaving the target permanently media-less; reset reported success even when the discard failed. | Reopen unconditionally; propagate the first real failure. | — (error paths; covered by inspection) |
| SCSI | SYNCHRONIZE CACHE returned GOOD without flushing, while MODE SENSE page 8 advertised a write-back cache. For the COW backend that was the only thing persisting the dirty map, so an abrupt exit lost the whole overlay. | Flush all backends; WRITE(10) honours FUA. | `synchronize_cache_flushes_and_reports_status`, `in_range_write_lands_and_fua_is_honoured` |
| SCSI | Writes were not bounds-checked though reads were, so an out-of-range write extended a raw image (and a COW overlay past its base). | LBA range check as on the read path. | `out_of_range_write_is_refused_and_does_not_grow_the_image` |
| SCSI | A CDB too short for its opcode indexed past its end — a guest-reachable panic that took the host process down. | Length check in the controller and in `request()`. | `truncated_cdbs_are_rejected_not_panics`, `full_length_cdbs_are_accepted` |
| SCSI | A zero-block WRITE(10) is legal and arrives with no data-out phase; it failed as a length mismatch. | Count checked before the data. | `zero_block_write_succeeds` |
| Raw COW | `commit` reconstructed the base path by stripping `.overlay` from the sidecar's name. | Remember the configured base path. | `commit_uses_the_configured_base_not_the_overlay_name` |
| Raw COW | `commit` truncated the overlay without persisting the empty dirty set, so a crash in between left a sidecar naming sectors the overlay no longer held and every read of one failed at EOF. | Persist the empty set before truncating, in commit and reset. | `commit_persists_an_empty_dirty_set_before_truncating`, `reset_persists_an_empty_dirty_set` |
| Raw COW | A corrupt dirty list was read as empty (hiding a session's writes), and its header count reached `HashSet::with_capacity` — `u64::MAX` aborts the process. | Validate the count against the file length and every LBA against the disk; reject rather than silently empty. | `corrupt_dirty_lists_are_rejected` |
| Raw COW | Writes were unbounded, and a non-sector-multiple length was a `debug_assert` — release builds wrote the tail and did not mark it dirty. | Range check; a real error for a bad length. | `out_of_range_writes_are_refused` |
| Snapshots | `is_cow()` is true for a CHD, so export was called and returned an empty list with no file. RAM rewound while the disk kept every later write. | A backend declares its own limitation; checked **before** `stop()` on save and restore. | `raw_cow_has_no_snapshot_blocker` |
| Snapshots | A restore with a missing overlay file adopted a non-empty dirty list anyway. | Refused; missing plus empty is still fine. | `import_refuses_a_missing_overlay_with_dirty_sectors` |
| Chunk store | `get` returned bytes without hashing them and `put` reused an existing file on name alone, so a chunk that never reached the platter would be adopted and loaded back as RAM. | BLAKE3 verified on read and on reuse. | `get_rejects_a_chunk_whose_content_changed`, `get_rejects_a_truncated_chunk` |
| Chunk store | The temp filename was a pure function of the hash, so concurrent puts shared one path and could publish a short chunk under the final name. | Per-writer temp names; `gc` sweeps leftovers. | `concurrent_puts_use_distinct_temp_files` |
| NFS | Only the final path component was `lstat`ed, so a symlink in the export was interned and then followed by every real operation — a way out of the export. | No component may be a symlink; such entries are not listed either. | `host_symlinks_cannot_escape_the_export` |
| NFS | Renaming a directory re-pointed only the exact path, so descendant handles went stale — and a file later created at an old path inherited one. | Re-point the whole subtree; retire a clobbered destination's handles and a removed subtree's. | `directory_rename_keeps_descendant_handles_valid`, `rename_over_a_file_retires_its_handle`, `remove_retires_handles` |
| NFS | WRITE answered `committed = FILE_SYNC` but only called `File::flush()`, a no-op. | `sync_all` on WRITE and on the SETATTR truncate. | `write_reports_file_sync_and_means_it` |

## Verdict on the earlier branch's claims

`storage-fixes` listed thirteen problems. Eleven are real and were reproduced
here independently. Two were described in a way that does not match the code,
and one serious consequence was missing.

**Confirmed, and the described fix is the right one** — CHD merge read errors;
CHD merge interruption; CHD metadata/IDNT; CHD write range validation; CHD
commit/reset leaving a device detached; CHD snapshot refusal before stopping the
machine; SCSI SYNCHRONIZE CACHE and FUA; SCSI out-of-range writes and short
CDBs; raw COW base path; raw COW dirty-list validation and truncation ordering;
snapshot chunk temp files and BLAKE3; NFS symlinks, directory renames and write
syncing.

**Described inaccurately:**

- *"COW on an uncompressed CHD produced an overlay the size of the whole disk"*
  (commit message). It did not — the open **failed outright**, so no overlay was
  usable at all. That sentence describes a first attempt at the fix, not the
  behaviour being fixed. The branch's own audit table states it correctly
  ("creating its diff succeeds but reopening the parent relationship fails"), so
  the two documents disagree with each other.
- *"MAME cannot link a diff to a parent whose SHA-1 is zero"* is true but
  incomplete in a way that matters for writing the code: `ChdInfo::has_parent`
  reports whether *the handle* was opened with a parent, not what the header
  records, and a child created against a zero-SHA-1 parent records **no parent
  at all**. Classifying an existing diff on `has_parent` therefore misfiles
  every diff, including a valid one over a compressed base. That was caught here
  only because the deletion path proves a file is empty before removing it.

**Missing:** that a failed COW attempt leaves the `.diff.chd` behind and
**permanently prevents the disk from opening**, with COW off as well. This is the
most user-visible consequence of the original bug — the disk appears to break
for good — and it is not mentioned in either the commit message or the audit.
`unusable_legacy_diff_does_not_brick_the_disk` covers it, and such files are now
cleared automatically once proven empty.

**Also not previously noted:** MAME's hunk map cannot serve as an overlay's
presence record, because an all-zero hunk is deallocated. Any sparse-overlay
implementation that reads presence from the map alone silently resurrects erased
guest data. The earlier branch used a separate sidecar, which sidesteps the trap
without naming it; this one reads the map and covers the zero case explicitly.

## Not done

1. **CHD snapshot support.** Still unimplemented, and now refused rather than
   silently skipped. It needs a closed, durable overlay captured with the exact
   base identity, every disk validated before RAM is touched, and a
   transactional install on restore.
2. **Raw snapshot transactions.** The overlay and its dirty list are still two
   files restored separately. A missing sidecar still means a previous session's
   raw-overlay writes cannot be reconstructed.
3. **Fault injection.** Disk-full, permission changes, interrupted journal
   writes, a base replaced mid-session, and Windows replacement semantics were
   not exercised. Single-writer ownership across two emulator processes is not
   enforced.
4. **NFS host races.** The symlink fix blocks symlinks that exist; it does not
   stop a hostile host process swapping a directory for one between the check
   and the operation. That needs descriptor-relative, no-follow syscalls.
5. **Guest-level validation.** No IRIX boot, `fsck`, or install was run against
   these changes. The 240 GB case is validated at the block-device layer only.

## Validation

- `cargo test --features lightning,rex-jit,jitv2,chd`: 1043 passed, 0 failed,
  21 ignored.
- `cargo test -p iris-gui --features iris/lightning,iris/rex-jit,iris/pcap,iris/jitv2`:
  52 passed.
- Default-feature `cargo build`: clean. `--features chd` build: clean.
- `sparse_overlay_on_a_240gb_disk` passes separately (1.2 s in release).
- Clippy is not clean in this repository and was not made clean; the four lints
  in the files touched here were fixed, leaving one pre-existing
  `io_other_error` in `chd_disk.rs`.
