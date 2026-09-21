//! CHD-backed disk implementations for the SCSI subsystem.
//!
//! Two flavors:
//!   * [`ChdHd`] — hard-disk CHD as a writable block device. Three write paths,
//!     chosen by the base's compression and the per-disk COW flag: in place
//!     (uncompressed base, COW off), a MAME-style parented `.diff.chd`
//!     (compressed base), or a [`SparseOverlay`] (uncompressed base, COW on) —
//!     see that type for why an uncompressed base cannot be a CHD parent.
//!   * [`ChdCd`] — single-track MODE1 CD CHD exposed as a 2048-byte/sector
//!     read-only stream via libchdman-rs's `CdCookedReader`.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use libchdman_rs::cd::CdCookedReader;
use libchdman_rs::hd::HdImage;
use libchdman_rs::Chd;

fn map_err<E: std::fmt::Debug>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{:?}", e))
}

fn corrupt(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn path_str(p: &Path) -> io::Result<&str> {
    p.to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 CHD path"))
}

/// `p` with `suffix` appended to its filename (rather than replacing its
/// extension, which `Path::with_extension` would do to `foo.diff.chd`).
fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Open a base CHD read-only — the source a COW overlay reads through.
fn open_base_chd(base: &Path) -> io::Result<Chd> {
    Chd::open(path_str(base)?, false, None).map_err(|e| {
        io::Error::other(format!("cannot open base CHD {}: {:?}", base.display(), e))
    })
}

fn fsync_path(p: &Path) -> io::Result<()> {
    std::fs::OpenOptions::new().read(true).write(true).open(p)?.sync_all()
}

fn fsync_dir(dir: Option<&Path>) -> io::Result<()> {
    if let Some(d) = dir {
        std::fs::File::open(d)?.sync_all()?;
    }
    Ok(())
}

// ── sparse copy-on-write overlay for an uncompressed base CHD ────────────────
//
// Why this exists at all: MAME's runtime-write strategy is a *parented* diff.
// `chd_file::create(..., parent)` stores the parent's SHA-1 in the child header
// and opening the child validates it against the parent. That works for a
// compressed base and only for a compressed base, because chdman computes a
// SHA-1 only while compressing — every uncompressed CHD carries an all-zero
// one, and MAME then refuses the link. `Chd::create_with_parent` succeeds and
// `Chd::open(diff, .., Some(parent))` fails `InvalidFile`, which is how COW on
// an uncompressed CHD used to fail outright (and, because the unusable diff was
// left on disk, kept the disk from opening at all afterwards).
// `uncompressed_base_cannot_be_a_chd_parent` pins that library behaviour.
//
// So an uncompressed base gets a *parentless* overlay instead: a CHD of the
// same geometry holding only the hunks the guest has written, with everything
// else read from the base and a hunk copied up on its first write. The base is
// never opened for writing while the overlay is attached.
//
// Presence — which hunks the overlay owns — is the whole difficulty. An
// uncompressed CHD's hunk map already records it (an unwritten hunk has an
// empty map entry, so `hunk_info().compbytes == 0`), and that is where the
// answer normally comes from: MAME maintains it for free, in the same file, as
// part of the same write. But MAME *deallocates* a hunk whose content is
// entirely zeros, so a first write that leaves a hunk all-zero is
// indistinguishable from one that never happened — and falling through to the
// base there would hand back the data the guest just erased. Those hunks, and
// only those, are recorded in a bitmap inside the overlay's own `IRCW`
// metadata record; presence is the union of the two.
// `zeroing_a_hunk_does_not_fall_through_to_the_base` is the regression for it.
//
// The record is fixed-length from creation so MAME rewrites it in place, and it
// is rewritten only when a hunk first becomes all-zero (rare) or on close —
// never on the ordinary write path, which costs exactly one `write_hunk`.

/// Metadata tag for the overlay's own bookkeeping record: `IRCW`.
const OVERLAY_TAG: u32 = u32::from_be_bytes(*b"IRCW");
/// Bumped only for an incompatible change to the record layout.
const OVERLAY_FORMAT: u32 = 1;
/// Byte offset of the presence bitmap within the record.
const BITMAP_OFFSET: usize = 48;

/// The overlay's `IRCW` record: which base this overlay belongs to, plus the
/// presence bitmap for hunks MAME deallocated because they hold only zeros.
///
/// Little-endian and fixed-size (`BITMAP_OFFSET + zero_hunks.len()`), so every
/// rewrite has the same length and MAME overwrites the record in place rather
/// than appending a fresh metadata block each time.
struct OverlayHeader {
    base_logical: u64,
    base_hunk_bytes: u32,
    base_unit_bytes: u32,
    /// Base file length and mtime, checked on attach: a base that changed
    /// underneath its overlay makes every copied-up hunk meaningless.
    base_len: u64,
    base_mtime_secs: i64,
    base_mtime_nanos: u32,
    /// Bit `h` set = the overlay owns hunk `h` even though its map entry is
    /// empty. Only ever set for a hunk whose content is entirely zeros.
    zero_hunks: Vec<u8>,
}

impl OverlayHeader {
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(BITMAP_OFFSET + self.zero_hunks.len());
        v.extend_from_slice(&OVERLAY_TAG.to_le_bytes());
        v.extend_from_slice(&OVERLAY_FORMAT.to_le_bytes());
        v.extend_from_slice(&self.base_logical.to_le_bytes());
        v.extend_from_slice(&self.base_hunk_bytes.to_le_bytes());
        v.extend_from_slice(&self.base_unit_bytes.to_le_bytes());
        v.extend_from_slice(&self.base_len.to_le_bytes());
        v.extend_from_slice(&self.base_mtime_secs.to_le_bytes());
        v.extend_from_slice(&self.base_mtime_nanos.to_le_bytes());
        v.extend_from_slice(&(self.zero_hunks.len() as u32).to_le_bytes());
        debug_assert_eq!(v.len(), BITMAP_OFFSET);
        v.extend_from_slice(&self.zero_hunks);
        v
    }

    fn decode(raw: &[u8]) -> io::Result<Self> {
        if raw.len() < BITMAP_OFFSET {
            return Err(corrupt("overlay IRCW record is too short"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(raw[o..o + 8].try_into().unwrap());
        if u32_at(0) != OVERLAY_TAG {
            return Err(corrupt("overlay IRCW record has the wrong magic"));
        }
        let fmt = u32_at(4);
        if fmt != OVERLAY_FORMAT {
            return Err(corrupt(format!(
                "overlay IRCW record format {fmt} is not supported (this build writes \
                 {OVERLAY_FORMAT})"
            )));
        }
        let bitmap_len = u32_at(44) as usize;
        if raw.len() != BITMAP_OFFSET + bitmap_len {
            return Err(corrupt("overlay IRCW record length disagrees with its bitmap size"));
        }
        Ok(Self {
            base_logical: u64_at(8),
            base_hunk_bytes: u32_at(16),
            base_unit_bytes: u32_at(20),
            base_len: u64_at(24),
            base_mtime_secs: i64::from_le_bytes(raw[32..40].try_into().unwrap()),
            base_mtime_nanos: u32_at(40),
            zero_hunks: raw[BITMAP_OFFSET..].to_vec(),
        })
    }
}

/// `(len, mtime_secs, mtime_nanos)` identity for a base CHD. Size and mtime
/// rather than a content hash: re-hashing a multi-gigabyte base on every launch
/// would dominate startup, and this catches what actually happens in practice —
/// the base being rebuilt, restored from a backup, or replaced behind the
/// overlay's back.
fn base_identity(base: &Path) -> io::Result<(u64, i64, u32)> {
    let md = std::fs::metadata(base)?;
    let (secs, nanos) = match md.modified()?.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        // A pre-epoch mtime is legal on some filesystems; keep it representable.
        Err(e) => (-(e.duration().as_secs() as i64), 0),
    };
    Ok((md.len(), secs, nanos))
}

/// Just the overlay side of a sparse overlay: the CHD and its `IRCW` record.
/// Split out from [`SparseOverlay`] so a commit can read the overlay while
/// holding the base open for writing, without two handles fighting over it.
struct OverlayFile {
    chd: Chd,
    header: OverlayHeader,
    /// Set when `header.zero_hunks` has changed since it was last persisted.
    header_dirty: bool,
    hunk_bytes: u32,
    hunk_count: u32,
    path: PathBuf,
}

impl OverlayFile {
    fn open(path: &Path) -> io::Result<Self> {
        let chd = Chd::open(path_str(path)?, true, None).map_err(|e| {
            corrupt(format!("cannot open COW overlay {}: {:?}", path.display(), e))
        })?;
        let raw = chd.read_metadata(OVERLAY_TAG, 0).map_err(|_| {
            corrupt(format!(
                "{} is not an IRIS sparse overlay (no IRCW record). Move it aside to \
                 start a fresh overlay, or keep it for offline inspection.",
                path.display()
            ))
        })?;
        let header = OverlayHeader::decode(&raw)?;
        let info = chd.info().map_err(map_err)?;
        let expect = (info.hunk_count as usize).div_ceil(8);
        if header.zero_hunks.len() != expect {
            return Err(corrupt(format!(
                "overlay {} has a {}-byte presence bitmap; this disk needs {}",
                path.display(),
                header.zero_hunks.len(),
                expect
            )));
        }
        Ok(Self {
            chd,
            header,
            header_dirty: false,
            hunk_bytes: info.hunk_bytes,
            hunk_count: info.hunk_count,
            path: path.to_path_buf(),
        })
    }

    fn zero_bit(&self, hunk: u32) -> bool {
        let (byte, bit) = (hunk as usize / 8, hunk % 8);
        self.header.zero_hunks.get(byte).is_some_and(|b| b & (1 << bit) != 0)
    }

    fn set_zero_bit(&mut self, hunk: u32) {
        let (byte, bit) = (hunk as usize / 8, hunk % 8);
        if let Some(b) = self.header.zero_hunks.get_mut(byte) {
            if *b & (1 << bit) == 0 {
                *b |= 1 << bit;
                self.header_dirty = true;
            }
        }
    }

    /// Whether the overlay owns hunk `h`: either MAME's own map has it, or it is
    /// one of the all-zero hunks MAME deallocated and the bitmap remembers.
    fn owns(&self, hunk: u32) -> io::Result<bool> {
        if self.zero_bit(hunk) {
            return Ok(true);
        }
        let hi = self.chd.hunk_info(hunk).map_err(map_err)?;
        // The overlay is always uncompressed and parentless, so a hunk is either
        // fully present (one raw block) or absent (an empty map entry). Anything
        // else means we are not looking at the file we think we are.
        match hi.compbytes {
            0 => Ok(false),
            n if n == self.hunk_bytes => Ok(true),
            n => Err(corrupt(format!(
                "overlay hunk {hunk} reports {n} bytes in a {}-byte-hunk uncompressed CHD",
                self.hunk_bytes
            ))),
        }
    }

    /// Hunks the overlay owns, ascending.
    fn owned_hunks(&self) -> io::Result<Vec<u32>> {
        let mut out = Vec::new();
        for h in 0..self.hunk_count {
            if self.owns(h)? {
                out.push(h);
            }
        }
        Ok(out)
    }

    /// Write the presence bitmap into the overlay if it changed. A same-length
    /// rewrite, so MAME overwrites the record in place and the file does not grow.
    ///
    /// This hands the record to MAME, which is not the same as putting it on the
    /// platter: MAME keeps the hunk map and the metadata index in memory and
    /// writes them out when the file is closed, which is when both become
    /// durable.
    fn persist_header(&mut self) -> io::Result<()> {
        if !self.header_dirty {
            return Ok(());
        }
        self.chd
            .write_metadata(OVERLAY_TAG, 0, &self.header.encode(), 0)
            .map_err(map_err)?;
        self.header_dirty = false;
        Ok(())
    }
}

/// A parentless CHD holding only the hunks the guest wrote, reading everything
/// else from the base. See the module comment above for why the base cannot
/// simply be a CHD parent.
pub struct SparseOverlay {
    base: Chd,
    ov: OverlayFile,
    unit_bytes: u32,
    logical_bytes: u64,
    /// Scratch for hunk copy-up, kept allocated across writes.
    scratch: Vec<u8>,
}

impl SparseOverlay {
    /// Create a fresh overlay for `base` at `overlay_path`, which must not
    /// already exist.
    fn create(base_path: &Path, overlay_path: &Path) -> io::Result<Self> {
        let base = open_base_chd(base_path)?;
        let info = base.info().map_err(map_err)?;
        if info.compressed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a compressed base uses a parented diff, not a sparse overlay",
            ));
        }
        if info.hunk_bytes == 0 || info.unit_bytes == 0 {
            return Err(corrupt("base CHD reports a zero hunk or unit size"));
        }
        let (base_len, secs, nanos) = base_identity(base_path)?;
        let header = OverlayHeader {
            base_logical: info.logical_bytes,
            base_hunk_bytes: info.hunk_bytes,
            base_unit_bytes: info.unit_bytes,
            base_len,
            base_mtime_secs: secs,
            base_mtime_nanos: nanos,
            zero_hunks: vec![0u8; (info.hunk_count as usize).div_ceil(8)],
        };

        // Build the overlay in a temp file and rename it into place. A failure
        // part-way through then leaves nothing behind: the previous code created
        // the diff before the step that failed, and that leftover file made the
        // disk refuse to open on every later launch, COW or not.
        let tmp = with_suffix(overlay_path, ".creating");
        let _ = std::fs::remove_file(&tmp);
        let build = || -> io::Result<()> {
            let mut ov = Chd::create(
                path_str(&tmp)?,
                info.logical_bytes,
                info.hunk_bytes,
                info.unit_bytes,
                [0; 4], // uncompressed: hunks have to be writable in place
            )
            .map_err(map_err)?;
            // Carry the base's geometry and ident across, so the overlay is a
            // well-formed HD CHD in its own right and chdman can inspect it.
            ov.clone_all_metadata(&base).map_err(map_err)?;
            ov.write_metadata(OVERLAY_TAG, 0, &header.encode(), 0).map_err(map_err)?;
            drop(ov);
            fsync_path(&tmp)
        };
        if let Err(e) = build() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, overlay_path)?;
        let _ = fsync_dir(overlay_path.parent());

        Ok(Self {
            base,
            ov: OverlayFile::open(overlay_path)?,
            unit_bytes: info.unit_bytes,
            logical_bytes: info.logical_bytes,
            scratch: vec![0u8; info.hunk_bytes as usize],
        })
    }

    /// Reopen an existing overlay against its base, refusing a base that has
    /// changed underneath it.
    fn attach(base_path: &Path, overlay_path: &Path) -> io::Result<Self> {
        let base = open_base_chd(base_path)?;
        let info = base.info().map_err(map_err)?;
        let ov = OverlayFile::open(overlay_path)?;

        // Geometry must match, or a hunk index means different things in the two
        // files and every copied-up hunk lands in the wrong place.
        if ov.header.base_logical != info.logical_bytes
            || ov.header.base_hunk_bytes != info.hunk_bytes
            || ov.header.base_unit_bytes != info.unit_bytes
        {
            return Err(corrupt(format!(
                "overlay {} was made for a {}-byte disk with {}-byte hunks; the base is \
                 now {} bytes with {}-byte hunks",
                overlay_path.display(),
                ov.header.base_logical,
                ov.header.base_hunk_bytes,
                info.logical_bytes,
                info.hunk_bytes
            )));
        }
        let identity = base_identity(base_path)?;
        if identity != (ov.header.base_len, ov.header.base_mtime_secs, ov.header.base_mtime_nanos) {
            return Err(corrupt(format!(
                "the base CHD {} changed since its COW overlay was created, so the \
                 overlay's copied-up hunks no longer line up with it. Commit or discard \
                 {} deliberately — IRIS will not guess which one you meant.",
                base_path.display(),
                overlay_path.display()
            )));
        }
        Ok(Self {
            base,
            ov,
            unit_bytes: info.unit_bytes,
            logical_bytes: info.logical_bytes,
            scratch: vec![0u8; info.hunk_bytes as usize],
        })
    }

    pub fn logical_bytes(&self) -> u64 {
        self.logical_bytes
    }

    pub fn unit_bytes(&self) -> u32 {
        self.unit_bytes
    }

    /// Number of hunks the overlay owns — the "how much is uncommitted" figure.
    pub fn owned_hunk_count(&self) -> io::Result<usize> {
        Ok(self.ov.owned_hunks()?.len())
    }

    /// Read `buf.len()` bytes at `offset`, taking each hunk from the overlay
    /// where it owns it and from the base everywhere else.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| corrupt("read offset overflows"))?;
        if end > self.logical_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("read of {} bytes at {} runs past the end of the disk", buf.len(), offset),
            ));
        }
        let hb = u64::from(self.ov.hunk_bytes);
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let hunk = (pos / hb) as u32;
            let within = (pos % hb) as usize;
            let n = (self.ov.hunk_bytes as usize - within).min(buf.len() - done);
            if self.ov.owns(hunk)? {
                self.ov.chd.read_hunk(hunk, &mut self.scratch).map_err(map_err)?;
                buf[done..done + n].copy_from_slice(&self.scratch[within..within + n]);
            } else {
                self.base.read_bytes(pos, &mut buf[done..done + n]).map_err(map_err)?;
            }
            done += n;
        }
        Ok(())
    }

    /// Write `data` at `offset`. A hunk the overlay does not own yet is copied
    /// up from the base first, so a partial-hunk write keeps the base's bytes
    /// around the part that changed.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| corrupt("write offset overflows"))?;
        if end > self.logical_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write of {} bytes at {} runs past the end of the disk", data.len(), offset),
            ));
        }
        let hb = u64::from(self.ov.hunk_bytes);
        let mut done = 0usize;
        while done < data.len() {
            let pos = offset + done as u64;
            let hunk = (pos / hb) as u32;
            let within = (pos % hb) as usize;
            let n = (self.ov.hunk_bytes as usize - within).min(data.len() - done);

            // Assemble the hunk's new content: what it holds now (overlay if
            // owned, base otherwise) with `data` patched over it.
            if self.ov.owns(hunk)? {
                self.ov.chd.read_hunk(hunk, &mut self.scratch).map_err(map_err)?;
            } else {
                self.base
                    .read_bytes(u64::from(hunk) * hb, &mut self.scratch)
                    .map_err(map_err)?;
            }
            self.scratch[within..within + n].copy_from_slice(&data[done..done + n]);

            // MAME deallocates an all-zero hunk, so record it in the bitmap
            // *before* writing it. Claiming presence and then failing to write
            // costs this hunk's recent contents; writing without claiming would
            // silently hand back the base's stale data forever after.
            if self.scratch.iter().all(|&b| b == 0) {
                self.ov.set_zero_bit(hunk);
                self.ov.persist_header()?;
            }
            self.ov.chd.write_hunk(hunk, &self.scratch).map_err(map_err)?;
            done += n;
        }
        Ok(())
    }

    /// Hand the presence bitmap to MAME, without closing the file.
    fn persist_pending(&mut self) -> io::Result<()> {
        self.ov.persist_header()
    }
}

// ── folding a sparse overlay back into its base ──────────────────────────────

/// Marker written while a sparse overlay is being folded into its base.
/// Writing hunks into the base in place is not atomic, so an interrupted commit
/// has to be resumable: the overlay still holds every hunk, replaying them is
/// idempotent, and [`resume_interrupted_commit`] finishes the job on the next
/// open. Without it, a crash mid-commit leaves the base half-updated with
/// nothing on disk recording that fact.
fn apply_marker_path(overlay: &Path) -> PathBuf {
    with_suffix(overlay, ".apply")
}

/// Fold every hunk the overlay owns into the base, in place, then drop the
/// overlay. The caller must have closed its handles to both files first.
pub fn commit_sparse_overlay(
    base: &Path,
    overlay: &Path,
    progress: &mut dyn FnMut(f32),
) -> io::Result<usize> {
    let marker = apply_marker_path(overlay);
    std::fs::write(
        &marker,
        format!("base={}\noverlay={}\n", base.display(), overlay.display()),
    )?;
    fsync_path(&marker)?;
    let _ = fsync_dir(marker.parent());

    let applied = apply_overlay_hunks(base, overlay, progress)?;

    std::fs::remove_file(overlay)?;
    std::fs::remove_file(&marker)?;
    let _ = fsync_dir(overlay.parent());
    progress(1.0);
    Ok(applied)
}

/// Fold whichever kind of overlay sits at `diff` back into `base`, and return
/// how much was applied.
///
/// The single entry point callers should use: a sparse overlay's hunks are
/// written back in place, while a MAME-style parented diff needs the base
/// rebuilt through the compressor. Picking the wrong one fails outright — a
/// parentless sparse overlay cannot be opened as a CHD parent's child — so the
/// choice is made here rather than at each call site.
///
/// The caller MUST have closed any open handle to both files first.
pub fn commit_overlay(
    base: &Path,
    diff: &Path,
    progress: &mut dyn FnMut(f32),
    cancel: &dyn Fn() -> bool,
) -> io::Result<usize> {
    match classify_overlay(diff)? {
        ExistingOverlay::Sparse => commit_sparse_overlay(base, diff, progress),
        ExistingOverlay::Parented => flatten_diff(base, diff, progress, cancel).map(|()| 1),
        ExistingOverlay::UnlinkableParentless => {
            // Nothing was ever written through it; clearing it is the whole job.
            discard_unusable_diff(diff)?;
            Ok(0)
        }
    }
}

/// Discard an overlay and the bookkeeping files that belong to it, rolling the
/// disk back to the base's contents. Missing files are not an error — that is
/// the state a rollback is trying to reach.
pub fn discard_overlay(diff: &Path) -> io::Result<()> {
    let _ = std::fs::remove_file(apply_marker_path(diff));
    let _ = std::fs::remove_file(with_suffix(diff, ".creating"));
    match std::fs::remove_file(diff) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let _ = fsync_dir(diff.parent());
    Ok(())
}

/// The write half of a commit, factored out so an interrupted one can be
/// replayed. Idempotent — it only ever copies overlay hunks onto the base.
///
/// Deliberately does *not* check the overlay's recorded base identity: a resumed
/// commit runs against a base this very operation has already partly rewritten,
/// so its size and mtime no longer match what the overlay recorded.
fn apply_overlay_hunks(
    base: &Path,
    overlay: &Path,
    progress: &mut dyn FnMut(f32),
) -> io::Result<usize> {
    let ov = OverlayFile::open(overlay)?;
    let hunks = ov.owned_hunks()?;
    let total = hunks.len().max(1);
    let mut buf = vec![0u8; ov.hunk_bytes as usize];

    let mut dst = Chd::open(path_str(base)?, true, None).map_err(|e| {
        io::Error::other(format!("cannot open base CHD {} for writing: {:?}", base.display(), e))
    })?;
    let info = dst.info().map_err(map_err)?;
    if info.hunk_bytes != ov.hunk_bytes || info.hunk_count != ov.hunk_count {
        return Err(corrupt(format!(
            "overlay {} describes {} hunks of {} bytes; base {} has {} of {}",
            overlay.display(),
            ov.hunk_count,
            ov.hunk_bytes,
            base.display(),
            info.hunk_count,
            info.hunk_bytes
        )));
    }
    for (i, &h) in hunks.iter().enumerate() {
        ov.chd.read_hunk(h, &mut buf).map_err(map_err)?;
        dst.write_hunk(h, &buf).map_err(map_err)?;
        progress((i + 1) as f32 / total as f32);
    }
    drop(dst);
    fsync_path(base)?;
    Ok(hunks.len())
}

/// Finish a commit that was interrupted, if a marker says one was running.
/// Called before attaching an overlay, so the base is never left half-folded.
fn resume_interrupted_commit(base: &Path, overlay: &Path) -> io::Result<bool> {
    let marker = apply_marker_path(overlay);
    if !marker.exists() {
        return Ok(false);
    }
    // A marker only ever accompanies a sparse overlay. If the overlay is gone,
    // the commit had already finished and only the marker was left; if it is
    // something else entirely, the marker is stale. Either way, clearing it is
    // the right move — treating a stale marker as a hard error would leave the
    // disk unopenable, which is the failure mode this whole path exists to avoid.
    let resumable = overlay.exists()
        && matches!(classify_overlay(overlay), Ok(ExistingOverlay::Sparse));
    if resumable {
        eprintln!(
            "iris: finishing an interrupted COW commit of {} into {}",
            overlay.display(),
            base.display()
        );
        apply_overlay_hunks(base, overlay, &mut |_| {})?;
        std::fs::remove_file(overlay)?;
    } else if overlay.exists() {
        eprintln!(
            "iris: ignoring a stale commit marker beside {} — the overlay there is not a \
             sparse one",
            overlay.display()
        );
    }
    std::fs::remove_file(&marker)?;
    let _ = fsync_dir(overlay.parent());
    Ok(resumable)
}

/// What an existing `.diff.chd` beside a base turns out to be.
enum ExistingOverlay {
    /// Our sparse overlay (has an `IRCW` record).
    Sparse,
    /// A MAME-style parented diff, usable against a compressed base.
    Parented,
    /// A diff whose header records no parent and which is not one of ours — the
    /// artefact an older build left behind when it tried to overlay an
    /// uncompressed base.
    ///
    /// MAME stores the parent relationship *as* the parent's SHA-1, so a child
    /// created against an uncompressed (zero-SHA-1) parent records no parent at
    /// all. That is precisely why reopening it *with* one fails: nothing can ever
    /// be read through it, so no write ever reached it either.
    UnlinkableParentless,
}

fn classify_overlay(diff: &Path) -> io::Result<ExistingOverlay> {
    let chd = Chd::open(path_str(diff)?, false, None)
        .map_err(|e| corrupt(format!("cannot open {}: {:?}", diff.display(), e)))?;
    if chd.read_metadata(OVERLAY_TAG, 0).is_ok() {
        return Ok(ExistingOverlay::Sparse);
    }
    // `ChdInfo::has_parent` reports whether *this handle* was opened with one,
    // which is never true here — the header's recorded parent SHA-1 is what says
    // whether a parent was ever meant to exist.
    if chd.info().map_err(map_err)?.parent_sha1 != [0u8; 20] {
        return Ok(ExistingOverlay::Parented);
    }
    Ok(ExistingOverlay::UnlinkableParentless)
}

/// Remove an unlinkable diff, but only after proving it holds no guest data.
/// Such a file cannot be opened against its parent, so the write path never
/// reached it — but check rather than assume, because deleting the wrong file
/// here would throw away a session's writes.
fn discard_unusable_diff(diff: &Path) -> io::Result<()> {
    let chd = Chd::open(path_str(diff)?, false, None)
        .map_err(|e| corrupt(format!("cannot open {}: {:?}", diff.display(), e)))?;
    let info = chd.info().map_err(map_err)?;
    for h in 0..info.hunk_count {
        if chd.hunk_info(h).map_err(map_err)?.compbytes != 0 {
            return Err(corrupt(format!(
                "{} is a diff whose parent link cannot be validated, but it holds written \
                 hunks. Move it aside and inspect it offline; IRIS will not discard it.",
                diff.display()
            )));
        }
    }
    drop(chd);
    eprintln!(
        "iris: discarding {} — an empty overlay left behind by an earlier build that \
         could not link it to an uncompressed base",
        diff.display()
    );
    std::fs::remove_file(diff)?;
    let _ = fsync_dir(diff.parent());
    Ok(())
}

// ── the SCSI-facing hard-disk backend ────────────────────────────────────────

/// Where this disk's writes actually land.
enum HdBackend {
    /// Uncompressed base, written in place. No overlay.
    InPlace(HdImage),
    /// Compressed base with a MAME-style parented `.diff.chd`.
    Parented(HdImage),
    /// Uncompressed base with a sparse COW overlay.
    Sparse(SparseOverlay),
}

/// Writable hard-disk CHD backend.
pub struct ChdHd {
    backend: HdBackend,
    sector_size: u32,
    total_bytes: u64,
    /// The base CHD path (the file the user configured).
    base_path: PathBuf,
    /// The overlay sidecar, when writes go to one. `None` when writing the base
    /// in place.
    diff_path: Option<PathBuf>,
    /// Whether the overlay holds changes worth folding back into the base —
    /// either we wrote this session, or we reattached one that already existed.
    dirty: bool,
    /// Copy-on-write requested for this disk (the per-disk `overlay`/COW flag).
    /// When set we ALWAYS overlay, even an uncompressed base, so the base is
    /// never written in-session; and we NEVER auto-fold on exit — the user
    /// commits or rolls back deliberately via `cow commit` / `cow reset`.
    cow: bool,
}

// The underlying MAME chd_file holds a raw pointer (`*mut ChdFile`), making it
// !Send by default. We only ever own these from the SCSI worker thread (the
// backend is moved in once and never shared), so transferring ownership across
// threads is safe — we just don't share refs (no Sync).
unsafe impl Send for ChdHd {}
unsafe impl Send for ChdCd {}

impl ChdHd {
    pub fn open(path: &str, cow: bool) -> io::Result<Self> {
        let p = Path::new(path);
        let diff = diff_path_for(p);

        // A commit that was cut short left a marker; finish it before deciding
        // anything else, so we never read a half-folded base.
        if resume_interrupted_commit(p, &diff)? {
            // The overlay is gone now; fall through and make a fresh one if COW
            // is still on.
        }
        // A create that was cut short can leave this; it is never read from.
        let _ = std::fs::remove_file(with_suffix(&diff, ".creating"));

        let compressed = {
            let base = open_base_chd(p)?;
            let info = base.info().map_err(map_err)?;
            if !info.is_hd {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a hard-disk CHD", p.display()),
                ));
            }
            info.compressed
        };

        if diff.exists() {
            match classify_overlay(&diff)? {
                // Reattaching an existing overlay carries a previous session's
                // changes, so mark it dirty: a clean non-COW exit folds it.
                ExistingOverlay::Sparse => {
                    let ov = SparseOverlay::attach(p, &diff)?;
                    return Ok(Self::from_sparse(ov, p, diff, true, cow));
                }
                ExistingOverlay::Parented => {
                    let img = HdImage::reopen_diff(p, &diff).map_err(|e| {
                        corrupt(format!(
                            "cannot reattach overlay {} to base {}: {:?}",
                            diff.display(),
                            p.display(),
                            e
                        ))
                    })?;
                    return Ok(Self::from_image(HdBackend::Parented(img), p, Some(diff), true, cow));
                }
                ExistingOverlay::UnlinkableParentless => discard_unusable_diff(&diff)?,
            }
        }

        // No overlay on disk. A compressed base cannot be written in place, so
        // it always gets one; an uncompressed base gets one only under COW.
        if compressed {
            let img = HdImage::open_with_diff(p, &diff).map_err(map_err)?;
            Ok(Self::from_image(HdBackend::Parented(img), p, Some(diff), false, cow))
        } else if cow {
            let ov = SparseOverlay::create(p, &diff)?;
            Ok(Self::from_sparse(ov, p, diff, false, cow))
        } else {
            let img = HdImage::open(p).map_err(map_err)?;
            Ok(Self::from_image(HdBackend::InPlace(img), p, None, false, cow))
        }
    }

    fn from_image(
        backend: HdBackend,
        base: &Path,
        diff: Option<PathBuf>,
        dirty: bool,
        cow: bool,
    ) -> Self {
        let (sector_size, total_bytes) = match &backend {
            HdBackend::InPlace(img) | HdBackend::Parented(img) => {
                (img.sector_size(), img.sector_count() * u64::from(img.sector_size()))
            }
            HdBackend::Sparse(_) => unreachable!("from_sparse builds the sparse case"),
        };
        Self {
            backend,
            sector_size,
            total_bytes,
            base_path: base.to_path_buf(),
            diff_path: diff,
            dirty,
            cow,
        }
    }

    fn from_sparse(ov: SparseOverlay, base: &Path, diff: PathBuf, dirty: bool, cow: bool) -> Self {
        let sector_size = ov.unit_bytes();
        let total_bytes = ov.logical_bytes();
        Self {
            backend: HdBackend::Sparse(ov),
            sector_size,
            total_bytes,
            base_path: base.to_path_buf(),
            diff_path: Some(diff),
            dirty,
            cow,
        }
    }

    /// `(base, diff)` paths if a clean exit should **auto-fold** this disk's
    /// overlay back into the base — i.e. it has overlay-borne changes AND COW is
    /// off (COW on means "keep separate"; commit/rollback are then manual).
    pub fn pending_sync(&self) -> Option<(PathBuf, PathBuf)> {
        if self.cow {
            return None; // keep changes separate; never auto-fold
        }
        self.overlay_paths().filter(|_| self.dirty)
    }

    /// Whether this disk is in copy-on-write mode (the per-disk COW flag).
    pub fn is_cow(&self) -> bool {
        self.cow
    }

    /// Whether this disk's overlay is a sparse one (uncompressed base) rather
    /// than a MAME-style parented diff. Commit takes a different path for each.
    pub fn is_sparse_overlay(&self) -> bool {
        matches!(self.backend, HdBackend::Sparse(_))
    }

    /// `(base, diff)` when writes are landing in an overlay (regardless of the
    /// COW flag — a compressed base always overlays). Used by commit/reset.
    pub fn overlay_paths(&self) -> Option<(PathBuf, PathBuf)> {
        self.diff_path.as_ref().map(|d| (self.base_path.clone(), d.clone()))
    }

    /// Whether the overlay holds uncommitted changes.
    pub fn diff_dirty(&self) -> bool {
        self.dirty
    }

    pub fn size(&self) -> u64 {
        self.total_bytes
    }

    pub fn read_blocks(&mut self, lba: u64, count: usize, block_size: u64) -> io::Result<Vec<u8>> {
        let ss = u64::from(self.sector_size);
        if block_size != ss {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("CHD HD sector size {} != requested block size {}", ss, block_size),
            ));
        }
        let mut buf = vec![0u8; count * ss as usize];
        match &mut self.backend {
            HdBackend::InPlace(img) | HdBackend::Parented(img) => {
                for i in 0..count {
                    let off = i * ss as usize;
                    img.read_sector(lba + i as u64, &mut buf[off..off + ss as usize])
                        .map_err(map_err)?;
                }
            }
            HdBackend::Sparse(ov) => ov.read_at(lba * ss, &mut buf)?,
        }
        Ok(buf)
    }

    pub fn write_sectors(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        let ss = self.sector_size as usize;
        if !data.len().is_multiple_of(ss) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("CHD HD write length {} not a multiple of sector {}", data.len(), ss),
            ));
        }
        let count = data.len() / ss;
        match &mut self.backend {
            HdBackend::InPlace(img) | HdBackend::Parented(img) => {
                for i in 0..count {
                    let off = i * ss;
                    img.write_sector(lba + i as u64, &data[off..off + ss]).map_err(map_err)?;
                }
            }
            HdBackend::Sparse(ov) => ov.write_at(lba * ss as u64, data)?,
        }
        // Writing to an overlay means it now diverges from the base, so a clean
        // shutdown should fold it back. (No-op for an in-place base.)
        if self.diff_path.is_some() {
            self.dirty = true;
        }
        Ok(())
    }

    /// Hand the sparse overlay's presence bitmap to MAME without closing the
    /// file, so it is part of what the close commits. MAME keeps a CHD's hunk
    /// map and metadata index in memory until the file is closed.
    fn persist_pending(&mut self) -> io::Result<()> {
        if let HdBackend::Sparse(ov) = &mut self.backend {
            ov.persist_pending()?;
        }
        Ok(())
    }
}

impl Drop for ChdHd {
    fn drop(&mut self) {
        // The backend is about to close, which is what commits the hunk map — so
        // the only thing left to do is get the presence bitmap into the file
        // first. Losing it would make the hunks it covers read through to the
        // base. No close/reopen cycle here: dropping does the close anyway.
        if let Err(e) = self.persist_pending() {
            eprintln!(
                "iris: recording the COW overlay's state for {} failed: {} (hunks written \
                 as all-zero this session may read back from the base)",
                self.base_path.display(),
                e
            );
        }
    }
}

/// A sequential `Read` over the merged (parent + diff) sectors of an [`HdImage`],

/// A sequential `Read` over the merged (parent + diff) sectors of an [`HdImage`],
/// used to feed [`flatten_diff`]'s rebuild. Reads one sector at a time.
struct MergedReader {
    img: HdImage,
    sector_size: usize,
    sector_count: u64,
    next_lba: u64,
    buf: Vec<u8>,
    pos: usize, // bytes consumed from `buf`
    len: usize, // valid bytes in `buf`
}

impl MergedReader {
    fn new(img: HdImage, sector_size: usize, sector_count: u64) -> Self {
        Self { img, sector_size, sector_count, next_lba: 0, buf: vec![0u8; sector_size], pos: 0, len: 0 }
    }
}

impl Read for MergedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            if self.next_lba >= self.sector_count {
                return Ok(0); // EOF — every sector streamed
            }
            self.img.read_sector(self.next_lba, &mut self.buf).map_err(map_err)?;
            self.next_lba += 1;
            self.pos = 0;
            self.len = self.sector_size;
        }
        let n = (self.len - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Fold a `.diff.chd` back into its base CHD: rebuild the base from the merged
/// (parent + diff) view, preserving the base's codecs / geometry / hunk+unit
/// sizes (so a compressed base stays compressed), via a temp file + atomic
/// rename, then delete the diff.
///
/// Safety: the base is only ever replaced by an atomic rename of a fully-written,
/// fsynced temp file, and the diff is deleted only after that rename succeeds. On
/// any error or cancellation the base and diff are left exactly as they were, so
/// the next launch simply reattaches the diff — nothing is lost.
///
/// `progress(fraction)` receives 0.0..=1.0; `cancel()` aborts cleanly. The caller
/// MUST have dropped any open [`ChdHd`] for this base first (so the files are
/// closed) before calling this.
pub fn flatten_diff(
    base: &Path,
    diff: &Path,
    progress: &mut dyn FnMut(f32),
    cancel: &dyn Fn() -> bool,
) -> io::Result<()> {
    use libchdman_rs::hd::{create_from_reader, read_geometry, HdCreateOptions};
    use libchdman_rs::{Chd, CompressionProgress};

    let base_str = base.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 CHD path"))?;

    // Read the base's structure so the rebuilt CHD matches it byte-for-byte in
    // codecs/geometry (compressed stays compressed). Scope the handle so it's
    // closed before we rename over the base.
    let (codecs, hunk_bytes, unit_bytes, logical, geom) = {
        let bchd = Chd::open(base_str, false, None).map_err(map_err)?;
        let info = bchd.info().map_err(map_err)?;
        if !info.is_hd {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a hard-disk CHD"));
        }
        (info.codecs, info.hunk_bytes, info.unit_bytes, info.logical_bytes, read_geometry(&bchd).ok())
    };

    // Merged view: parent (base) with the diff applied. Its sectors are the
    // contents we rebuild the base from.
    let merged = HdImage::reopen_diff(base, diff).map_err(map_err)?;
    let sector_size = merged.sector_size() as usize;
    let sector_count = merged.sector_count();

    // Rebuild into a temp file next to the base (same filesystem → the rename is
    // atomic). The reader (and the merged HdImage it owns) is dropped when
    // create_from_reader returns, closing the base+diff handles before rename.
    let tmp = temp_sync_path_for(base);
    let opts = HdCreateOptions {
        logical_size: logical,
        hunk_size: hunk_bytes,
        unit_size: unit_bytes,
        codecs,
        geometry: geom,
        ident: None,
    };
    let reader = MergedReader::new(merged, sector_size, sector_count);
    let total = logical.max(1);
    let mut cb = |cp: CompressionProgress| {
        progress((cp.bytes_done as f64 / total as f64).min(1.0) as f32);
    };
    if let Err(e) = create_from_reader(reader, &tmp, opts, &mut cb, cancel) {
        let _ = std::fs::remove_file(&tmp); // base + diff untouched
        return Err(map_err(e));
    }

    // Durably replace the base, then drop the diff. The diff is removed only
    // after the rename, so an interruption anywhere above leaves base+diff intact.
    fsync_path(&tmp)?;
    std::fs::rename(&tmp, base)?;
    let _ = fsync_dir(base.parent());
    let _ = std::fs::remove_file(diff);
    progress(1.0);
    Ok(())
}

/// Temp path for the rebuilt CHD, alongside the base so the rename is atomic.
fn temp_sync_path_for(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".synctmp.chd");
    PathBuf::from(s)
}

/// Read-only CD CHD backend.
pub struct ChdCd {
    reader: CdCookedReader,
    total_bytes: u64,
}

impl ChdCd {
    pub fn open(path: &str) -> io::Result<Self> {
        let chd = Chd::open(path, false, None).map_err(map_err)?;
        let reader = CdCookedReader::open(chd).map_err(map_err)?;
        let total_bytes = reader.len();
        Ok(Self { reader, total_bytes })
    }

    pub fn size(&self) -> u64 {
        self.total_bytes
    }

    pub fn read_blocks(&mut self, lba: u64, count: usize, block_size: u64) -> io::Result<Vec<u8>> {
        let byte_offset = lba * block_size;
        let byte_count = (count as u64) * block_size;
        self.reader.seek(SeekFrom::Start(byte_offset))?;
        let mut buf = vec![0u8; byte_count as usize];
        self.reader.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// The `.diff.chd` sidecar path for a base CHD (honors `IRIS_CHD_DIFF_DIR`).
/// Public so the GUI can check for / locate a disk's overlay for commit/rollback.
pub fn diff_path_for(parent: &Path) -> PathBuf {
    // A compressed HD CHD can't be written in place, so writes go to an
    // uncompressed `.diff.chd` sidecar. By default it sits next to the parent.
    //
    // That fails under the macOS App Sandbox: the user grants access to the CHD
    // *file*, but creating a new sibling in its directory needs write access to
    // the directory, which the sandbox denies. iris-gui's App Store build sets
    // IRIS_CHD_DIFF_DIR to a writable container path; when present, put the diff
    // there, named by the parent's stem plus a hash of its full path so two
    // like-named CHDs in different folders don't collide.
    if let Some(dir) = std::env::var_os("IRIS_CHD_DIFF_DIR") {
        let dir = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        parent.hash(&mut h);
        let stem = parent.file_stem().and_then(|s| s.to_str()).unwrap_or("disk");
        return dir.join(format!("{stem}.{:016x}.diff.chd", h.finish()));
    }
    with_suffix(parent, ".diff.chd")
}

pub fn is_chd(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("chd"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libchdman_rs::hd::{create_from_reader, HdCreateOptions};
    use libchdman_rs::{Chd, CHD_CODEC_ZLIB};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    const HD_GDDD_TAG: u32 = u32::from_be_bytes(*b"GDDD");

    fn unique_base() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "iris_chd_{}_{}.chd",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Build a base CHD full of `fill`, compressed or not.
    fn make_base(logical: u64, fill: u8, compressed: bool) -> PathBuf {
        let base = unique_base();
        let _ = std::fs::remove_file(&base);
        let _ = std::fs::remove_file(diff_path_for(&base));
        create_from_reader(
            Cursor::new(vec![fill; logical as usize]),
            &base,
            HdCreateOptions {
                logical_size: logical,
                hunk_size: 4096,
                unit_size: 512,
                codecs: if compressed { [CHD_CODEC_ZLIB, 0, 0, 0] } else { [0, 0, 0, 0] },
                geometry: None,
                ident: None,
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();
        base
    }

    fn cleanup(base: &Path) {
        let d = diff_path_for(base);
        let _ = std::fs::remove_file(base);
        let _ = std::fs::remove_file(&d);
        let _ = std::fs::remove_file(with_suffix(&d, ".apply"));
        let _ = std::fs::remove_file(with_suffix(&d, ".creating"));
    }

    /// The library behaviour the sparse overlay exists to work around: an
    /// uncompressed CHD has an all-zero SHA-1, so MAME will not link a diff to
    /// it. If a future libchdman fixes this, the simpler parented path becomes
    /// available for uncompressed bases too and this test is the signal.
    #[test]
    fn uncompressed_base_cannot_be_a_chd_parent() {
        let base = make_base(1024 * 1024, 0xAA, false);
        let diff = unique_base();
        let _ = std::fs::remove_file(&diff);
        let parent = Chd::open(base.to_str().unwrap(), false, None).unwrap();
        assert_eq!(parent.info().unwrap().sha1, [0u8; 20], "uncompressed CHDs carry no SHA-1");
        {
            let mut child = Chd::create_with_parent(
                diff.to_str().unwrap(),
                parent.logical_bytes(),
                parent.hunk_bytes(),
                [0; 4],
                &parent,
            )
            .expect("creating the child succeeds");
            child.clone_all_metadata(&parent).unwrap();
        }
        assert!(
            Chd::open(diff.to_str().unwrap(), true, Some(&parent)).is_err(),
            "MAME must refuse a parent whose SHA-1 is zero — the sparse overlay's reason \
             to exist"
        );
        drop(parent);
        let _ = std::fs::remove_file(&diff);
        cleanup(&base);
    }

    /// COW on an uncompressed base works end to end: the base is untouched, the
    /// overlay stays small, and reads see the writes across a reopen.
    #[test]
    fn sparse_overlay_on_uncompressed_base() {
        let base = make_base(4 * 1024 * 1024, 0xAA, false);
        let base_before = std::fs::read(&base).unwrap();

        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert!(hd.is_cow() && hd.is_sparse_overlay());
        assert_eq!(hd.size(), 4 * 1024 * 1024);
        assert_eq!(hd.read_blocks(3, 1, 512).unwrap(), vec![0xAA; 512]);

        hd.write_sectors(3, &[0x5A; 512]).unwrap();
        // A partial-hunk write must leave the rest of the hunk as the base had it.
        assert_eq!(hd.read_blocks(3, 1, 512).unwrap(), vec![0x5A; 512]);
        assert_eq!(hd.read_blocks(2, 1, 512).unwrap(), vec![0xAA; 512], "neighbour intact");
        assert_eq!(hd.read_blocks(4, 1, 512).unwrap(), vec![0xAA; 512], "neighbour intact");
        // A multi-sector write spanning a hunk boundary.
        hd.write_sectors(6, &[0x77; 512 * 4]).unwrap();
        assert_eq!(hd.read_blocks(6, 4, 512).unwrap(), vec![0x77; 512 * 4]);
        assert!(hd.diff_dirty());
        assert!(hd.pending_sync().is_none(), "COW never auto-folds");
        drop(hd);

        assert_eq!(std::fs::read(&base).unwrap(), base_before, "base untouched by COW writes");
        let diff = diff_path_for(&base);
        let diff_len = std::fs::metadata(&diff).unwrap().len();
        assert!(diff_len < 256 * 1024, "overlay stays sparse, was {diff_len} bytes");

        // Reopen: the overlay's writes are still there.
        let mut hd2 = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(hd2.read_blocks(3, 1, 512).unwrap(), vec![0x5A; 512]);
        assert_eq!(hd2.read_blocks(6, 4, 512).unwrap(), vec![0x77; 512 * 4]);
        assert_eq!(hd2.read_blocks(0, 1, 512).unwrap(), vec![0xAA; 512]);
        drop(hd2);
        cleanup(&base);
    }

    /// The trap the presence bitmap exists for: MAME deallocates an all-zero
    /// hunk, so a guest zeroing a hunk whose base content was non-zero must not
    /// read back the base's old data.
    #[test]
    fn zeroing_a_hunk_does_not_fall_through_to_the_base() {
        let base = make_base(1024 * 1024, 0xAA, false);
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        // Zero a whole 4096-byte hunk (8 sectors), so the overlay hunk is all
        // zeros and MAME drops it from the map.
        hd.write_sectors(8, &[0x00; 4096]).unwrap();
        assert_eq!(hd.read_blocks(8, 8, 512).unwrap(), vec![0x00; 4096], "zeros in-session");
        drop(hd);

        let mut hd2 = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(
            hd2.read_blocks(8, 8, 512).unwrap(),
            vec![0x00; 4096],
            "a zeroed hunk must stay zeroed across a reopen, not revert to the base"
        );
        assert_eq!(hd2.read_blocks(16, 1, 512).unwrap(), vec![0xAA; 512], "untouched hunk");
        drop(hd2);
        cleanup(&base);
    }

    /// Commit folds the overlay into the base and leaves a clean slate.
    #[test]
    fn sparse_overlay_commit_writes_through_to_the_base() {
        let base = make_base(1024 * 1024, 0xAA, false);
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        hd.write_sectors(5, &[0x5A; 512]).unwrap();
        hd.write_sectors(8, &[0x00; 4096]).unwrap(); // the all-zero case too
        let (b, d) = hd.overlay_paths().unwrap();
        drop(hd);

        let applied = commit_sparse_overlay(&b, &d, &mut |_| {}).unwrap();
        assert!(applied >= 2, "both touched hunks folded in, got {applied}");
        assert!(!d.exists(), "overlay removed after commit");
        assert!(!with_suffix(&d, ".apply").exists(), "marker cleared");

        // Read the base directly — no overlay in play.
        let mut plain = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert!(!plain.is_sparse_overlay() && plain.overlay_paths().is_none());
        assert_eq!(plain.read_blocks(5, 1, 512).unwrap(), vec![0x5A; 512]);
        assert_eq!(plain.read_blocks(8, 8, 512).unwrap(), vec![0x00; 4096]);
        assert_eq!(plain.read_blocks(0, 1, 512).unwrap(), vec![0xAA; 512]);
        drop(plain);
        cleanup(&base);
    }

    /// An interrupted commit is finished by the next open rather than leaving
    /// the base half-folded.
    #[test]
    fn interrupted_commit_is_resumed_on_open() {
        let base = make_base(1024 * 1024, 0xAA, false);
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        hd.write_sectors(5, &[0x5A; 512]).unwrap();
        let (b, d) = hd.overlay_paths().unwrap();
        drop(hd);

        // Simulate a crash right after the marker went down: marker present,
        // overlay still there, base not yet touched.
        std::fs::write(with_suffix(&d, ".apply"), "base=?\noverlay=?\n").unwrap();
        assert_eq!(b, base);

        let mut hd2 = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert!(!d.exists(), "the resumed commit consumed the overlay");
        assert!(!with_suffix(&d, ".apply").exists(), "marker cleared");
        assert_eq!(
            hd2.read_blocks(5, 1, 512).unwrap(),
            vec![0x5A; 512],
            "the interrupted commit's write landed in the base"
        );
        drop(hd2);
        cleanup(&base);
    }

    /// A base that changed underneath its overlay is refused, not silently
    /// mixed with stale copied-up hunks.
    #[test]
    fn changed_base_is_refused() {
        let base = make_base(1024 * 1024, 0xAA, false);
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        hd.write_sectors(5, &[0x5A; 512]).unwrap();
        drop(hd);

        // Change the base behind the overlay's back. Appending rather than
        // rewriting a hunk in place so the length moves too — mtime alone has
        // one-second granularity on some filesystems and would make this flaky.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&base).unwrap();
            f.write_all(&[0u8; 8]).unwrap();
            f.sync_all().unwrap();
        }
        let Err(e) = ChdHd::open(base.to_str().unwrap(), true) else {
            panic!("a base that moved under its overlay must be refused");
        };
        assert!(
            e.to_string().contains("changed since its COW overlay"),
            "unexpected error: {e}"
        );
        cleanup(&base);
    }

    /// Writes outside the disk are refused outright, and refused *before*
    /// anything is written — a multi-sector write whose tail is out of range
    /// must not modify the sectors ahead of it.
    #[test]
    fn out_of_range_writes_are_refused_whole() {
        let base = make_base(1024 * 1024, 0xAA, false); // 2048 sectors
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert!(hd.write_sectors(2048, &[0x5A; 512]).is_err(), "first sector past the end");
        // Straddling the end: sector 2047 is valid, 2048 is not.
        assert!(hd.write_sectors(2047, &[0x5A; 1024]).is_err());
        assert_eq!(
            hd.read_blocks(2047, 1, 512).unwrap(),
            vec![0xAA; 512],
            "the in-range half of a rejected write must not have landed"
        );
        assert!(hd.read_blocks(2048, 1, 512).is_err(), "reads are bounded too");
        drop(hd);
        cleanup(&base);
    }

    /// The size that started this: a 240 GB uncompressed CHD, the geometry from
    /// the original report (`chdman createhd -ss 512 -chs 128,16,228882 -c none`).
    /// An overlay for it must stay in the hundreds of megabytes — that floor is
    /// the hunk map itself (58,593,792 hunks × 4 bytes), not copied data.
    ///
    /// Ignored by default: it writes ~470 MB of files. Run it with
    /// `cargo test --features chd -- --ignored sparse_overlay_on_a_240gb_disk`.
    #[test]
    #[ignore = "creates ~470 MB of CHDs"]
    fn sparse_overlay_on_a_240gb_disk() {
        use libchdman_rs::hd::{format_gddd, HdGeometry};
        let base = unique_base();
        let _ = std::fs::remove_file(diff_path_for(&base));
        let geom = HdGeometry { cylinders: 128, heads: 16, sectors: 228_882, sector_bytes: 512 };
        let logical = geom.logical_bytes();
        assert_eq!(logical, 240_000_172_032);
        // Build the blank base the way `chdman createhd -c none` does: allocate
        // the container and its hunk map, no data stream. Going through
        // `create_from_reader` here would push 240 GB of zeros through the
        // compressor for a disk that has nothing in it.
        {
            let mut c = Chd::create(base.to_str().unwrap(), logical, 4096, 512, [0; 4]).unwrap();
            c.write_metadata(HD_GDDD_TAG, 0, &format_gddd(geom), 1).unwrap();
        }

        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(hd.size(), logical);
        let last = logical / 512 - 1;
        assert_eq!(hd.read_blocks(last, 1, 512).unwrap(), vec![0x00; 512], "blank disk reads zero");
        hd.write_sectors(last, &[0x5A; 512]).unwrap();
        assert_eq!(hd.read_blocks(last, 1, 512).unwrap(), vec![0x5A; 512]);
        assert_eq!(hd.read_blocks(last - 1, 1, 512).unwrap(), vec![0x00; 512], "neighbour intact");
        drop(hd);

        let diff = diff_path_for(&base);
        let diff_len = std::fs::metadata(&diff).unwrap().len();
        assert!(
            diff_len < 300 * 1024 * 1024,
            "a 240 GB overlay must not be a copy of the base; it is {diff_len} bytes"
        );

        let mut hd2 = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(hd2.read_blocks(last, 1, 512).unwrap(), vec![0x5A; 512], "survives reopen");
        drop(hd2);
        cleanup(&base);
    }

    /// An empty diff left behind by an older build that could not link it to an
    /// uncompressed base is cleared, so one failed COW attempt does not make the
    /// disk permanently unopenable.
    #[test]
    fn unusable_legacy_diff_does_not_brick_the_disk() {
        let base = make_base(1024 * 1024, 0xAA, false);
        let diff = diff_path_for(&base);
        // Reproduce exactly what the old path left: a parented diff over an
        // uncompressed (zero-SHA-1) base.
        {
            let parent = Chd::open(base.to_str().unwrap(), false, None).unwrap();
            let mut child = Chd::create_with_parent(
                diff.to_str().unwrap(),
                parent.logical_bytes(),
                parent.hunk_bytes(),
                [0; 4],
                &parent,
            )
            .unwrap();
            child.clone_all_metadata(&parent).unwrap();
        }
        assert!(diff.exists());
        let mut hd = ChdHd::open(base.to_str().unwrap(), false).expect("disk still opens");
        assert_eq!(hd.read_blocks(0, 1, 512).unwrap(), vec![0xAA; 512]);
        drop(hd);
        cleanup(&base);
    }

    /// End-to-end: a compressed base gets a write via its parented diff, and
    /// flatten folds the write back into the (still-compressed) base, removing
    /// the diff.
    #[test]
    fn flatten_folds_diff_into_compressed_base() {
        let base = make_base(256 * 1024, 0xAB, true);
        let mut hd = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert!(hd.pending_sync().is_none(), "fresh diff, nothing written → not pending");
        hd.write_sectors(2, &[0x5A; 512]).unwrap();
        let (b, d) = hd.pending_sync().expect("a write makes it pending");
        assert_eq!(b, base);
        assert!(d.exists(), "diff sidecar exists");
        drop(hd);

        let mut last = 0.0f32;
        flatten_diff(&b, &d, &mut |f| last = f, &|| false).unwrap();
        assert_eq!(last, 1.0, "progress reaches 100%");
        assert!(!d.exists(), "diff removed after a successful flatten");

        {
            let chd = Chd::open(base.to_str().unwrap(), false, None).unwrap();
            assert!(chd.info().unwrap().compressed, "base is still a compressed CHD");
            chd.verify().expect("flattened base verifies");
        }
        let mut hd2 = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert_eq!(hd2.read_blocks(2, 1, 512).unwrap(), vec![0x5A; 512], "the write folded in");
        assert_eq!(hd2.read_blocks(0, 1, 512).unwrap(), vec![0xAB; 512], "rest preserved");
        drop(hd2);
        cleanup(&base);
    }

    /// COW keeps changes in the overlay (no auto-fold), the same diff DOES
    /// auto-fold when COW is off, and a rollback restores the base.
    #[test]
    fn cow_keeps_changes_separate_and_rolls_back() {
        let base = make_base(128 * 1024, 0xCC, true);
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert!(hd.is_cow());
        assert!(hd.overlay_paths().is_some());
        hd.write_sectors(1, &[0x33; 512]).unwrap();
        assert!(hd.diff_dirty());
        assert!(hd.pending_sync().is_none(), "COW keeps changes separate");
        drop(hd);

        let mut hd_off = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert!(hd_off.pending_sync().is_some(), "COW off: the diff auto-folds on a clean exit");
        assert_eq!(hd_off.read_blocks(1, 1, 512).unwrap(), vec![0x33; 512]);
        drop(hd_off);

        std::fs::remove_file(diff_path_for(&base)).unwrap();
        let mut hd_rb = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(hd_rb.read_blocks(1, 1, 512).unwrap(), vec![0xCC; 512], "rollback restored");
        drop(hd_rb);
        cleanup(&base);
    }

    /// A cancelled flatten leaves the base and diff intact.
    #[test]
    fn cancelled_flatten_preserves_base_and_diff() {
        let base = make_base(128 * 1024, 0x11, true);
        let mut hd = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        hd.write_sectors(1, &[0x22; 512]).unwrap();
        let (b, d) = hd.pending_sync().unwrap();
        drop(hd);

        let err = flatten_diff(&b, &d, &mut |_| {}, &|| true).unwrap_err();
        let _ = err; // cancellation surfaces as an error
        assert!(b.exists(), "base intact after cancel");
        assert!(d.exists(), "diff intact after cancel");
        assert!(!temp_sync_path_for(&b).exists(), "no temp left behind");
        cleanup(&base);
    }

    /// The `IRCW` record round-trips, and a truncated or mislabelled one is
    /// rejected rather than half-read.
    #[test]
    fn overlay_header_round_trips_and_rejects_damage() {
        let h = OverlayHeader {
            base_logical: 240_000_172_032,
            base_hunk_bytes: 4096,
            base_unit_bytes: 512,
            base_len: 234_375_292,
            base_mtime_secs: 1_789_000_000,
            base_mtime_nanos: 123_456_789,
            zero_hunks: vec![0x01, 0x80, 0x00, 0xFF],
        };
        let raw = h.encode();
        assert_eq!(raw.len(), BITMAP_OFFSET + 4);
        let back = OverlayHeader::decode(&raw).unwrap();
        assert_eq!(back.base_logical, h.base_logical);
        assert_eq!(back.base_hunk_bytes, h.base_hunk_bytes);
        assert_eq!(back.base_unit_bytes, h.base_unit_bytes);
        assert_eq!(back.base_len, h.base_len);
        assert_eq!(back.base_mtime_secs, h.base_mtime_secs);
        assert_eq!(back.base_mtime_nanos, h.base_mtime_nanos);
        assert_eq!(back.zero_hunks, h.zero_hunks);

        assert!(OverlayHeader::decode(&raw[..BITMAP_OFFSET - 1]).is_err(), "too short");
        let mut bad_magic = raw.clone();
        bad_magic[0] ^= 0xFF;
        assert!(OverlayHeader::decode(&bad_magic).is_err(), "wrong magic");
        let mut bad_len = raw.clone();
        bad_len[44] = 99;
        assert!(OverlayHeader::decode(&bad_len).is_err(), "bitmap length mismatch");
    }
}
