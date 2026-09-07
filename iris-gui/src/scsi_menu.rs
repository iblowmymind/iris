use iris::config::{MachineConfig, ScsiDeviceConfig};
use std::path::Path;

/// What the user picked from a SCSI submenu, deferred for the App to act on
/// (so we don't hold &mut MachineConfig across nested closures and dialogs).
pub enum ScsiAction {
    AttachHdd { id: u8, path: String },
    AttachEmptyCdrom { id: u8 },
    AttachCdromWithDisc { id: u8, path: String },
    InsertDisc { id: u8, path: String },
    Eject { id: u8 },
    Detach { id: u8 },
    ToggleOverlay { id: u8 },
}

/// The user-visible name of a SCSI slot: what is attached, and for a disk
/// image its file name and size.
pub fn render_label(id: u8, dev: Option<&ScsiDeviceConfig>) -> String {
    match dev {
        None => format!("SCSI #{id}: (empty)"),
        Some(d) if d.is_daynaport() => format!("SCSI #{id}: DaynaPort (Ethernet)"),
        Some(d) if d.is_cdrom() => {
            if d.path.is_empty() {
                format!("SCSI #{id}: CD (no media)")
            } else if !Path::new(&d.path).exists() {
                format!("SCSI #{id}: CD ⚠ {} (missing)", d.path)
            } else {
                let name = Path::new(&d.path).file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| d.path.clone());
                format!("SCSI #{id}: CD {name}")
            }
        }
        Some(d) => {
            let name = Path::new(&d.path).file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| d.path.clone());
            let size = std::fs::metadata(&d.path).map(|m| m.len()).unwrap_or(0);
            let mb = size as f64 / (1024.0 * 1024.0);
            let suffix = if d.overlay { " [COW]" } else { "" };
            if size > 0 {
                format!("SCSI #{id}: HDD {name} ({mb:.0} MB){suffix}")
            } else {
                format!("SCSI #{id}: HDD {name}{suffix}")
            }
        }
    }
}

// Seed the picker at `cur`'s folder, else the managed disks dir — never the OS
// default. See `crate::filedialog`, which is where the "else" used to be a
// silent no-op and the panel opened at whatever the user last browsed.
fn dialog_at(title: &str, cur: &str) -> rfd::FileDialog {
    crate::filedialog::dialog(title, cur, crate::filedialog::Anchor::Disks,
                              crate::filedialog::Purpose::Open)
}

pub fn pick_disk(title: &str, cur: &str) -> Option<String> {
    dialog_at(title, cur)
        .add_filter("Disk images", &["raw", "img", "chd"])
        .add_filter("All", &["*"])
        .pick_file()
        .map(|p| p.to_string_lossy().into_owned())
}

pub fn pick_iso(title: &str, cur: &str) -> Option<String> {
    dialog_at(title, cur)
        .add_filter("ISO images", &["iso", "chd"])
        .add_filter("All", &["*"])
        .pick_file()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Apply an action to the config.
pub fn apply(cfg: &mut MachineConfig, action: ScsiAction) -> Option<String> {
    match action {
        ScsiAction::AttachHdd { id, path } => {
            cfg.scsi.insert(id, ScsiDeviceConfig { path, ..Default::default() });
            Some(format!("scsi{id}: HDD attached"))
        }
        ScsiAction::AttachEmptyCdrom { id } => {
            cfg.scsi.insert(id, ScsiDeviceConfig { cdrom: true, ..Default::default() });
            Some(format!("scsi{id}: empty CD-ROM drive attached (Stop→Start if VM is running)"))
        }
        ScsiAction::AttachCdromWithDisc { id, path } => {
            cfg.scsi.insert(id, ScsiDeviceConfig {
                path: path.clone(), cdrom: true, ..Default::default()
            });
            Some(format!("scsi{id}: CD-ROM attached with disc"))
        }
        ScsiAction::InsertDisc { id, path } => {
            if let Some(d) = cfg.scsi.get_mut(&id) { d.path = path; }
            Some(format!("scsi{id}: disc inserted"))
        }
        ScsiAction::Eject { id } => {
            if let Some(d) = cfg.scsi.get_mut(&id) { d.path = String::new(); }
            Some(format!("scsi{id}: ejected"))
        }
        ScsiAction::Detach { id } => {
            cfg.scsi.remove(&id);
            Some(format!("scsi{id}: detached"))
        }
        ScsiAction::ToggleOverlay { id } => {
            if let Some(d) = cfg.scsi.get_mut(&id) { d.overlay = !d.overlay; }
            Some(format!("scsi{id}: overlay toggled"))
        }
    }
}
