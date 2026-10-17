//! Menu model and dispatcher for the native macOS menu bar.
//!
//! The menus hold what the classic sidebar's File, Machine, Memory, SCSI,
//! View and Help menus hold. A feature added to one should normally be added to
//! the other too.

use crate::settings;
use crate::App;
use eframe::egui;
use iris::config::CpuModel;

/// Everything the menus can ask the app to do.
///
/// Deliberately data-only: a native `NSMenuItem` can't hold a closure, so a
/// click resolves to one of these and is applied later on the UI thread, after
/// the menu has closed. That also means a file picker opened by a menu item
/// (`AttachHdd` and friends) runs *after* the menu is gone, rather than
/// underneath it.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    // --- File ---
    NewMachine,
    SwitchMachine(String),
    RenameMachine,
    DeleteMachine,
    PrepareForPremiere,
    GrantDiskFolder,
    RevealFolder(String),
    RevokeFolder(String),
    Quit,

    // --- Machine ---
    Start,
    Stop,
    Reset,
    ResetNvram,
    SetCpu(CpuModel),
    SaveState(String),
    RestoreState(String),
    Screenshot,
    SerialConsole,
    ToggleCapture,

    // --- Memory ---
    SetRam(u32),
    SetBank(usize, u32),

    // --- SCSI ---
    Scsi(ScsiCmd),
    CowCommit { base: String, chd: bool },
    CowDiscard { id: u8, base: String, chd: bool },

    // --- View ---
    ToggleFullscreen,
    SetVmScale(f32),
    SetUiScale(f32),
    /// A language code from `i18n::languages()`, or empty for the system's.
    SetLanguage(String),
    ShowConfig,

    // --- Help ---
    NetCheck,
    CameraTest,
    HelpInfo,
    NfsHelp,
    Ultra64Help,
    License,
    Privacy,
    About,
    OpenUrl(&'static str),
}

/// Save-state slots offered by the Machine menu.
const SAVE_SLOTS: [&str; 4] = ["snap1", "snap2", "snap3", "snap4"];

/// A SCSI bus operation. The `Pick*` variants open a file dialog when applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScsiCmd {
    PickHdd(u8),
    AttachEmptyCdrom(u8),
    PickCdromWithDisc(u8),
    PickDisc(u8),
    Eject(u8),
    Remount(u8),
    Detach(u8),
    CreateBlank(u8),
    ToggleOverlay(u8),
}

/// A keyboard equivalent. Command on macOS, Ctrl elsewhere — the modifier is
/// implied, since a menu accelerator that isn't the platform's own command key
/// would collide with keystrokes meant for the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accel {
    pub key: char,
    pub shift: bool,
}

/// Lines of explanation, one row each, or nothing when the catalog leaves the
/// text empty (the default layout's `subtitle` convention). A menu item's
/// title is a single line, so a multi-line value is split.
fn info(text: String) -> Vec<Item> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| Item::Info(l.to_string()))
        .collect()
}

/// One row of a menu.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Action {
        label: String,
        action: Action,
        enabled: bool,
        checked: bool,
        accel: Option<Accel>,
    },
    Sub {
        label: String,
        items: Vec<Item>,
    },
    /// Non-clickable text: a section heading, or a line of explanation.
    Info(String),
    Separator,
}

/// A top-level menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    pub title: String,
    pub items: Vec<Item>,
}

/// A clickable item. Chain `.off()` / `.checked()` / `.accel()` to refine it.
pub fn act(label: impl Into<String>, action: Action) -> Item {
    Item::Action {
        label: label.into(),
        action,
        enabled: true,
        checked: false,
        accel: None,
    }
}

impl Item {
    /// Enable only when `yes`.
    pub fn enabled_if(mut self, yes: bool) -> Self {
        if let Item::Action { enabled, .. } = &mut self {
            *enabled = yes;
        }
        self
    }
    pub fn checked_if(mut self, yes: bool) -> Self {
        if let Item::Action { checked, .. } = &mut self {
            *checked = yes;
        }
        self
    }
    pub fn accel(mut self, key: char) -> Self {
        if let Item::Action { accel, .. } = &mut self {
            *accel = Some(Accel { key, shift: false });
        }
        self
    }
    pub fn accel_shift(mut self, key: char) -> Self {
        if let Item::Action { accel, .. } = &mut self {
            *accel = Some(Accel { key, shift: true });
        }
        self
    }
}

/// "1", "1.25", "1.33": a scale factor to two decimals, trailing zeros dropped
/// so the common whole steps read clean (as the default layout's status footer
/// writes it). Catalog strings add the "×".
pub fn scale_number(s: f32) -> String {
    let text = format!("{s:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// "1×", "1.25×", for the scale choices themselves.
pub fn scale_label(s: f32) -> String {
    format!("{}\u{00d7}", scale_number(s))
}

/// Steps of a scale slider, as menu-friendly discrete choices.
fn scale_steps(min: f32, max: f32, step: f32) -> Vec<f32> {
    let mut out = Vec::new();
    let n = ((max - min) / step).round() as i32;
    for i in 0..=n {
        out.push(min + step * i as f32);
    }
    out
}

fn same_scale(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

impl App {
    /// Build the whole menu tree from current state.
    ///
    /// Cheap enough to call a few times a second (see the rebuild throttle in
    /// `refresh_menus`), but *not* every frame: naming a SCSI disk stats its image
    /// file to report the size.
    pub fn build_menus(&self) -> Vec<Menu> {
        vec![
            self.file_menu(),
            self.machine_menu(),
            self.memory_menu(),
            self.scsi_menu(),
            self.view_menu(),
            self.help_menu(),
        ]
    }

    fn file_menu(&self) -> Menu {
        let can_change = self.emu.can_change_directory();
        let mut items = vec![
            act(t!("menu-file-new-machine"), Action::NewMachine).accel('n').enabled_if(can_change),
        ];

        let mut machines: Vec<Item> = Vec::new();
        if self.prefs.machines.is_empty() {
            machines.push(Item::Info(t!("menu-file-no-machines")));
        }
        for name in self.prefs.machines.keys() {
            let active = self.prefs.active_machine.as_deref() == Some(name.as_str());
            machines.push(
                act(name.clone(), Action::SwitchMachine(name.clone()))
                    .checked_if(active)
                    .enabled_if(can_change),
            );
        }
        items.push(Item::Sub { label: t!("menu-file-switch-machine"), items: machines });

        let has_active = self.prefs.active_machine.is_some();
        items.push(
            act(t!("menu-file-rename-machine"), Action::RenameMachine).enabled_if(has_active && can_change),
        );
        items.push(
            act(t!("menu-file-delete-machine"), Action::DeleteMachine).enabled_if(has_active && can_change),
        );

        items.push(Item::Separator);
        items.push(act(t!("macos-file-settings"), Action::ShowConfig).accel(','));

        // A source-build affordance for users who also run the standalone
        // `iris` CLI: it flushes the active machine's own TOML and reports
        // the path to run the CLI's --config against (from that machine's
        // folder — see prepare_for_premiere). Hidden in pre-compiled / App
        // Store builds (the `bundled` feature). See iris-gui Cargo.toml.
        if !cfg!(feature = "bundled") {
            items.push(act(t!("menu-file-premiere"), Action::PrepareForPremiere));
        }

        // App Store sandbox: grant a whole folder (recursive) so the disk-sync
        // fold — which writes a temp beside the base and renames over it —
        // works, and so a disk image / NFS shared subfolder under it is covered
        // by one grant. Hidden elsewhere.
        if cfg!(feature = "appstore") {
            items.push(Item::Separator);
            items.push(Item::Info(t!("menu-file-folders-heading")));
            items.push(act(t!("menu-file-grant-folder"), Action::GrantDiskFolder));
            if self.prefs.disk_folders.is_empty() {
                items.push(Item::Info(t!("menu-file-no-folders")));
            }
            for f in &self.prefs.disk_folders {
                // Live access state: a grant can lapse mid-session, and the
                // only fix is to re-grant, so say which it is.
                let label = if crate::folder_accessible(f) {
                    t!("macos-file-folder-granted", path = f)
                } else {
                    t!("macos-file-folder-lapsed", path = f)
                };
                items.push(Item::Sub {
                    label,
                    items: vec![
                        act(t!("macos-file-folder-reveal"), Action::RevealFolder(f.clone())),
                        act(t!("macos-file-folder-revoke"), Action::RevokeFolder(f.clone())),
                    ],
                });
            }
        }

        items.push(Item::Separator);
        items.push(act(t!("menu-file-quit"), Action::Quit).accel('q'));
        Menu { title: t!("menu-file"), items }
    }

    fn machine_menu(&self) -> Menu {
        let running = self.emu.is_running();
        let mut items = vec![
            act(t!("menu-machine-start"), Action::Start).enabled_if(!running).accel('r'),
            act(t!("menu-machine-stop"), Action::Stop).enabled_if(running).accel_shift('r'),
            act(t!("menu-machine-reset"), Action::Reset).enabled_if(running),
            act(t!("menu-machine-reset-nvram"), Action::ResetNvram).enabled_if(!running),
            Item::Separator,
        ];

        // Chosen at Machine::new, so an edit while running is pending, not live.
        let mut cpus: Vec<Item> = CpuModel::ALL
            .iter()
            .map(|&c| {
                act(c.label(), Action::SetCpu(c))
                    .checked_if(self.cfg.machine.cpu == c)
                    .enabled_if(!running)
            })
            .collect();
        if running {
            cpus.push(Item::Separator);
            match self.started_cpu {
                Some(started) if started != self.cfg.machine.cpu => cpus.push(Item::Info(
                    t!("menu-machine-running-pending", cpu = started.label()),
                )),
                Some(started) => cpus.push(Item::Info(t!("menu-machine-running", cpu = started.label()))),
                None => {}
            }
            // Matches the default layout's menu_list Machine-menu hint — the
            // radio items above are disabled while running for the same
            // reason.
            cpus.extend(info(t!("menu-machine-cpu-subtitle")));
        }
        items.push(Item::Sub {
            label: t!("menu-machine-processor", cpu = self.cfg.machine.cpu.label()),
            items: cpus,
        });

        items.push(Item::Separator);
        // Named slots rather than a typed-in name: a menu has nowhere to type,
        // and a fixed set of slots is what a save state is used as anyway.
        let slots = |kind: fn(String) -> Action| -> Vec<Item> {
            SAVE_SLOTS
                .iter()
                .map(|name| act(*name, kind(name.to_string())).enabled_if(running))
                .collect()
        };
        items.push(Item::Sub { label: t!("menu-machine-save-state"), items: slots(Action::SaveState) });
        items.push(Item::Sub { label: t!("menu-machine-restore-state"), items: slots(Action::RestoreState) });
        items.push(Item::Separator);
        items.push(act(t!("menu-machine-screenshot"), Action::Screenshot).enabled_if(running));
        items.push(act(t!("menu-machine-serial-console"), Action::SerialConsole).enabled_if(running));
        items.push(Item::Separator);
        items.push(
            act(
                if self.input_state.captured {
                    t!("macos-machine-release-capture", keys = crate::input::RELEASE_HINT)
                } else {
                    t!("main-capture")
                },
                Action::ToggleCapture,
            )
            .enabled_if(running)
            .accel('k'),
        );
        if running && self.input_state.captured {
            // The chord only does anything while captured (input.rs returns
            // early on the uncaptured path before it sets f11_to_guest), and
            // the default layout's capture_controls only shows this line in
            // the same state — surface it here too.
            items.extend(info(t!("main-capture-f11-subtitle")));
        }
        Menu { title: t!("menu-machine"), items }
    }

    fn memory_menu(&self) -> Menu {
        let running = self.emu.is_running();
        let mut items = vec![Item::Info(t!(
            "menu-memory-configured",
            ram = crate::ram_summary(&self.cfg.banks)
        ))];
        if running {
            if let Some(started) = self.started_banks {
                if started != self.cfg.banks {
                    items.push(Item::Info(t!(
                        "menu-memory-running-pending",
                        ram = crate::ram_summary(&started)
                    )));
                } else {
                    items.push(Item::Info(t!("menu-memory-running", ram = crate::ram_summary(&started))));
                }
            }
            items.extend(info(t!("menu-memory-running-subtitle")));
        } else {
            items.extend(info(t!("menu-memory-stopped-subtitle")));
        }
        let ip28_banks = self.cfg.machine.profile.ip28();
        items.push(Item::Separator);
        items.push(Item::Info(t!("menu-memory-presets")));
        for &p in crate::ram::ram_presets(ip28_banks) {
            items.push(
                act(t!("menu-memory-size", mb = p), Action::SetRam(p))
                    .enabled_if(!running)
                    .checked_if(self.cfg.banks == crate::distribute_ram(p, ip28_banks)),
            );
        }
        items.push(Item::Separator);
        items.push(Item::Info(t!("menu-memory-banks")));
        for i in 0..4 {
            let banks: Vec<Item> = iris::config::VALID_BANK_SIZES
                .iter()
                .map(|&sz| {
                    act(t!("menu-memory-size", mb = sz), Action::SetBank(i, sz))
                        .enabled_if(!running && (sz <= 128 || ip28_banks))
                        .checked_if(self.cfg.banks[i] == sz)
                })
                .collect();
            items.push(Item::Sub {
                label: t!("menu-memory-bank", bank = i, mb = self.cfg.banks[i]),
                items: banks,
            });
        }
        Menu { title: t!("menu-memory"), items }
    }

    fn scsi_menu(&self) -> Menu {
        let mut items = Vec::new();
        for id in 1u8..=7 {
            let dev = self.cfg.scsi.get(&id);
            let label = crate::scsi_menu::render_label(id, dev);
            let sub = match dev {
                None => vec![
                    act(t!("scsi-attach-hdd"), Action::Scsi(ScsiCmd::PickHdd(id))),
                    // Attaching a CD-ROM gives an empty drive by default; the
                    // user loads media afterwards via "Insert disc…". Mirrors
                    // real hardware and avoids an upfront file prompt.
                    act(t!("scsi-attach-cdrom-empty"), Action::Scsi(ScsiCmd::AttachEmptyCdrom(id))),
                    act(t!("scsi-attach-cdrom-disc"), Action::Scsi(ScsiCmd::PickCdromWithDisc(id))),
                    act(t!("scsi-create-blank"), Action::Scsi(ScsiCmd::CreateBlank(id))),
                ],
                Some(d) if d.is_daynaport() => {
                    let mut v = info(t!("scsi-daynaport-subtitle"));
                    if !v.is_empty() {
                        v.push(Item::Separator);
                    }
                    v.push(act(t!("scsi-detach-daynaport"), Action::Scsi(ScsiCmd::Detach(id))));
                    v
                }
                Some(d) if d.is_cdrom() => {
                    let has_media =
                        !d.path.is_empty() && std::path::Path::new(&d.path).exists();
                    let mut v = Vec::new();
                    if has_media {
                        v.push(act(t!("scsi-eject"), Action::Scsi(ScsiCmd::Eject(id))));
                    }
                    v.push(act(
                        if has_media { t!("scsi-change-disc") } else { t!("scsi-insert-disc") },
                        Action::Scsi(ScsiCmd::PickDisc(id)),
                    ));
                    if has_media {
                        v.push(act(t!("scsi-mount-in-irix"), Action::Scsi(ScsiCmd::Remount(id))));
                    }
                    v.push(Item::Separator);
                    v.push(act(t!("scsi-detach-cdrom"), Action::Scsi(ScsiCmd::Detach(id))));
                    v
                }
                Some(d) => vec![
                    act(
                        if d.overlay {
                            t!("scsi-overlay-disable")
                        } else {
                            t!("scsi-overlay-enable")
                        },
                        Action::Scsi(ScsiCmd::ToggleOverlay(id)),
                    ),
                    act(t!("scsi-replace-image"), Action::Scsi(ScsiCmd::PickHdd(id))),
                    Item::Separator,
                    act(t!("scsi-detach-hdd"), Action::Scsi(ScsiCmd::Detach(id))),
                ],
            };
            items.push(Item::Sub { label, items: sub });
        }
        let hint = info(t!("scsi-menu-subtitle"));
        if !hint.is_empty() {
            items.push(Item::Separator);
            items.extend(hint);
        }

        // Copy-on-write overlays with something in them: commit or roll back.
        let cow = self.cow_entries();
        if !cow.is_empty() {
            items.push(Item::Separator);
            items.push(Item::Info(t!("menu-cow-heading")));
            if self.emu.is_running() {
                items.push(Item::Info(t!("menu-cow-stop-first")));
            } else {
                for (id, base, is_chd) in cow {
                    let name = std::path::Path::new(&base)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&base)
                        .to_string();
                    items.push(Item::Sub {
                        label: t!("menu-cow-disk", id = id, name = name),
                        items: vec![
                            act(
                                t!("menu-cow-commit"),
                                Action::CowCommit { base: base.clone(), chd: is_chd },
                            ),
                            act(
                                t!("menu-cow-discard"),
                                Action::CowDiscard { id, base: base.clone(), chd: is_chd },
                            ),
                        ],
                    });
                }
            }
        }
        Menu { title: t!("menu-scsi"), items }
    }

    fn view_menu(&self) -> Menu {
        let mut items = vec![
            act(
                if self.fullscreen { t!("macos-view-exit-fullscreen") } else { t!("macos-view-fullscreen") },
                Action::ToggleFullscreen,
            )
            .accel('f'),
            Item::Separator,
        ];

        // The emulated display is drawn at a whole number of device pixels per
        // emulated pixel wherever it can be, so these are exact: 1× is the
        // guest's own resolution, one emulated pixel per logical point.
        let vm: Vec<Item> = scale_steps(
            settings::VM_SCALE_MIN,
            settings::VM_SCALE_MAX,
            settings::VM_SCALE_STEP as f32,
        )
        .into_iter()
        .map(|s| {
            act(scale_label(s), Action::SetVmScale(s))
                .checked_if(same_scale(self.prefs.vm_scale, s))
        })
        .collect();
        items.push(Item::Sub {
            label: t!("macos-view-vm-scale", scale = scale_number(self.prefs.vm_scale)),
            items: vm,
        });

        let ui_steps: Vec<Item> = scale_steps(settings::UI_SCALE_MIN, settings::UI_SCALE_MAX, 0.25)
            .into_iter()
            .map(|s| {
                act(scale_label(s), Action::SetUiScale(s))
                    .checked_if(same_scale(self.prefs.ui_scale, s))
            })
            .collect();
        items.push(Item::Sub {
            label: t!("macos-view-ui-scale", scale = scale_number(self.prefs.ui_scale)),
            items: ui_steps,
        });

        items.push(Item::Separator);
        let current = &self.prefs.language;
        let mut languages = vec![
            act(t!("menu-view-language-system"), Action::SetLanguage(String::new()))
                .checked_if(current.is_empty()),
        ];
        for (code, name) in crate::i18n::languages() {
            languages.push(
                act(name, Action::SetLanguage(code.to_string())).checked_if(current == code),
            );
        }
        items.push(Item::Sub { label: t!("menu-view-language"), items: languages });

        Menu { title: t!("menu-view"), items }
    }

    fn help_menu(&self) -> Menu {
        let running = self.emu.is_running();
        // The diagnostic tests the guest IP against the NAT subnet, so it
        // doesn't apply in PCAP mode — matches the default layout's "check"
        // button (main.rs's nat_running gate).
        let nat_running = running
            && matches!(self.launched_net.as_ref(), Some((iris::config::NetMode::Nat, _)));
        let mut items = vec![
            Item::Info(t!("menu-help-diagnostics")),
            act(t!("menu-help-test-camera"), Action::CameraTest).enabled_if(running),
            act(t!("menu-help-serial-console"), Action::SerialConsole).enabled_if(running),
            act(t!("macos-help-net-check"), Action::NetCheck).enabled_if(nat_running),
            Item::Separator,
            act(t!("menu-help-camera-network"), Action::HelpInfo),
            act(t!("menu-help-nfs"), Action::NfsHelp),
        ];
        // N64 dev board getting-started guide. Ultra64 is always compiled in
        // now (see the "feature flag cleanups" commit), but it never runs in
        // App Store builds (needs an external gopher64 over POSIX shm).
        if !cfg!(feature = "appstore") {
            items.push(act(t!("menu-help-ultra64"), Action::Ultra64Help));
        }
        items.push(Item::Separator);
        items.push(act(t!("menu-help-licenses"), Action::License));
        items.push(act(t!("menu-help-privacy"), Action::Privacy));
        items.push(Item::Separator);
        items.push(act(
            t!("macos-help-github"),
            Action::OpenUrl("https://github.com/techomancer/iris"),
        ));
        items.push(Item::Separator);
        items.push(act(t!("macos-app-about"), Action::About));
        Menu { title: t!("menu-help"), items }
    }
}

impl App {
    /// Disks that currently have a copy-on-write overlay on disk:
    /// `(scsi id, base image path, is a CHD)`.
    pub fn cow_entries(&self) -> Vec<(u8, String, bool)> {
        let mut entries = Vec::new();
        for (&id, dev) in &self.cfg.scsi {
            if dev.cdrom || dev.scratch || dev.path.trim().is_empty() {
                continue;
            }
            let is_chd = iris::chd_disk::is_chd(&dev.path);
            let has_overlay = if is_chd {
                iris::chd_disk::diff_path_for(std::path::Path::new(&dev.path)).exists()
            } else if dev.overlay {
                std::path::Path::new(&format!("{}.overlay", dev.path)).exists()
            } else {
                false
            };
            if has_overlay {
                entries.push((id, dev.path.clone(), is_chd));
            }
        }
        entries
    }

    /// The one place a chosen menu item takes effect.
    pub fn apply_menu_action(&mut self, action: Action, ctx: &egui::Context) {
        use crate::handle::Cmd;
        use egui::ViewportCommand;
        match action {
            // --- File ---
            Action::NewMachine => self.new_machine.open(),
            Action::SwitchMachine(name) => self.switch_to(&name),
            Action::RenameMachine => {
                // Opens the rename dialog seeded with the current name.
                self.rename_buffer = self.prefs.active_machine.clone();
            }
            Action::DeleteMachine => {
                if let Some(name) = self.prefs.active_machine.clone() {
                    self.delete_machine(&name);
                }
            }
            Action::PrepareForPremiere => self.prepare_for_premiere(),
            Action::GrantDiskFolder => self.grant_disk_folder(),
            Action::RevealFolder(f) => crate::config_ui::reveal_in_file_manager(&f),
            Action::RevokeFolder(f) => {
                self.prefs.disk_folders.retain(|x| x != &f);
                self.prefs.bookmarks.remove(&f);
                let _ = self.prefs.save();
            }
            Action::Quit => {
                if self.cfg_dirty {
                    self.flush_machine();
                }
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }

            // --- Machine ---
            Action::Start => self.start_emulator(),
            Action::Stop => self.request_stop(),
            Action::Reset => {
                self.emu.send(Cmd::Stop);
                self.start_emulator();
            }
            Action::ResetNvram => {
                // Indigo2/IP28 read eaddr and PROM env from the NVRAM EEPROM
                // (nveeprom), not the DS1386 nvram file below \u{2014} see
                // rules/irix/networking.md. Reset and re-MAC both, so this
                // works regardless of machine profile. `toast` holds one
                // message, so collect outcomes into a single final call.
                let seed = self.prefs.active_machine.as_deref().unwrap_or("indy");
                let mac = settings::generate_mac_bytes(seed);
                let mut errors = Vec::new();
                match settings::reset_nvram(&self.cfg.nvram) {
                    Ok(()) => { let _ = settings::write_nvram_mac(&self.cfg.nvram, mac); }
                    Err(e) => errors.push(t!("toast-nvram-reset-failed", error = e.to_string())),
                }
                match settings::reset_nveeprom(&self.cfg.nveeprom) {
                    Ok(()) => { let _ = settings::write_nveeprom_mac(&self.cfg.nveeprom, mac); }
                    Err(e) => errors.push(t!("toast-nveeprom-reset-failed", error = e.to_string())),
                }
                self.toast(if errors.is_empty() {
                    t!("toast-nvram-reset", mac = settings::mac_to_string(mac))
                } else {
                    // One line: the toast is shown in the window title.
                    errors.join(" ")
                });
            }
            Action::SetCpu(c) => {
                self.cfg.machine.cpu = c;
                self.mark_dirty();
                self.toast(t!("toast-cpu-set", cpu = c.label()));
            }
            Action::SaveState(slot) => self.emu.send(Cmd::SaveState(slot)),
            Action::RestoreState(slot) => self.emu.send(Cmd::RestoreState(slot)),
            Action::Screenshot => {
                if let Some(p) = crate::native_save_dialog(&t!("menu-machine-screenshot-title"), &[("PNG", &["png"])]) {
                    self.emu.send(Cmd::Screenshot(p));
                }
            }
            Action::SerialConsole => self.open_serial_console(),
            Action::ToggleCapture => {
                if self.input_state.captured {
                    crate::input::force_release(ctx, &mut self.input_state);
                } else {
                    crate::input::engage_capture(ctx, &mut self.input_state);
                }
            }

            // --- Memory ---
            Action::SetRam(mb) => {
                self.cfg.banks = crate::distribute_ram(mb, self.cfg.machine.profile.ip28());
                self.mark_dirty();
                self.toast(t!(
                    "toast-ram-set",
                    ram = crate::ram_summary(&self.cfg.banks),
                    banks = format!("{:?}", self.cfg.banks)
                ));
            }
            Action::SetBank(i, sz) => {
                self.cfg.banks[i] = sz;
                self.mark_dirty();
            }

            // --- SCSI ---
            Action::Scsi(cmd) => self.apply_scsi(cmd),
            Action::CowCommit { base, chd } => {
                if chd {
                    // A CHD commit recompresses — show the progress modal.
                    self.syncing = Some(crate::SyncJob { disk: 0, total: 1, fraction: 0.0 });
                }
                self.emu.send(Cmd::CowCommit { base, chd });
            }
            Action::CowDiscard { id, base, chd } => {
                // Destructive — confirm before discarding.
                self.cow_discard_confirm = Some(crate::CowDiscard { id, base, chd });
            }

            // --- View ---
            Action::ToggleFullscreen => {
                self.toggle_fullscreen(ctx);
            }
            Action::SetVmScale(s) => {
                self.prefs.vm_scale = s;
                self.pending_fb_snap = true;
                let _ = self.prefs.save();
            }
            Action::SetUiScale(s) => {
                self.prefs.ui_scale = s;
                ctx.set_zoom_factor(s);
                // Re-fit the window so bigger/smaller controls grow the window
                // rather than squeezing the picture.
                self.pending_fb_snap = true;
                let _ = self.prefs.save();
            }
            Action::SetLanguage(code) => {
                self.prefs.language = code;
                crate::i18n::set_language(&self.prefs.language);
                if let Err(e) = self.prefs.save() {
                    self.toast(e);
                }
                self.invalidate_menus();
            }
            Action::ShowConfig => self.show_config_editor = true,

            // --- Help ---
            Action::NetCheck => self.show_net_check = true,
            Action::CameraTest => self.open_camera_test(),
            Action::HelpInfo => self.show_help_info = true,
            Action::NfsHelp => self.show_nfs_help = true,
            Action::Ultra64Help => self.show_ultra64_help = true,
            Action::License => self.show_license = true,
            Action::Privacy => self.show_privacy = true,
            Action::About => self.native.show_about = true,
            Action::OpenUrl(url) => open_url(url),
        }
    }

    fn apply_scsi(&mut self, cmd: ScsiCmd) {
        use crate::handle::Cmd;
        use crate::scsi_menu::{self as sm, ScsiAction};
        // The picker runs here rather than inside the menu, so it opens after
        // the menu has closed instead of underneath it.
        let picked = match cmd {
            ScsiCmd::PickHdd(id) => {
                let cur = self.cfg.scsi.get(&id).map(|d| d.path.clone()).unwrap_or_default();
                let title = if cur.is_empty() {
                    t!("scsi-attach-hdd-picker")
                } else {
                    t!("scsi-replace-image-picker")
                };
                sm::pick_disk(&title, &cur).map(|path| ScsiAction::AttachHdd { id, path })
            }
            ScsiCmd::AttachEmptyCdrom(id) => Some(ScsiAction::AttachEmptyCdrom { id }),
            ScsiCmd::PickCdromWithDisc(id) => sm::pick_iso(&t!("scsi-attach-cdrom-disc-picker"), "")
                .map(|path| ScsiAction::AttachCdromWithDisc { id, path }),
            ScsiCmd::PickDisc(id) => {
                let cur = self.cfg.scsi.get(&id).map(|d| d.path.clone()).unwrap_or_default();
                sm::pick_iso(&t!("scsi-insert-disc-picker"), &cur).map(|path| ScsiAction::InsertDisc { id, path })
            }
            ScsiCmd::Eject(id) => Some(ScsiAction::Eject { id }),
            ScsiCmd::Detach(id) => Some(ScsiAction::Detach { id }),
            ScsiCmd::ToggleOverlay(id) => Some(ScsiAction::ToggleOverlay { id }),
            ScsiCmd::CreateBlank(id) => {
                self.create_disk.open_for(id);
                None
            }
            ScsiCmd::Remount(id) => {
                if self.emu.is_running() {
                    self.emu.send(Cmd::RemountCdrom { id });
                    self.toast(t!("toast-cdrom-remount-sent", id = id));
                } else {
                    self.toast(t!("toast-cdrom-remount-not-running"));
                }
                None
            }
        };
        let Some(action) = picked else { return };

        // Media changes reach a running machine live; a new *drive* needs a
        // Stop → Start, since the SCSI bus is built at Machine::new.
        let live = match &action {
            ScsiAction::InsertDisc { id, path } | ScsiAction::AttachCdromWithDisc { id, path } => {
                Some(Cmd::LoadDisc { id: *id, path: path.clone(), remount: true })
            }
            ScsiAction::Eject { id } => Some(Cmd::EjectCdrom { id: *id }),
            _ => None,
        };
        let was_insert = matches!(action, ScsiAction::InsertDisc { .. });
        if let Some(msg) = sm::apply(&mut self.cfg, action) {
            self.mark_dirty();
            self.toast(msg);
        }
        match (live, self.emu.is_running()) {
            (Some(cmd), true) => self.emu.send(cmd),
            (Some(_), false) if was_insert => {
                self.toast(t!("toast-disc-saved"))
            }
            _ => {}
        }
    }
}

/// Open `url` in the user's browser.
fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}
