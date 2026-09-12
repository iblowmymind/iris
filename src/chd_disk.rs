//! CHD-backed disk implementations for the SCSI subsystem.
//!
//! Two flavors:
//!   * [`ChdHd`] — hard-disk CHD as a writable block device. Uncompressed CHDs
//!     are written in place; compressed CHDs get an uncompressed `.diff.chd`
//!     sidecar so the parent stays untouched (MAME's strategy).
//!   * [`ChdCd`] — single-track MODE1 CD CHD exposed as a 2048-byte/sector
//!     read-only stream via libchdman-rs's `CdCookedReader`.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use libchdman_rs::cd::CdCookedReader;
use libchdman_rs::hd::HdImage;
use libchdman_rs::Chd;

// A standalone COW copy is necessary for uncompressed parents with no SHA-1.
// MAME cannot represent a parent relationship with an all-zero parent hash.
const STANDALONE_PARENT_TAG: u32 = u32::from_be_bytes(*b"IRIS");

fn file_hash(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().as_bytes().to_vec())
}

fn open_merged(base: &Path, diff: &Path) -> io::Result<HdImage> {
    let probe = Chd::open(diff.to_str().ok_or_else(||
        io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 CHD path"))?, false, None);
    if let Ok(chd) = probe {
        if chd.parent_sha1() == [0; 20] {
            let expected = chd.read_metadata(STANDALONE_PARENT_TAG, 0).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData,
                    "CHD overlay has no parent identity; preserve it and recreate the overlay from its base")
            })?;
            if expected != file_hash(base)? {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "CHD overlay base has changed"));
            }
            drop(chd);
            return HdImage::open(diff).map_err(map_err);
        }
    }
    HdImage::reopen_diff(base, diff).map_err(map_err)
}

fn create_overlay(base: &Path, diff: &Path) -> io::Result<HdImage> {
    let parent = Chd::open(base.to_str().ok_or_else(||
        io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 CHD path"))?, false, None).map_err(map_err)?;
    if parent.sha1() != [0; 20] {
        drop(parent);
        let tmp = temp_sync_path_for(diff);
        let result = (|| {
            let image = HdImage::open_with_diff(base, &tmp).map_err(map_err)?;
            drop(image);
            fsync_path(&tmp)?;
            std::fs::rename(&tmp, diff)?;
            open_merged(base, diff)
        })();
        if result.is_err() { let _ = std::fs::remove_file(&tmp); }
        return result;
    }
    drop(parent);
    // Copy the CHD container, not its logical sectors: unallocated hunks stay
    // unallocated even for a disk whose virtual capacity is hundreds of GB.
    let hash = file_hash(base)?;
    let tmp = temp_sync_path_for(diff);
    let result = (|| {
        std::fs::copy(base, &tmp)?;
        let mut copy = HdImage::open(&tmp).map_err(map_err)?;
        copy.as_chd_mut().write_metadata(STANDALONE_PARENT_TAG, 0, &hash, 0).map_err(map_err)?;
        drop(copy);
        fsync_path(&tmp)?;
        std::fs::rename(&tmp, diff)?;
        open_merged(base, diff)
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result
}

fn map_err<E: std::fmt::Debug>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{:?}", e))
}

/// Writable hard-disk CHD backend.
pub struct ChdHd {
    img: HdImage,
    sector_size: u32,
    total_bytes: u64,
    /// The base CHD path (the file the user configured).
    base_path: PathBuf,
    /// The `.diff.chd` sidecar, when writes go to a diff (compressed base, or a
    /// COW-style overlay). `None` when writing the base in place (uncompressed).
    diff_path: Option<PathBuf>,
    /// Whether the diff holds changes worth folding back into the base on a
    /// clean shutdown — either we wrote this session, or we reattached a diff
    /// that already existed (carrying changes from a previous session).
    dirty: bool,
    /// Copy-on-write requested for this disk (the per-disk `overlay`/COW flag).
    /// When set we ALWAYS overlay (even an uncompressed base gets a diff, so the
    /// base is never written in-session) and we NEVER auto-fold on exit — the
    /// user commits or rolls back deliberately via `cow commit` / `cow reset`.
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
        recover_sync(p, &diff)?;

        // Cases:
        //  - a diff already exists → reattach to it; it carries changes from a
        //    previous session, so mark dirty so a (non-COW) clean exit folds it.
        //  - COW on → always overlay, even an uncompressed base, so the base is
        //    never written in-session (protect + rollback).
        //  - no diff, base opens writable in place (uncompressed, COW off) → no diff.
        //  - no diff, base won't open writable (compressed) → create a fresh diff.
        let (img, diff_path, dirty) = if diff.exists() {
            (open_merged(p, &diff)?, Some(diff), true)
        } else if cow {
            (create_overlay(p, &diff)?, Some(diff), false)
        } else {
            match HdImage::open(p) {
                Ok(img) => (img, None, false),
                Err(_) => (create_overlay(p, &diff)?, Some(diff), false),
            }
        };

        let sector_size = img.sector_size();
        if sector_size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "CHD has zero-byte sectors"));
        }
        let total_bytes = img.sector_count() * u64::from(sector_size);
        Ok(Self { img, sector_size, total_bytes, base_path: p.to_path_buf(), diff_path, dirty, cow })
    }

    /// `(base, diff)` paths if a clean exit should **auto-fold** this disk's diff
    /// back into the base — i.e. it has diff-borne changes AND COW is off (COW on
    /// means "keep separate"; commit/rollback are then manual). `None` otherwise.
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

    /// `(base, diff)` when writes are landing in a `.diff.chd` overlay (regardless
    /// of the COW flag — a compressed base always overlays). Used by commit/reset.
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

    pub fn flush(&mut self) -> io::Result<()> {
        // MAME writes hunks and map entries through to the OS before returning;
        // SYNCHRONIZE CACHE must additionally make those writes durable.
        let path = self.diff_path.as_deref().unwrap_or(&self.base_path);
        fsync_path(path)?;
        fsync_dir(path.parent())
    }

    pub fn read_blocks(&mut self, lba: u64, count: usize, block_size: u64) -> io::Result<Vec<u8>> {
        let ss = u64::from(self.sector_size);
        if block_size != ss {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("CHD HD sector size {} != requested block size {}", ss, block_size),
            ));
        }
        self.check_range(lba, count)?;
        let bytes = count.checked_mul(ss as usize).ok_or_else(||
            io::Error::new(io::ErrorKind::InvalidInput, "CHD read length overflow"))?;
        let mut buf = vec![0u8; bytes];
        for i in 0..count {
            let off = i * ss as usize;
            self.img
                .read_sector(lba + i as u64, &mut buf[off..off + ss as usize])
                .map_err(map_err)?;
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
        self.check_range(lba, count)?;
        // A failing multi-sector write may already have changed the diff.
        if count != 0 && self.diff_path.is_some() {
            self.dirty = true;
        }
        for i in 0..count {
            let off = i * ss;
            self.img
                .write_sector(lba + i as u64, &data[off..off + ss])
                .map_err(map_err)?;
        }
        // Writing to a diff means the sidecar now diverges from the base, so a
        // clean shutdown should fold it back. (No-op for an in-place base.)
        if self.diff_path.is_some() {
            self.dirty = true;
        }
        Ok(())
    }

    fn check_range(&self, lba: u64, count: usize) -> io::Result<()> {
        let sectors = self.total_bytes / u64::from(self.sector_size);
        if lba > sectors || count as u64 > sectors - lba {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "CHD request exceeds disk capacity"));
        }
        Ok(())
    }
}

/// The compressor treats read errors and premature EOF as zero padding. Keep
/// an independent error channel so a damaged source can never replace a disk.
struct CheckedReader<R> {
    inner: R,
    remaining: u64,
    error: std::sync::Arc<std::sync::Mutex<Option<io::Error>>>,
}

impl<R: Read> Read for CheckedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.remaining == 0 { return Ok(0); }
        let len = out.len().min(self.remaining.min(usize::MAX as u64) as usize);
        let result = loop {
            match self.inner.read(&mut out[..len]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(0) => break Err(io::Error::new(io::ErrorKind::UnexpectedEof, "CHD source ended during merge")),
                other => break other,
            }
        };
        match result {
            Ok(n) => { self.remaining -= n as u64; Ok(n) }
            Err(e) => {
                let mut error = self.error.lock().unwrap();
                if error.is_none() { *error = Some(io::Error::new(e.kind(), e.to_string())); }
                Err(e)
            }
        }
    }
}

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
/// The base is replaced only after the output has been checked and synced.
/// A durable journal identifies an installed replacement if shutdown interrupts
/// removal of its old diff. Opening the disk completes that cleanup before use.
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
    recover_sync(base, diff)?;
    if cancel() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "CHD merge cancelled"));
    }

    let merged = open_merged(base, diff)?;
    if merged.as_chd().read_metadata(STANDALONE_PARENT_TAG, 0).is_ok() {
        drop(merged);
        let tmp = temp_sync_path_for(base);
        let result = (|| {
            std::fs::copy(diff, &tmp)?;
            let mut copy = HdImage::open(&tmp).map_err(map_err)?;
            copy.as_chd_mut().delete_metadata(STANDALONE_PARENT_TAG, 0).map_err(map_err)?;
            drop(copy);
            if cancel() { return Err(io::Error::new(io::ErrorKind::Interrupted, "CHD merge cancelled")); }
            install_rebuilt(base, diff, &tmp)
        })();
        let _ = std::fs::remove_file(&tmp);
        result?;
        progress(1.0);
        return Ok(());
    }
    drop(merged);

    // Read the base's structure so the rebuilt CHD matches it byte-for-byte in
    // codecs/geometry (compressed stays compressed). Scope the handle so it's
    // closed before we rename over the base.
    let (codecs, hunk_bytes, unit_bytes, logical, geom, ident) = {
        let bchd = Chd::open(base_str, false, None).map_err(map_err)?;
        let info = bchd.info().map_err(map_err)?;
        if !info.is_hd {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a hard-disk CHD"));
        }
        // The high-level compressor only preserves GDDD and IDNT. Refuse to
        // silently drop any other metadata until it can be copied faithfully.
        let gddd = u32::from_be_bytes(*b"GDDD");
        let idnt = u32::from_be_bytes(*b"IDNT");
        if info.metadata_tags.iter().any(|&(tag, index)| (tag != gddd && tag != idnt) || index != 0) {
            return Err(io::Error::new(io::ErrorKind::Unsupported,
                "CHD merge cannot preserve this image's additional metadata; keeping its overlay"));
        }
        let ident = if info.metadata_tags.contains(&(idnt, 0)) {
            Some(bchd.read_metadata(idnt, 0).map_err(map_err)?)
        } else { None };
        (info.codecs, info.hunk_bytes, info.unit_bytes, info.logical_bytes,
            Some(read_geometry(&bchd).map_err(map_err)?), ident)
    };

    // Merged view: parent (base) with the diff applied. Its sectors are the
    // contents we rebuild the base from.
    let merged = open_merged(base, diff)?;
    let sector_size = merged.sector_size() as usize;
    if sector_size == 0 || !logical.is_multiple_of(sector_size as u64) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid CHD sector geometry"));
    }
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
        ident,
    };
    let error = std::sync::Arc::new(std::sync::Mutex::new(None));
    let reader = CheckedReader {
        inner: MergedReader::new(merged, sector_size, sector_count),
        remaining: logical,
        error: error.clone(),
    };
    let total = logical.max(1);
    let mut cb = |cp: CompressionProgress| {
        progress((cp.bytes_done as f64 / total as f64).min(1.0) as f32);
    };
    let result = create_from_reader(reader, &tmp, opts, &mut cb, cancel).map_err(map_err);
    let result = match error.lock().unwrap().take() {
        Some(e) => Err(e),
        None => result,
    };
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp); // base + diff untouched
        return Err(e);
    }

    // Durably replace the base, then drop the diff. The diff is removed only
    // after the rename, so an interruption anywhere above leaves base+diff intact.
    install_rebuilt(base, diff, &tmp)?;
    progress(1.0);
    Ok(())
}

/// Temp path for the rebuilt CHD, alongside the base so the rename is atomic.
fn temp_sync_path_for(base: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".{}.{}.synctmp.chd", std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    PathBuf::from(s)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SyncJournal {
    original: Vec<u8>,
    replacement: Vec<u8>,
    diff: Vec<u8>,
}

fn journal_path(base: &Path) -> PathBuf {
    let mut path = base.as_os_str().to_owned();
    path.push(".sync.json");
    PathBuf::from(path)
}

fn recover_sync(base: &Path, diff: &Path) -> io::Result<()> {
    let path = journal_path(base);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let journal: SyncJournal = serde_json::from_reader(file).map_err(|e|
        io::Error::new(io::ErrorKind::InvalidData, format!("Invalid CHD merge journal: {e}")))?;
    let current = file_hash(base)?;
    if current == journal.replacement {
        match file_hash(diff) {
            Ok(hash) if hash == journal.diff => {
                std::fs::remove_file(diff)?;
                fsync_dir(diff.parent())?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData,
                "CHD diff changed during interrupted merge; preserve base and diff for recovery")),
        }
    } else if current != journal.original {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            "CHD base changed during interrupted merge; preserve base and diff for recovery"));
    }
    std::fs::remove_file(path)?;
    fsync_dir(base.parent())
}

fn install_rebuilt(base: &Path, diff: &Path, tmp: &Path) -> io::Result<()> {
    use std::io::Write;
    fsync_path(tmp)?;
    let journal = SyncJournal {
        original: file_hash(base)?, replacement: file_hash(tmp)?, diff: file_hash(diff)?,
    };
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(journal_path(base))?;
    file.write_all(&serde_json::to_vec(&journal).map_err(map_err)?)?;
    file.sync_all()?;
    drop(file);
    fsync_dir(base.parent())?;
    std::fs::rename(tmp, base)?;
    fsync_dir(base.parent())?;
    recover_sync(base, diff)
}

fn fsync_path(p: &Path) -> io::Result<()> {
    std::fs::OpenOptions::new().read(true).write(true).open(p)?.sync_all()
}

fn fsync_dir(dir: Option<&Path>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(d) = dir {
        let d = if d.as_os_str().is_empty() { Path::new(".") } else { d };
        std::fs::File::open(d)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
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
    let mut s = parent.as_os_str().to_owned();
    s.push(".diff.chd");
    PathBuf::from(s)
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

    fn unique_base() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "iris_flatten_{}_{}.chd",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn sparse_disk_round_trip() {
        for cow in [false, true] {
            let base = unique_base();
            let mut chd = Chd::create(base.to_str().unwrap(), 131072, 4096, 512, [0; 4]).unwrap();
            chd.write_metadata(u32::from_be_bytes(*b"GDDD"), 0,
                b"CYLS:4,HEADS:4,SECS:16,BPS:512\0", 1).unwrap();
            drop(chd);
            let mut hd = ChdHd::open(base.to_str().unwrap(), cow).unwrap();
            assert_eq!(hd.size(), 131072);
            assert_eq!(hd.read_blocks(0, 8, 512).unwrap(), vec![0; 4096]);
            hd.write_sectors(1, &[0x5a; 512]).unwrap();
            hd.write_sectors(255, &[0xa5; 512]).unwrap();
            drop(hd);
            let mut hd = ChdHd::open(base.to_str().unwrap(), cow).unwrap();
            assert_eq!(hd.read_blocks(0, 1, 512).unwrap(), vec![0; 512]);
            assert_eq!(hd.read_blocks(1, 1, 512).unwrap(), vec![0x5a; 512]);
            assert_eq!(hd.read_blocks(2, 1, 512).unwrap(), vec![0; 512]);
            assert_eq!(hd.read_blocks(255, 1, 512).unwrap(), vec![0xa5; 512]);
            assert!(hd.write_sectors(255, &[0xff; 1024]).is_err());
            assert_eq!(hd.read_blocks(255, 1, 512).unwrap(), vec![0xa5; 512]);
            drop(hd);
            if cow {
                flatten_diff(&base, &diff_path_for(&base), &mut |_| {}, &|| false).unwrap();
                assert!(std::fs::metadata(&base).unwrap().len() < 32768);
                let mut hd = ChdHd::open(base.to_str().unwrap(), false).unwrap();
                assert_eq!(hd.read_blocks(1, 1, 512).unwrap(), vec![0x5a; 512]);
                assert_eq!(hd.read_blocks(255, 1, 512).unwrap(), vec![0xa5; 512]);
            }
            let _ = std::fs::remove_file(diff_path_for(&base));
            std::fs::remove_file(base).unwrap();
        }
    }

    #[test]
    fn compression_source_errors_are_latched() {
        for fail in [false, true] {
            struct Broken(bool);
            impl Read for Broken {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    if self.0 { Err(io::Error::other("injected disk read failure")) } else { Ok(0) }
                }
            }
            let error = std::sync::Arc::new(std::sync::Mutex::new(None));
            let reader = CheckedReader { inner: Broken(fail), remaining: 131072, error: error.clone() };
            let tmp = unique_base();
            // Upstream reports success even for an unreadable source.
            create_from_reader(reader, &tmp, HdCreateOptions {
                logical_size: 131072, ..Default::default()
            }, &mut |_| {}, &|| false).unwrap();
            let kind = error.lock().unwrap().as_ref().unwrap().kind();
            assert_eq!(kind, if fail { io::ErrorKind::Other } else { io::ErrorKind::UnexpectedEof });
            std::fs::remove_file(tmp).unwrap();
        }
    }

    #[test]
    fn interrupted_merge_finishes_cleanup() {
        let base = unique_base();
        let diff = diff_path_for(&base);
        std::fs::write(&base, b"new disk").unwrap();
        std::fs::write(&diff, b"old diff").unwrap();
        let journal = SyncJournal { original: vec![0; 32], replacement: file_hash(&base).unwrap(), diff: file_hash(&diff).unwrap() };
        std::fs::write(journal_path(&base), serde_json::to_vec(&journal).unwrap()).unwrap();
        recover_sync(&base, &diff).unwrap();
        assert!(!diff.exists());
        assert!(!journal_path(&base).exists());
        assert_eq!(std::fs::read(&base).unwrap(), b"new disk");
        std::fs::remove_file(base).unwrap();
    }

    #[test]
    #[ignore = "requires chdman; creates a 240 GB logical disk with a 234 MB hunk map"]
    fn chdman_sparse_large_disk() {
        let base = unique_base();
        let status = std::process::Command::new("chdman")
            .args(["createhd", "-ss", "512", "-chs", "128,16,228882", "-c", "none", "-o"])
            .arg(&base).status().unwrap();
        assert!(status.success());
        let bytes = 128_u64 * 16 * 228882 * 512;
        let original = file_hash(&base).unwrap();
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(hd.size(), bytes);
        assert_eq!(hd.read_blocks(0, 1, 512).unwrap(), vec![0; 512]);
        hd.write_sectors(bytes / 512 - 1, &[0xa5; 512]).unwrap();
        drop(hd);
        assert_eq!(file_hash(&base).unwrap(), original);
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(hd.read_blocks(bytes / 512 - 1, 1, 512).unwrap(), vec![0xa5; 512]);
        drop(hd);
        flatten_diff(&base, &diff_path_for(&base), &mut |_| {}, &|| false).unwrap();
        assert!(std::fs::metadata(&base).unwrap().len() < 300_000_000);
        let mut hd = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert_eq!(hd.read_blocks(bytes / 512 - 1, 1, 512).unwrap(), vec![0xa5; 512]);
        drop(hd);
        std::fs::remove_file(base).unwrap();
    }

    /// End-to-end: a compressed base gets a write via its diff, and flatten folds
    /// the write back into the (still-compressed) base, removing the diff.
    #[test]
    fn flatten_folds_diff_into_compressed_base() {
        let base = unique_base();
        let logical = 256 * 1024u64; // 512 sectors of 512 bytes
        // Build a COMPRESSED base full of 0xAB.
        let src = vec![0xABu8; logical as usize];
        create_from_reader(
            Cursor::new(src),
            &base,
            HdCreateOptions {
                logical_size: logical,
                hunk_size: 4096,
                unit_size: 512,
                codecs: [CHD_CODEC_ZLIB, 0, 0, 0],
                geometry: None,
                ident: None,
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();

        // Open via ChdHd: compressed → can't write in place → diff is created.
        let mut hd = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert!(hd.pending_sync().is_none(), "fresh diff, nothing written → not pending");
        hd.write_sectors(2, &[0x5Au8; 512]).unwrap();
        let (b, d) = hd.pending_sync().expect("a write makes it pending");
        assert_eq!(b, base);
        assert!(d.exists(), "diff sidecar exists");
        drop(hd); // close the CHD before flattening

        // Flatten: fold the diff back into the base.
        let mut last = 0.0f32;
        flatten_diff(&b, &d, &mut |f| last = f, &|| false).unwrap();
        assert_eq!(last, 1.0, "progress reaches 100%");
        assert!(!d.exists(), "diff removed after a successful flatten");

        // Base now carries the write, is still readable, and still compressed.
        {
            let chd = Chd::open(base.to_str().unwrap(), false, None).unwrap();
            assert!(chd.info().unwrap().compressed, "base is still a compressed CHD");
            chd.verify().expect("flattened base verifies");
        }
        // Reopen as a disk (compressed → fresh empty diff) and read the sectors.
        let mut hd2 = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert_eq!(hd2.read_blocks(2, 1, 512).unwrap(), vec![0x5A; 512], "the written sector folded in");
        assert_eq!(hd2.read_blocks(0, 1, 512).unwrap(), vec![0xAB; 512], "untouched sectors preserved");
        drop(hd2);

        // Cleanup.
        let _ = std::fs::remove_file(&base);
        let _ = std::fs::remove_file(diff_path_for(&base));
    }

    /// COW keeps changes in the overlay (no auto-fold), the same diff DOES auto-
    /// fold when COW is off, and a rollback (discard the diff) restores the base.
    #[test]
    fn cow_keeps_changes_separate_and_rolls_back() {
        let base = unique_base();
        let logical = 128 * 1024u64;
        create_from_reader(
            Cursor::new(vec![0xCCu8; logical as usize]),
            &base,
            HdCreateOptions {
                logical_size: logical,
                hunk_size: 4096,
                unit_size: 512,
                codecs: [CHD_CODEC_ZLIB, 0, 0, 0],
                geometry: None,
                ident: None,
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();

        // COW on: writes land in the overlay and are NOT pending an auto-fold.
        let mut hd = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert!(hd.is_cow());
        assert!(hd.overlay_paths().is_some());
        hd.write_sectors(1, &[0x33u8; 512]).unwrap();
        assert!(hd.diff_dirty());
        assert!(hd.pending_sync().is_none(), "COW keeps changes separate — never auto-folds");
        drop(hd);

        // Reopen COW off: the same dirty diff IS now pending an exit fold, and the
        // write is visible through the overlay.
        let mut hd_off = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        assert!(hd_off.pending_sync().is_some(), "COW off: the diff auto-folds on a clean exit");
        assert_eq!(hd_off.read_blocks(1, 1, 512).unwrap(), vec![0x33; 512], "write visible via the overlay");
        drop(hd_off);

        // Roll back: discard the diff. A fresh overlay over the untouched base
        // reads the original content again.
        std::fs::remove_file(diff_path_for(&base)).unwrap();
        let mut hd_rb = ChdHd::open(base.to_str().unwrap(), true).unwrap();
        assert_eq!(hd_rb.read_blocks(1, 1, 512).unwrap(), vec![0xCC; 512], "rollback restored the base content");
        drop(hd_rb);

        let _ = std::fs::remove_file(&base);
        let _ = std::fs::remove_file(diff_path_for(&base));
    }

    /// A cancelled flatten leaves the base and diff intact.
    #[test]
    fn cancelled_flatten_preserves_base_and_diff() {
        let base = unique_base();
        let logical = 128 * 1024u64;
        create_from_reader(
            Cursor::new(vec![0x11u8; logical as usize]),
            &base,
            HdCreateOptions {
                logical_size: logical,
                hunk_size: 4096,
                unit_size: 512,
                codecs: [CHD_CODEC_ZLIB, 0, 0, 0],
                geometry: None,
                ident: None,
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();
        let mut hd = ChdHd::open(base.to_str().unwrap(), false).unwrap();
        hd.write_sectors(1, &[0x22u8; 512]).unwrap();
        let (b, d) = hd.pending_sync().unwrap();
        drop(hd);

        let err = flatten_diff(&b, &d, &mut |_| {}, &|| true).unwrap_err();
        let _ = err; // cancellation surfaces as an error
        assert!(b.exists(), "base intact after cancel");
        assert!(d.exists(), "diff intact after cancel");
        assert!(!temp_sync_path_for(&b).exists(), "no temp left behind");

        let _ = std::fs::remove_file(&base);
        let _ = std::fs::remove_file(&d);
    }
}
