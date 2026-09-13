// Original sidebar layout from upstream aac7674, used outside macOS.
use super::*;

impl App {
    pub(super) fn draw_cow_menu(&mut self, ui: &mut egui::Ui) {
        // Snapshot disks that currently have an overlay on disk (cloned so we
        // don't hold a `cfg` borrow while sending worker commands below).
        let mut entries: Vec<(u8, String, bool)> = Vec::new(); // (id, base, is_chd)
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
        if entries.is_empty() {
            return;
        }
        ui.separator();
        ui.label(RichText::new("Copy-on-write changes").strong());
        if self.emu.is_running() {
            ui.label(RichText::new("Stop the machine to commit or roll back.").weak());
            return;
        }
        for (id, base, is_chd) in entries {
            let name = std::path::Path::new(&base).file_name().and_then(|n| n.to_str()).unwrap_or(&base);
            ui.label(RichText::new(format!("SCSI {id}: {name}")).weak());
            if ui.button("    ⬇ Commit changes to disk")
                .on_hover_text("Permanently merge this session's overlay into the disk image")
                .clicked()
            {
                if is_chd {
                    // A CHD commit recompresses — show the progress modal.
                    self.syncing = Some(SyncJob { disk: 0, total: 1, fraction: 0.0 });
                }
                self.emu.send(Cmd::CowCommit { base: base.clone(), chd: is_chd });
                ui.close();
            }
            if ui.button("    ↩ Discard changes (roll back)")
                .on_hover_text("Throw away this session's overlay and revert to the disk as it was")
                .clicked()
            {
                // Destructive — confirm before discarding.
                self.cow_discard_confirm = Some(CowDiscard { id, base: base.clone(), chd: is_chd });
                ui.close();
            }
        }
    }

    pub(super) fn menu_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.vertical(|ui| {
            ui.menu_button("File  ▶", |ui| {
                ui.set_min_width(220.0);
                if ui.button("New machine…").clicked() {
                    self.new_machine.open();
                    ui.close();
                }
                ui.menu_button("Switch to machine", |ui| {
                    ui.set_min_width(200.0);
                    let active = self.prefs.active_machine.clone();
                    let names: Vec<String> = self.prefs.machines.keys().cloned().collect();
                    if names.is_empty() {
                        ui.label(RichText::new("(no saved machines yet)").weak());
                    }
                    let mut want_switch: Option<String> = None;
                    for name in names {
                        // selectable_label highlights the active machine — no
                        // marker glyph (a leading ● rendered as tofu).
                        let is_active = active.as_deref() == Some(name.as_str());
                        if ui.selectable_label(is_active, name.as_str()).clicked() {
                            want_switch = Some(name);
                            ui.close();
                        }
                    }
                    if let Some(n) = want_switch { self.switch_to(&n); }
                });
                if ui.add_enabled(self.prefs.active_machine.is_some(),
                        egui::Button::new("Rename current…"))
                    .on_disabled_hover_text("No active machine")
                    .clicked()
                {
                    // Open the rename modal seeded with the current name. (A
                    // text box inside the menu can't work — the menu closure
                    // re-runs each frame and would reset the buffer.)
                    self.rename_buffer = self.prefs.active_machine.clone();
                    ui.close();
                }
                let active = self.prefs.active_machine.clone();
                if ui.add_enabled(active.is_some(), egui::Button::new("Delete current machine")).clicked() {
                    if let Some(name) = active {
                        self.prefs.machines.remove(&name);
                        self.prefs.active_machine = self.prefs.machines.keys().next().cloned();
                        if let Some(next) = self.prefs.active_machine.clone() {
                            self.cfg = self.prefs.machines[&next].clone();
                        } else {
                            self.cfg = MachineConfig::default();
                            self.new_machine.open();
                        }
                        let _ = self.prefs.save();
                        self.toast(format!("deleted '{name}'"));
                    }
                    ui.close();
                }
                // iris.toml import/export is a source-build affordance for users
                // who also run the standalone `iris` CLI; the GUI's own gui.json
                // machine store is the system of record. Hidden in pre-compiled /
                // App Store builds (the `bundled` feature). See iris-gui Cargo.toml.
                if !cfg!(feature = "bundled") {
                    ui.separator();
                    if ui.button("Import iris.toml…").clicked() {
                        if let Some(path) = native_open_dialog("Import iris.toml", &[("TOML", &["toml"])]) {
                            let cfg = MachineConfig::load_toml(&path.to_string_lossy());
                            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("imported");
                            let name = self.prefs.unique_name(stem);
                            self.prefs.machines.insert(name.clone(), cfg.clone());
                            self.prefs.active_machine = Some(name.clone());
                            self.cfg = cfg;
                            self.cfg_path = Some(path);
                            self.flush_machine();
                            self.toast(format!("imported as '{name}'"));
                        }
                        ui.close();
                    }
                    if ui.button("Export current to iris.toml…").clicked() {
                        if let Some(path) = native_save_dialog("Export iris.toml", &[("TOML", &["toml"])]) {
                            self.save_config(path);
                        }
                        ui.close();
                    }
                    if ui.button("Prepare for premiere…").clicked() {
                        self.prepare_for_premiere();
                        ui.close();
                    }
                }
                // App Store sandbox: grant a whole folder (recursive) so the
                // disk-sync fold — which writes a temp beside the base and
                // renames over it — works, and so a disk image / NFS shared
                // subfolder under it is covered by one grant. Hidden elsewhere.
                if cfg!(feature = "appstore") {
                    ui.separator();
                    ui.label(RichText::new("Disk folder access").strong());
                    if ui.button("Grant a disk folder…")
                        .on_hover_text("Pick the folder your disk images live in. Grants read/write to \
                                        everything inside (disk images, CHD diffs, an NFS shared subfolder) \
                                        so changes can be synced back into the disk.")
                        .clicked()
                    {
                        self.grant_disk_folder();
                        ui.close();
                    }
                    let folders = self.prefs.disk_folders.clone();
                    if folders.is_empty() {
                        ui.label(RichText::new("(no folders granted yet)").weak().small());
                    }
                    for f in &folders {
                        ui.horizontal(|ui| {
                            // Live access state: green bullet if the grant is in
                            // effect this session, red if it lapsed (re-grant to
                            // fix). U+2022 (bullet) renders; U+25CF (●) and the
                            // check/cross dingbats are tofu in egui's label font.
                            let live = folder_accessible(f);
                            let (color, tip) = if live {
                                (Color32::from_rgb(120, 200, 120), "Access is active — IRIS can read/write here")
                            } else {
                                (Color32::from_rgb(220, 140, 90), "No access right now — re-grant this folder")
                            };
                            ui.label(RichText::new("\u{2022}").size(15.0).color(color)).on_hover_text(tip);
                            ui.label(RichText::new(f).weak());
                            if ui.small_button("📂").on_hover_text("Reveal in Finder").clicked() {
                                config_ui::reveal_in_file_manager(f);
                            }
                            if ui.small_button("\u{00D7}").on_hover_text("Revoke").clicked() {
                                self.prefs.disk_folders.retain(|x| x != f);
                                self.prefs.bookmarks.remove(f);
                                let _ = self.prefs.save();
                            }
                        });
                    }
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    if self.cfg_dirty { self.flush_machine(); }
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });
            ui.menu_button("Machine  ▶", |ui| {
                let running = self.emu.is_running();
                if ui.add_enabled(!running, egui::Button::new("Start")).clicked() {
                    self.start_emulator();
                    ui.close();
                }
                if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                    self.request_stop();
                    ui.close();
                }
                if ui.add_enabled(running, egui::Button::new("Reset")).clicked() {
                    self.emu.send(Cmd::Stop);
                    self.start_emulator();
                    ui.close();
                }
                if ui.add_enabled(!running, egui::Button::new("Reset NVRAM (fresh PRAM)"))
                    .on_hover_text(format!(
                        "Restore this machine's NVRAM to defaults and assign a fresh Ethernet MAC.\n{}",
                        abs_path(&self.cfg.nvram)))
                    .clicked()
                {
                    match settings::reset_nvram(&self.cfg.nvram) {
                        Ok(()) => {
                            let seed = self.prefs.active_machine.as_deref().unwrap_or("indy");
                            let mac = settings::generate_mac_bytes(seed);
                            let _ = settings::write_nvram_mac(&self.cfg.nvram, mac);
                            self.toast(format!("NVRAM reset — new MAC {}", settings::mac_to_string(mac)));
                        }
                        Err(e) => self.toast(format!("NVRAM reset failed: {e}")),
                    }
                    ui.close();
                }
                ui.separator();
                // Chosen at Machine::new, so an edit while running is pending, not live —
                // same shape as the Memory menu's RAM rows.
                ui.set_min_width(240.0);
                ui.label(RichText::new(format!("Processor: {}", self.cfg.machine.cpu.label())).strong());
                ui.add_enabled_ui(!running, |ui| {
                    for c in iris::config::CpuModel::ALL {
                        if ui.radio(self.cfg.machine.cpu == c, c.label()).clicked() {
                            self.cfg.machine.cpu = c;
                            self.mark_dirty();
                            self.toast(format!("{} — applies at next Start", c.label()));
                        }
                    }
                });
                if running {
                    match self.started_cpu {
                        Some(started) if started != self.cfg.machine.cpu => {
                            ui.label(RichText::new(format!(
                                "Running: {} (Stop to apply edits)", started.label()))
                                .color(Color32::YELLOW));
                        }
                        Some(started) => {
                            ui.label(RichText::new(format!("Running: {}", started.label())).weak());
                        }
                        None => {}
                    }
                    ui.label(RichText::new("CPU changes apply after Stop → Start").weak().small());
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Save state:");
                    ui.add(egui::TextEdit::singleline(&mut self.save_state_name).desired_width(120.0));
                    if ui.button("Save").clicked() {
                        self.emu.send(Cmd::SaveState(self.save_state_name.clone()));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Restore state:");
                    ui.add(egui::TextEdit::singleline(&mut self.restore_state_name).desired_width(120.0));
                    if ui.button("Restore").clicked() {
                        self.emu.send(Cmd::RestoreState(self.restore_state_name.clone()));
                    }
                });
                ui.separator();
                if ui.button("Screenshot…").clicked() {
                    if let Some(p) = native_save_dialog("Save screenshot", &[("PNG", &["png"])]) {
                        self.emu.send(Cmd::Screenshot(p));
                    }
                    ui.close();
                }
                if ui.add_enabled(running, egui::Button::new("Serial console…"))
                    .on_hover_text("View the IRIX serial console (ttyd1) over the loopback serial server")
                    .clicked()
                {
                    self.open_serial_console();
                    ui.close();
                }
            });
            ui.menu_button("Memory  ▶", |ui| {
                ui.set_min_width(220.0);
                let running = self.emu.is_running();
                let summary = ram_summary(&self.cfg.banks);
                ui.label(RichText::new(format!("Config: {summary}")).strong());
                if running {
                    if let Some(started) = self.started_banks {
                        if started != self.cfg.banks {
                            ui.label(
                                RichText::new(format!(
                                    "Running: {} (Stop to apply edits)",
                                    ram_summary(&started)
                                ))
                                .color(Color32::YELLOW),
                            );
                        } else {
                            ui.label(RichText::new(format!("Running: {}", ram_summary(&started))).weak());
                        }
                    }
                    ui.label(
                        RichText::new("RAM changes apply after Stop → Start")
                            .weak()
                            .small(),
                    );
                } else {
                    ui.label(RichText::new("Applied at next Start").weak().small());
                }
                ui.separator();
                ui.label("Quick presets (auto-distributed):");
                for &p in RAM_PRESETS {
                    if ui
                        .add_enabled(!running, egui::Button::new(format!("{p} MB")))
                        .on_disabled_hover_text("Stop the VM to change RAM")
                        .clicked()
                    {
                        self.cfg.banks = distribute_ram(p);
                        self.mark_dirty();
                        self.toast(format!("RAM set to {} ({:?})", ram_summary(&self.cfg.banks), self.cfg.banks));
                        ui.close();
                    }
                }
                ui.separator();
                ui.label("Per-bank (advanced):");
                for i in 0..4 {
                    ui.menu_button(format!("Bank {i}: {} MB", self.cfg.banks[i]), |ui| {
                        for &sz in iris::config::VALID_BANK_SIZES {
                            if ui
                                .add_enabled(!running, egui::Button::new(format!("{sz} MB")))
                                .on_disabled_hover_text("Stop the VM to change RAM")
                                .clicked()
                            {
                                self.cfg.banks[i] = sz;
                                self.mark_dirty();
                                ui.close();
                            }
                        }
                    });
                }
            });
            ui.menu_button("SCSI  ▶", |ui| {
                let action = scsi_menu::draw(ui, &self.cfg);
                match action {
                    scsi_menu::ScsiAction::None => {}
                    scsi_menu::ScsiAction::CreateBlank { id } => {
                        self.create_disk.open_for(id);
                    }
                    scsi_menu::ScsiAction::InsertDisc { id, path } => {
                        if let Some(msg) = scsi_menu::apply(
                            &mut self.cfg,
                            scsi_menu::ScsiAction::InsertDisc { id, path: path.clone() },
                        ) {
                            self.mark_dirty();
                            self.toast(msg);
                        }
                        if self.emu.is_running() {
                            self.emu.send(Cmd::LoadDisc { id, path, remount: true });
                        } else {
                            self.toast("Disc saved — Stop→Start to load into SCSI drive");
                        }
                    }
                    scsi_menu::ScsiAction::AttachCdromWithDisc { id, path } => {
                        if let Some(msg) = scsi_menu::apply(
                            &mut self.cfg,
                            scsi_menu::ScsiAction::AttachCdromWithDisc { id, path: path.clone() },
                        ) {
                            self.mark_dirty();
                            self.toast(msg);
                        }
                        if self.emu.is_running() {
                            self.emu.send(Cmd::LoadDisc { id, path, remount: true });
                        }
                    }
                    scsi_menu::ScsiAction::Eject { id } => {
                        if let Some(msg) = scsi_menu::apply(
                            &mut self.cfg,
                            scsi_menu::ScsiAction::Eject { id },
                        ) {
                            self.mark_dirty();
                            self.toast(msg);
                        }
                        if self.emu.is_running() {
                            self.emu.send(Cmd::EjectCdrom { id });
                        }
                    }
                    scsi_menu::ScsiAction::RemountInIrix { id } => {
                        if self.emu.is_running() {
                            self.emu.send(Cmd::RemountCdrom { id });
                            self.toast(format!(
                                "SCSI #{id}: remount sent — keep a shell focused on the console"
                            ));
                        } else {
                            self.toast("Mount /CDROM: start the VM first");
                        }
                    }
                    other => {
                        if let Some(msg) = scsi_menu::apply(&mut self.cfg, other) {
                            self.mark_dirty();
                            self.toast(msg);
                        }
                    }
                }
                self.draw_cow_menu(ui);
            });
            ui.menu_button("View  ▶", |ui| {
                if ui.button(if self.fullscreen { "Exit fullscreen (F11)" } else { "Fullscreen (F11)" }).clicked() {
                    self.toggle_fullscreen(ctx);
                    ui.close();
                }
                ui.horizontal(|ui| {
                    ui.label("UI scale");
                    // Adjust the slider freely; only commit to the live zoom
                    // factor on Apply. Applying mid-drag rescales the whole UI
                    // (slider included) under the cursor, which makes the value
                    // jump around — hence the explicit button.
                    ui.add(egui::Slider::new(&mut self.prefs.ui_scale, UI_SCALE_MIN..=UI_SCALE_MAX));
                    if ui.button("Apply").clicked() {
                        ctx.set_zoom_factor(self.prefs.ui_scale);
                        // Re-fit the window so the bigger/smaller controls grow
                        // the window rather than squeezing the picture.
                        self.pending_fb_snap = true;
                    }
                });
                ui.label(RichText::new("Ctrl+= / Ctrl+- / Ctrl+0 to zoom").weak().small());
                ui.separator();
                ui.menu_button("Graphics scaling", |ui| {
                    let mut changed = false;
                    changed |= ui.selectable_value(&mut self.prefs.display_scaling,
                        settings::DisplayScaling::NearestInteger, "Nearest integer").changed();
                    changed |= ui.selectable_value(&mut self.prefs.display_scaling,
                        settings::DisplayScaling::Stretch, "Stretch").changed();
                    changed |= ui.add_enabled(
                        self.prefs.display_scaling == settings::DisplayScaling::Stretch,
                        egui::Checkbox::new(&mut self.prefs.keep_aspect_ratio, "Keep aspect ratio")
                    ).changed();
                    if changed { let _ = self.prefs.save(); }
                });
                ui.horizontal(|ui| {
                    ui.label("VM screen");
                    // Sets the emulated-display magnification directly (1× =
                    // native), independent of UI scale. The window resizes to
                    // hold the picture at the chosen size on the next frame.
                    let changed = ui.add(
                        egui::Slider::new(&mut self.prefs.vm_scale, settings::VM_SCALE_MIN..=settings::VM_SCALE_MAX)
                            .step_by(settings::VM_SCALE_STEP)
                            .suffix("×"),
                    ).changed();
                    if changed { self.pending_fb_snap = true; }
                });
                ui.label(RichText::new("1× = native pixels; ¼× steps (½-integers crispest on Retina)").weak().small());
            });
            ui.menu_button("Help  ▶", |ui| {
                ui.label(RichText::new("IRIS — SGI Indy (MIPS R4400) Emulator").strong());
                ui.label(format!("Version {}", env!("APP_VERSION")));
                ui.separator();
                ui.label(RichText::new("Diagnostics").strong());
                let running = self.emu.is_running();
                if ui.add_enabled(running, egui::Button::new("📷 Test Camera…"))
                    .on_hover_text("Preview the host camera used for the emulated IndyCam")
                    .on_disabled_hover_text("Start a machine first")
                    .clicked()
                {
                    self.open_camera_test();
                    ui.close();
                }
                if ui.add_enabled(running, egui::Button::new("Serial console…"))
                    .on_hover_text("Connect to the emulator's loopback serial server (127.0.0.1:8881)")
                    .on_disabled_hover_text("Start a machine first")
                    .clicked()
                {
                    self.open_serial_console();
                    ui.close();
                }
                if ui.button("ℹ How camera & networking work…").clicked() {
                    self.show_help_info = true;
                    ui.close();
                }
                if ui.button("📂 Mount the shared folder in IRIX…")
                    .on_hover_text("The exact mount command for the NFS share")
                    .clicked()
                {
                    self.show_nfs_help = true;
                    ui.close();
                }
                // N64 dev board getting-started guide. Only in builds that carry
                // the board (source builds with --features ultra64), and never in
                // App Store builds, where it can't run anyway (sandbox blocks the
                // POSIX shm bridge and there's no way to run the external gopher64).
                #[cfg(all(feature = "ultra64", not(feature = "appstore")))]
                if ui.button("🎮 N64 development board (Ultra64)…")
                    .on_hover_text("How to set up the N64 devkit and run ROMs with gload")
                    .clicked()
                {
                    self.show_ultra64_help = true;
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new("Legal").strong());
                if ui.button("Licenses…")
                    .on_hover_text("BSD 3-Clause (IRIS, and the CHD backend when built in)")
                    .clicked()
                {
                    self.show_license = true;
                    ui.close();
                }
                if ui.button("Privacy policy…").clicked() {
                    self.show_privacy = true;
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new("Authors").strong());
                ui.label("Original: techomancer");
                ui.label("iris-gui fork: Dani Sarfati (danifunker)");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Upstream:");
                    ui.hyperlink_to("techomancer/iris", "https://github.com/techomancer/iris");
                });
                ui.horizontal(|ui| {
                    ui.label("This fork:");
                    ui.hyperlink_to("danifunker/iris", "https://github.com/danifunker/iris");
                });
                ui.separator();
                ui.label(RichText::new("Build features:").strong());
                use iris::build_features as bf;
                ui.label(format!("  chd:       {}", if bf::CHD { "on" } else { "off" }));
                ui.label(format!("  camera:    {}", if bf::CAMERA { "on" } else { "off" }));
                // rex-jit is a compile-time feature, but the sandbox (App
                // Store) build forces interpreter-only at runtime via IRIS_NO_JIT
                // (Cranelift's non-MAP_JIT pages get killed under the sandbox).
                // Report the runtime reality so a compiled-in "on" doesn't read
                // as "the JIT is running" when it can't be.
                let jit_off = std::env::var_os("IRIS_NO_JIT").is_some();
                let jit_state = |feat: bool| if !feat { "off" } else if jit_off { "off (sandbox)" } else { "on" };
                ui.label(format!("  rex-jit:   {}", jit_state(bf::REX_JIT)));
                ui.label(format!("  lightning: {}", if bf::LIGHTNING { "on (no debug)" } else { "off" }));
                // ultra64 (N64 dev board) is a source-build opt-in; shipped builds
                // don't carry it, and the App Store sandbox couldn't open its
                // POSIX shm bridge even if they did.
                ui.label(format!("  ultra64:   {}", if bf::ULTRA64 { "on" } else { "off" }));
            });
        });
    }

    pub(super) fn machine_controls(&mut self, ui: &mut egui::Ui) {
        let running = self.emu.is_running();
        let full = egui::vec2(ui.available_width(), 0.0);
        if !running {
            if ui.add_sized(full, egui::Button::new(RichText::new("▶ Start").size(16.0))
                .fill(Color32::from_rgb(40, 110, 40))).clicked()
            {
                self.start_emulator();
            }
        } else if ui.add_sized(full, egui::Button::new(RichText::new("■ Stop").size(16.0))
            .fill(Color32::from_rgb(160, 60, 60))).clicked()
        {
            self.request_stop();
        }
        if ui.add_enabled_ui(running, |ui| {
            ui.add_sized(egui::vec2(ui.available_width(), 0.0), egui::Button::new("💾 Save state")).clicked()
        }).inner {
            self.emu.send(Cmd::SaveState(self.save_state_name.clone()));
        }
        if ui.add_enabled_ui(running, |ui| {
            ui.add_sized(egui::vec2(ui.available_width(), 0.0), egui::Button::new("Restore state")).clicked()
        }).inner {
            self.emu.send(Cmd::RestoreState(self.restore_state_name.clone()));
        }
    }

    pub(super) fn config_quick_buttons(&mut self, ui: &mut egui::Ui) {
        let edit_label = if self.show_config_editor { "Hide config editor" } else { "Edit config…" };
        if ui.add_sized(egui::vec2(ui.available_width(), 0.0), egui::Button::new(edit_label)).clicked() {
            self.show_config_editor = !self.show_config_editor;
        }
        if self.show_config_editor {
            ui.indent("quick_tabs", |ui| {
                if ui.button("Network").clicked()  { self.tab = Tab::Network; }
                if ui.button("Video-In").clicked() { self.tab = Tab::VideoIn; }
                // Debug/JIT is compiled out of lightning builds; CI is hidden
                // in App Store builds — keep the quick-buttons in step with
                // Tab::visible() so they never jump to a hidden tab.
                if !iris::build_features::LIGHTNING && ui.button("Debug").clicked() {
                    self.tab = Tab::Debug;
                }
                if !cfg!(feature = "appstore") && ui.button("CI").clicked() {
                    self.tab = Tab::Ci;
                }
            });
        }
    }

    pub(super) fn capture_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.input_state.captured {
            ui.label(RichText::new("Mouse/Keyboard Captured").color(Color32::LIGHT_GREEN));
            ui.label(RichText::new(format!("To disable: {}", input::RELEASE_HINT)).weak());
            ui.label(RichText::new("Send F11 to IRIX: Ctrl+Alt+F11").weak());
        } else {
            ui.label(RichText::new("Mouse/Keyboard Capture Disabled").weak());
            if ui
                .add_sized(
                    egui::vec2(ui.available_width(), 0.0),
                    egui::Button::new("Capture mouse/keyboard"),
                )
                .clicked()
            {
                input::engage_capture(ctx, &mut self.input_state);
            }
        }
    }

    pub(super) fn run_state_label(&mut self, ui: &mut egui::Ui) {
        let running = self.emu.is_running();
        let halted = running && self.emu.status.cpu_halted;
        let status = if halted {
            RichText::new("halted — safe to stop").color(Color32::LIGHT_GRAY)
        } else if self.emu.status.in_prom {
            RichText::new("PROM").color(Color32::LIGHT_BLUE)
        } else if running {
            RichText::new("IRIX running").color(Color32::LIGHT_GREEN)
        } else {
            RichText::new("stopped").color(Color32::GRAY)
        };
        ui.label(status);
        if running && !halted {
            ui.label(format!("{:.0} MIPS", self.emu.status.mips))
                .on_hover_text(
                    "MIPS = instructions per wall-clock second on your PC (real emulation speed).\n\
                     IRIX System Manager \"MHz\" from hinv is inventory from the PROM — not host performance.",
                );
        }
        // Networking indicator — ONE badge that shows both liveness (the dot's
        // colour) and the active backend (the label: NAT / PCAP). Grey while
        // unpowered/halted; green once the guest is carrying IP traffic, red when
        // a running guest has produced none. Works for both backends — the PCAP
        // engine counts guest IP frames too. The backend is latched on Start
        // (launched_net), not the live editor.
        use iris::config::NetMode;
        let backend = self.launched_net.clone();
        let state = self.emu.net_state();
        let net_color = match state {
            NetState::Active => Color32::from_rgb(0x35, 0xb8, 0x4a),
            NetState::Idle => Color32::from_rgb(0xd9, 0x4a, 0x3d),
            NetState::Off => Color32::from_gray(0x80),
        };
        let (net_label, net_tip): (&str, String) = match (running, backend.as_ref()) {
            (true, Some((NetMode::Pcap, iface))) => {
                let iface = iface.as_deref().filter(|s| !s.is_empty()).unwrap_or("auto");
                let tip = match state {
                    NetState::Active => format!("Bridged (PCAP) on {iface}: guest is carrying IP traffic."),
                    NetState::Idle => format!("Bridged (PCAP) on {iface}: no guest IP traffic yet.\nCheck the guest's IP / DHCP — wired connections only."),
                    NetState::Off => "Bridged (PCAP) networking: machine not running.".into(),
                };
                ("PCAP", tip)
            }
            (true, Some((NetMode::Nat, _))) => {
                let tip = match state {
                    NetState::Active => "Internal NAT network: carrying guest traffic.".into(),
                    NetState::Idle => "Internal NAT network: no traffic yet.\nThe guest may have no IP — or the wrong IP for this NAT subnet.".into(),
                    NetState::Off => "Internal NAT network: machine not running.".into(),
                };
                ("NAT", tip)
            }
            _ => ("NET", "Internal network: machine not running.".into()),
        };
        // "check" diagnoses the guest IP against the NAT subnet — only meaningful
        // in NAT mode, so it's hidden in PCAP.
        let nat_running = running && matches!(backend.as_ref(), Some((NetMode::Nat, _)));
        let mut want_check = false;
        ui.horizontal(|ui| {
            // U+2022 (bullet) as a status light. NOT U+25CF (●), which renders as
            // tofu in egui's font chain. See rules/gui/egui-…-tofu.md.
            ui.label(RichText::new("\u{2022}").size(18.0).color(net_color)).on_hover_text(net_tip.as_str());
            ui.label(net_label).on_hover_text(net_tip.as_str());
            if nat_running && ui.small_button("check").on_hover_text("Diagnose guest networking").clicked() {
                want_check = true;
            }
        });
        if want_check {
            self.show_net_check = true;
        }
        if running && self.fb_scale > 0.0 {
            // How magnified the emulated display currently is (1× = native).
            // Round-snap the readout so a whole-number scale reads cleanly.
            let mag = self.fb_scale;
            let whole = (mag - mag.round()).abs() <= 0.01;
            let num = if whole { format!("{:.0}×", mag.round()) } else { format!("{mag:.2}×") };
            let label = if self.fb_nearest {
                RichText::new(format!("{num} scale")).color(Color32::LIGHT_GRAY)
            } else {
                RichText::new(format!("{num} scale · filtered")).color(Color32::from_rgb(200, 175, 90))
            };
            ui.label(label).on_hover_text(
                "On-screen size of the emulated display: logical points per emulated \
                 pixel (1× = native). \"filtered\" means the current size isn't a whole \
                 device-pixel multiple, so the image is smoothed rather than pixel-crisp.",
            );
        }
    }

    pub(super) fn config_editor_panel(&mut self, ui: &mut egui::Ui) {
        let machine = self.prefs.active_machine.as_deref().unwrap_or("default").to_string();
        ui.horizontal(|ui| {
            ui.heading(format!("Configuration — {machine}"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("×").on_hover_text("Hide config editor").clicked() {
                    self.show_config_editor = false;
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| self.central_tabs(ui));
    }

    pub(super) fn status_block(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        self.run_state_label(ui);
        ui.separator();
        let name = self.prefs.active_machine.as_deref().unwrap_or("(unsaved)");
        let profile = self.cfg.machine.profile.label();
        // While running, name the CPU the guest actually booted on — the config
        // may already have been changed to one that only takes effect next Start.
        let running_cpu = if self.emu.is_running() { self.started_cpu } else { None };
        let cpu = running_cpu.unwrap_or(self.cfg.machine.cpu);
        ui.label(format!(
            "Machine: {name} · {profile} · {}{}",
            cpu.label(),
            if self.cfg_dirty { " *" } else { "" }
        ));
        if let Some(started) = running_cpu {
            if started != self.cfg.machine.cpu {
                ui.label(
                    RichText::new(format!("{} pending (Stop to apply)", self.cfg.machine.cpu.label()))
                        .color(Color32::YELLOW).small(),
                );
            }
        }
        ui.label(format!("Dirty COW: {}", self.emu.status.dirty_cow));
        if let Some((msg, when)) = self.toast.clone() {
            if when.elapsed().as_secs() < 5 {
                ui.add_space(2.0);
                ui.label(RichText::new(msg).color(Color32::YELLOW));
            } else {
                self.toast = None;
            }
        }
        ui.add_space(2.0);
    }

    pub(super) fn control_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Panel::bottom("ctl_status")
            .show(ui, |ui| self.status_block(ui));

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(4.0);
            self.machine_controls(ui);
            ui.separator();
            self.menu_list(ui, ctx);
            ui.separator();
            self.config_quick_buttons(ui);
            // Capture status + "Capture" button — only while a machine is up
            // (it's meaningless at the stopped state shown in the footer).
            if self.emu.is_running() {
                ui.separator();
                self.capture_controls(ui, ctx);
            }
        });
    }
}
