//! Copy-on-write disk overlay for SCSI disk images.
//!
//! Protects the base disk image from writes by redirecting them to a sparse
//! overlay file. Reads check the overlay first, falling back to the base image
//! for clean sectors. Deleting the overlay file resets the disk to its original state.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SECTOR_SIZE: u64 = 512;

/// Clone `src` to `dst` via filesystem-level CoW (APFS clonefile, Linux
/// FICLONE) when supported; fall back to a regular byte copy otherwise. On a
/// reflink-capable filesystem this is metadata-only — sub-millisecond for any
/// size — which makes per-snapshot overlay capture essentially free.
fn reflink_or_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let _ = std::fs::remove_file(dst);
    if try_reflink(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

#[cfg(target_os = "macos")]
fn try_reflink(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src_c = CString::new(src.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "linux")]
fn try_reflink(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // FICLONE = _IOW(0x94, 9, int); see linux/fs.h.
    const FICLONE: libc::c_ulong = 0x40049409;
    let src_f = File::open(src)?;
    let dst_f = OpenOptions::new().write(true).create(true).truncate(true).open(dst)?;
    let rc = unsafe { libc::ioctl(dst_f.as_raw_fd(), FICLONE, src_f.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        let _ = std::fs::remove_file(dst);
        Err(err)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn try_reflink(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "reflink not supported on this OS"))
}

/// Sidecar file holding the dirty sector list. Written next to the overlay
/// (e.g. `foo.overlay.dirty`). Format: binary, `u64` little-endian count
/// followed by that many `u64` sector LBAs, also LE. Compact enough that
/// flushing it on shutdown or on a periodic schedule is cheap.
fn dirty_sidecar_path(overlay_path: &str) -> PathBuf {
    PathBuf::from(format!("{}.dirty", overlay_path))
}

/// Read the dirty list, rejecting one that cannot be what we wrote.
///
/// `max_sector` is the disk's sector count: an entry beyond it would send reads
/// to an offset the base does not have, and `commit` would extend the base
/// writing it back. The header count is checked against the file's actual length
/// before it is used to reserve anything — a truncated or corrupt sidecar
/// claiming `u64::MAX` entries would otherwise abort the process on the spot.
fn load_dirty_sidecar(path: &Path, max_sector: u64) -> io::Result<HashSet<u64>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let mut f = File::open(path)?;
    let file_len = f.metadata()?.len();
    let mut count_buf = [0u8; 8];
    if f.read_exact(&mut count_buf).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: dirty list is too short to hold its own header", path.display()),
        ));
    }
    let count = u64::from_le_bytes(count_buf);
    // `saturating_sub` because the file could in principle be shortened between
    // the `metadata` call and the read above.
    let available = file_len.saturating_sub(8) / 8;
    if count != available {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: dirty list claims {} sectors but the file holds {}",
                path.display(),
                count,
                available
            ),
        ));
    }
    let mut set = HashSet::with_capacity(count as usize);
    let mut buf = [0u8; 8];
    for _ in 0..count {
        f.read_exact(&mut buf)?;
        let lba = u64::from_le_bytes(buf);
        if lba >= max_sector {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: dirty list names sector {} on a {}-sector disk",
                    path.display(),
                    lba,
                    max_sector
                ),
            ));
        }
        set.insert(lba);
    }
    Ok(set)
}

fn save_dirty_sidecar(path: &Path, dirty: &HashSet<u64>) -> io::Result<()> {
    // Write atomically: write to a temp file then rename.
    let tmp = path.with_extension("dirty.tmp");
    {
        let mut f = File::create(&tmp)?;
        let count = dirty.len() as u64;
        f.write_all(&count.to_le_bytes())?;
        for &s in dirty {
            f.write_all(&s.to_le_bytes())?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub struct CowDisk {
    base: File,
    overlay: File,
    dirty: HashSet<u64>,
    base_size: u64,
    overlay_path: String,
    /// The base image's path as configured. Kept rather than re-derived from
    /// `overlay_path`: `commit` used to reconstruct it by stripping a
    /// `.overlay` suffix, which fails outright for any overlay named otherwise
    /// and would target the wrong file for one that merely ends that way.
    base_path: String,
}

impl CowDisk {
    /// Open a COW disk with the given base image (read-only) and overlay file (read-write).
    /// If the overlay file exists, its dirty sectors are reconstructed from its sparse extent.
    /// If it doesn't exist, a new empty overlay is created.
    pub fn new(base_path: &str, overlay_path: &str) -> io::Result<Self> {
        let base = File::open(base_path)?;
        let base_size = base.metadata()?.len();

        let overlay = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(overlay_path)?;

        // Recover the dirty set from our sidecar file (written on flush /
        // shutdown by previous runs). If the sidecar is missing we start
        // empty — any prior writes in the overlay file are effectively
        // invisible until the sidecar gets written. This is deliberate:
        // "dirty" means "the host finished writing this sector," not "the
        // file has some bytes here" (sparse allocation can contain partial
        // writes from an interrupted run, which can't be trusted).
        let sidecar = dirty_sidecar_path(overlay_path);
        // A sidecar we cannot trust is a hard error, not an empty set: silently
        // starting clean would hide every write a previous session made, and the
        // overlay's bytes would still be sitting there unreferenced.
        let dirty = load_dirty_sidecar(&sidecar, base_size / SECTOR_SIZE)?;

        eprintln!("iris: COW overlay active (base: {}, overlay: {}, dirty sectors: {})",
                  base_path, overlay_path, dirty.len());
        if dirty.is_empty() && std::fs::metadata(overlay_path).map(|m| m.len()).unwrap_or(0) > 0 {
            eprintln!("iris: note: overlay file has data but no .dirty sidecar — prior writes are not in use");
        }
        eprintln!("iris: to reset disk to clean state, delete {} and {}",
                  overlay_path, sidecar.display());

        Ok(Self {
            base,
            overlay,
            dirty,
            base_size,
            overlay_path: overlay_path.to_string(),
            base_path: base_path.to_string(),
        })
    }

    /// Sectors addressable on this disk. Valid LBAs are `0..sector_count()`.
    fn sector_count(&self) -> u64 {
        self.base_size / SECTOR_SIZE
    }

    fn check_range(&self, lba: u64, count: u64) -> io::Result<()> {
        let end = lba.checked_add(count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "COW LBA range overflows")
        })?;
        if end > self.sector_count() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "COW LBA {}..{} is outside the base image's {} sectors",
                    lba,
                    end,
                    self.sector_count()
                ),
            ));
        }
        Ok(())
    }

    /// Read `count` sectors starting at `lba`.
    /// Dirty sectors are read from the overlay, clean sectors from the base.
    pub fn read_sectors(&mut self, lba: u64, count: usize) -> io::Result<Vec<u8>> {
        self.check_range(lba, count as u64)?;
        let total = count * SECTOR_SIZE as usize;
        let mut data = vec![0u8; total];

        // Batch consecutive sectors from the same source to minimize seeks.
        let mut pos = 0usize;
        let mut sector = lba;
        while pos < total {
            // Determine run length from the same source.
            let is_dirty = self.dirty.contains(&sector);
            let mut run = 1usize;
            while pos + run * SECTOR_SIZE as usize <= total {
                let next = sector + run as u64;
                if self.dirty.contains(&next) != is_dirty {
                    break;
                }
                run += 1;
            }
            // Don't overshoot.
            let run_sectors = run.min((total - pos) / SECTOR_SIZE as usize);
            let run_bytes = run_sectors * SECTOR_SIZE as usize;

            let file = if is_dirty { &mut self.overlay } else { &mut self.base };
            file.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
            file.read_exact(&mut data[pos..pos + run_bytes])?;

            pos += run_bytes;
            sector += run_sectors as u64;
        }

        Ok(data)
    }

    /// Write sectors starting at `lba`. Data length must be a multiple of 512.
    /// Writes go to the overlay file only; the base image is never modified.
    pub fn write_sectors(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        // A real error, not a `debug_assert`: a release build used to write the
        // odd tail bytes and then mark only the whole sectors dirty, so the tail
        // sat in the overlay while reads of it came from the base.
        if !data.len().is_multiple_of(SECTOR_SIZE as usize) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("COW write length {} is not a multiple of {}", data.len(), SECTOR_SIZE),
            ));
        }
        let count = data.len() / SECTOR_SIZE as usize;
        // Bounds-check before writing: the overlay is a plain file, so a write
        // past the base's capacity would just extend it, and `commit` would then
        // extend the base image to match.
        self.check_range(lba, count as u64)?;

        self.overlay.seek(SeekFrom::Start(lba * SECTOR_SIZE))?;
        self.overlay.write_all(data)?;

        for i in 0..count as u64 {
            self.dirty.insert(lba + i);
        }

        Ok(())
    }

    /// Base image size in bytes.
    pub fn size(&self) -> u64 {
        self.base_size
    }

    /// Merge all dirty overlay sectors into the base image, then truncate overlay.
    pub fn commit(&mut self) -> io::Result<usize> {
        let base_path = self.base_path.clone();
        let mut base_rw = OpenOptions::new().read(true).write(true).open(&base_path)?;
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        let mut committed = 0usize;

        // Sorted, so the base is written front to back rather than in hash order.
        let mut sectors: Vec<u64> = self.dirty.iter().copied().collect();
        sectors.sort_unstable();
        for lba in sectors {
            self.overlay.seek(SeekFrom::Start(lba * SECTOR_SIZE))?;
            self.overlay.read_exact(&mut buf)?;
            base_rw.seek(SeekFrom::Start(lba * SECTOR_SIZE))?;
            base_rw.write_all(&buf)?;
            committed += 1;
        }
        base_rw.sync_all()?;

        // Persist the empty dirty set *before* truncating the overlay. In the
        // other order, a crash in between leaves a sidecar naming sectors the
        // truncated overlay no longer holds, and every read of one of them fails
        // at end-of-file.
        self.dirty.clear();
        save_dirty_sidecar(&dirty_sidecar_path(&self.overlay_path), &self.dirty)?;
        self.overlay.set_len(0)?;
        self.overlay.sync_all()?;

        // Reopen base read-only to pick up committed data.
        self.base = File::open(&base_path)?;

        eprintln!("iris: COW committed {} sectors to {}", committed, base_path);
        Ok(committed)
    }

    /// Discard every overlay write, returning the disk to the base image's
    /// contents.
    pub fn reset_overlay(&mut self) -> io::Result<()> {
        // Same ordering argument as `commit`: the record of "nothing is dirty"
        // has to be durable before the data it describes goes away.
        self.dirty.clear();
        save_dirty_sidecar(&dirty_sidecar_path(&self.overlay_path), &self.dirty)?;
        self.overlay.set_len(0)?;
        self.overlay.seek(SeekFrom::Start(0))?;
        self.overlay.sync_all()?;
        Ok(())
    }

    /// Flush the overlay file's data and persist the dirty sector set to
    /// the sidecar. Call this on clean shutdown or before snapshot save so
    /// a subsequent run can read back what we wrote.
    pub fn flush(&mut self) -> io::Result<()> {
        self.overlay.sync_all()?;
        save_dirty_sidecar(&dirty_sidecar_path(&self.overlay_path), &self.dirty)
    }

    /// Number of dirty sectors in the overlay.
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// Copy the current overlay file to `dest` and return the dirty sector
    /// list (sorted, ascending). Used by snapshot save so the entire disk
    /// state — base + overlay — is captured consistently with RAM.
    pub fn export_overlay(&mut self, dest: &Path) -> io::Result<Vec<u64>> {
        self.overlay.sync_all()?;
        reflink_or_copy(Path::new(&self.overlay_path), dest)?;
        let mut dirty: Vec<u64> = self.dirty.iter().copied().collect();
        dirty.sort_unstable();
        Ok(dirty)
    }

    /// Replace the overlay contents with `source` and adopt `dirty` as the
    /// dirty sector set. Used by snapshot load. If `source` doesn't exist
    /// the overlay is truncated instead (matches `reset_overlay` behavior —
    /// handles old snapshots without overlay data).
    pub fn import_overlay(&mut self, source: &Path, dirty: Vec<u64>) -> io::Result<()> {
        // Validate before changing anything: adopting a sector list that names
        // offsets past the base, or one that describes an overlay file we do not
        // have, leaves every read of those sectors failing at end-of-file.
        if let Some(&bad) = dirty.iter().find(|&&lba| lba >= self.sector_count()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "snapshot overlay names sector {} on a {}-sector disk",
                    bad,
                    self.sector_count()
                ),
            ));
        }
        if source.exists() {
            reflink_or_copy(source, Path::new(&self.overlay_path))?;
        } else {
            if !dirty.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "{} is missing but its snapshot lists {} dirty sectors; restoring \
                         would point the disk at data that is not there",
                        source.display(),
                        dirty.len()
                    ),
                ));
            }
            // Nothing was saved for this device and nothing claims to be dirty:
            // an empty overlay is the correct state.
            std::fs::File::create(&self.overlay_path)?;
        }
        // Reopen the file handle — the previous File object points at the
        // old inode (which std::fs::copy replaced on some platforms).
        self.overlay = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.overlay_path)?;
        self.dirty = dirty.into_iter().collect();
        Ok(())
    }
}

impl Drop for CowDisk {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("iris: COW flush on drop failed for {}: {} (writes may be lost)",
                      self.overlay_path, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn unique_tmp(tag: &str, ext: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("iris-cow-{}-{}.{}", tag, nanos, ext))
    }

    #[test]
    fn reflink_or_copy_preserves_bytes() {
        let src = unique_tmp("reflink-src", "bin");
        let dst = unique_tmp("reflink-dst", "bin");
        let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024 + 17).collect();
        {
            let mut f = File::create(&src).unwrap();
            f.write_all(&payload).unwrap();
            f.sync_all().unwrap();
        }
        reflink_or_copy(&src, &dst).expect("reflink_or_copy");
        let read_back = std::fs::read(&dst).unwrap();
        assert_eq!(read_back, payload);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }

    /// A base image of `sectors` 512-byte blocks filled with `fill`, plus an
    /// overlay path beside it.
    fn cow_pair(tag: &str, sectors: u64, fill: u8) -> (PathBuf, PathBuf) {
        let base = unique_tmp(tag, "img");
        let overlay = unique_tmp(tag, "overlay");
        let mut f = File::create(&base).unwrap();
        f.write_all(&vec![fill; (sectors * SECTOR_SIZE) as usize]).unwrap();
        f.sync_all().unwrap();
        (base, overlay)
    }

    fn scrub(base: &Path, overlay: &Path) {
        let _ = std::fs::remove_file(base);
        let _ = std::fs::remove_file(overlay);
        let _ = std::fs::remove_file(dirty_sidecar_path(overlay.to_str().unwrap()));
    }

    /// Commit has to write to the configured base, whatever the overlay is
    /// called. It used to reconstruct the path by stripping a `.overlay` suffix,
    /// so any other naming failed outright.
    #[test]
    fn commit_uses_the_configured_base_not_the_overlay_name() {
        let (base, overlay) = cow_pair("commit", 64, 0xAA);
        // Deliberately *not* "<base>.overlay".
        let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
        cow.write_sectors(3, &[0x5A; 512]).unwrap();
        assert_eq!(cow.read_sectors(3, 1).unwrap(), vec![0x5A; 512]);
        assert_eq!(std::fs::read(&base).unwrap()[3 * 512], 0xAA, "base untouched before commit");

        assert_eq!(cow.commit().unwrap(), 1);
        assert_eq!(cow.dirty_count(), 0);
        let on_disk = std::fs::read(&base).unwrap();
        assert_eq!(&on_disk[3 * 512..4 * 512], &vec![0x5A; 512][..], "commit reached the base");
        assert_eq!(&on_disk[2 * 512..3 * 512], &vec![0xAA; 512][..], "neighbour untouched");
        drop(cow);
        scrub(&base, &overlay);
    }

    /// After a commit the sidecar must already say "nothing dirty" — if the
    /// truncation lands first, a crash in between leaves it naming sectors the
    /// empty overlay no longer holds and every read of one fails at EOF.
    #[test]
    fn commit_persists_an_empty_dirty_set_before_truncating() {
        let (base, overlay) = cow_pair("order", 64, 0xAA);
        let sidecar = dirty_sidecar_path(overlay.to_str().unwrap());
        {
            let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
            cow.write_sectors(7, &[0x5A; 512]).unwrap();
            cow.commit().unwrap();
            // Inspect the sidecar without letting Drop rewrite it.
            let set = load_dirty_sidecar(&sidecar, 64).unwrap();
            assert!(set.is_empty(), "the empty set is durable as soon as commit returns");
            assert_eq!(std::fs::metadata(&overlay).unwrap().len(), 0, "overlay truncated");
        }
        // Reopening after that sequence is clean, and reads come from the base.
        let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
        assert_eq!(cow.dirty_count(), 0);
        assert_eq!(cow.read_sectors(7, 1).unwrap(), vec![0x5A; 512], "reads the committed base");
        drop(cow);
        scrub(&base, &overlay);
    }

    /// Same ordering guarantee for a rollback.
    #[test]
    fn reset_persists_an_empty_dirty_set() {
        let (base, overlay) = cow_pair("reset", 64, 0xAA);
        let sidecar = dirty_sidecar_path(overlay.to_str().unwrap());
        let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
        cow.write_sectors(9, &[0x5A; 512]).unwrap();
        cow.reset_overlay().unwrap();
        assert!(load_dirty_sidecar(&sidecar, 64).unwrap().is_empty());
        assert_eq!(cow.read_sectors(9, 1).unwrap(), vec![0xAA; 512], "rollback restored the base");
        drop(cow);
        scrub(&base, &overlay);
    }

    /// A dirty list that cannot be what we wrote must be refused, not silently
    /// treated as empty (which would orphan a previous session's writes) and not
    /// used to size an allocation (`u64::MAX` entries would abort the process).
    #[test]
    fn corrupt_dirty_lists_are_rejected() {
        let (base, overlay) = cow_pair("corrupt", 64, 0xAA);
        let sidecar = dirty_sidecar_path(overlay.to_str().unwrap());
        File::create(&overlay).unwrap();

        // A header claiming an absurd count, with no entries behind it.
        std::fs::write(&sidecar, u64::MAX.to_le_bytes()).unwrap();
        let Err(e) = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()) else {
            panic!("an absurd dirty-list count must be refused");
        };
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("claims"), "{e}");

        // A count that disagrees with the file length.
        let mut bad = Vec::new();
        bad.extend_from_slice(&5u64.to_le_bytes());
        bad.extend_from_slice(&1u64.to_le_bytes());
        std::fs::write(&sidecar, &bad).unwrap();
        assert!(CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).is_err());

        // An entry outside the disk.
        let mut oob = Vec::new();
        oob.extend_from_slice(&1u64.to_le_bytes());
        oob.extend_from_slice(&999u64.to_le_bytes());
        std::fs::write(&sidecar, &oob).unwrap();
        let Err(e) = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()) else {
            panic!("an out-of-range dirty sector must be refused");
        };
        assert!(e.to_string().contains("999"), "{e}");

        // A well-formed one still loads.
        let mut good = Vec::new();
        good.extend_from_slice(&1u64.to_le_bytes());
        good.extend_from_slice(&4u64.to_le_bytes());
        std::fs::write(&sidecar, &good).unwrap();
        let cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
        assert_eq!(cow.dirty_count(), 1);
        drop(cow);
        scrub(&base, &overlay);
    }

    /// Writes past the base's capacity are refused rather than extending the
    /// overlay — and with it, the base at the next commit.
    #[test]
    fn out_of_range_writes_are_refused() {
        let (base, overlay) = cow_pair("bounds", 64, 0xAA);
        let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
        assert!(cow.write_sectors(64, &[0x5A; 512]).is_err(), "first sector past the end");
        assert!(cow.write_sectors(63, &[0x5A; 1024]).is_err(), "straddling the end");
        assert!(cow.read_sectors(64, 1).is_err(), "reads bounded too");
        assert_eq!(cow.dirty_count(), 0, "a rejected write marks nothing dirty");
        // A non-sector-multiple length is an error, not a debug-only assert:
        // release builds used to write the tail and not record it.
        assert!(cow.write_sectors(0, &[0x5A; 700]).is_err());
        assert_eq!(cow.dirty_count(), 0);
        drop(cow);
        scrub(&base, &overlay);
    }

    /// Restoring a snapshot whose overlay file is missing, but whose sector list
    /// is not empty, must fail rather than point the disk at data that is gone.
    #[test]
    fn import_refuses_a_missing_overlay_with_dirty_sectors() {
        let (base, overlay) = cow_pair("import", 64, 0xAA);
        let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
        let absent = unique_tmp("import-absent", "overlay");
        let e = cow.import_overlay(&absent, vec![1, 2, 3]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        // An out-of-range sector list is refused too.
        assert!(cow.import_overlay(&absent, vec![9999]).is_err());
        // Missing + genuinely empty is fine: that is a clean disk.
        cow.import_overlay(&absent, vec![]).unwrap();
        assert_eq!(cow.dirty_count(), 0);
        drop(cow);
        scrub(&base, &overlay);
    }

    /// The overlay survives a close/reopen cycle through the sidecar.
    #[test]
    fn overlay_round_trips_across_a_reopen() {
        let (base, overlay) = cow_pair("persist", 64, 0xAA);
        {
            let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
            cow.write_sectors(2, &[0x11; 512]).unwrap();
            cow.write_sectors(20, &[0x22; 1024]).unwrap();
            cow.flush().unwrap();
        }
        let mut cow = CowDisk::new(base.to_str().unwrap(), overlay.to_str().unwrap()).unwrap();
        assert_eq!(cow.dirty_count(), 3);
        assert_eq!(cow.read_sectors(2, 1).unwrap(), vec![0x11; 512]);
        assert_eq!(cow.read_sectors(20, 2).unwrap(), vec![0x22; 1024]);
        assert_eq!(cow.read_sectors(3, 1).unwrap(), vec![0xAA; 512], "clean sector from the base");
        // A run spanning clean and dirty sectors.
        let span = cow.read_sectors(1, 3).unwrap();
        assert_eq!(&span[..512], &vec![0xAA; 512][..]);
        assert_eq!(&span[512..1024], &vec![0x11; 512][..]);
        assert_eq!(&span[1024..], &vec![0xAA; 512][..]);
        drop(cow);
        scrub(&base, &overlay);
    }

    #[test]
    fn reflink_or_copy_overwrites_existing_dst() {
        let src = unique_tmp("reflink-src2", "bin");
        let dst = unique_tmp("reflink-dst2", "bin");
        std::fs::write(&src, b"new content").unwrap();
        std::fs::write(&dst, b"old content that is longer than the new one").unwrap();
        reflink_or_copy(&src, &dst).expect("overwrite");
        assert_eq!(std::fs::read(&dst).unwrap(), b"new content");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    }
}
