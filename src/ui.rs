use crate::actions::{
    apply_auto_select, available_drives, clear_selection, format_bytes, format_duration,
    format_timestamp, move_selected_to_recycle_bin, reveal_in_explorer, sort_groups,
};
use crate::models::{
    AutoSelectRule, DuplicateGroup, IoProfile, ScanConfig, ScanProgress, ScanResult, ScanStage,
    ScanSummary, SortBy,
};
use crate::scanner::{is_scannable_dir, run_scan};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Instant;

const MIN_SIZE_PRESETS: &[(&str, u64)] = &[
    ("> 0 B (Skip empty)", 1),
    ("≥ 1 KB", 1024),
    ("≥ 100 KB", 100 * 1024),
    ("≥ 1 MB", 1024 * 1024),
    ("≥ 10 MB", 10 * 1024 * 1024),
    ("≥ 100 MB", 100 * 1024 * 1024),
];

enum DisplayRow {
    GroupHeader {
        group_idx: usize,
    },
    FileItem {
        group_idx: usize,
        file_idx: usize,
    },
}

pub struct DupFinderApp {
    config: ScanConfig,
    drives: Vec<PathBuf>,

    // Active scan state
    scanning: bool,
    scan_started_at: Option<Instant>,
    progress: Arc<ScanProgress>,
    result_rx: Option<Receiver<ScanResult>>,

    // Scan results
    groups: Vec<DuplicateGroup>,
    summary: Option<ScanSummary>,
    last_cancelled: bool,

    // View controls
    search_query: String,
    sort_by: SortBy,
    collapsed_groups: rustc_hash::FxHashSet<String>,

    // Delete confirmation & feedback
    confirm_delete_open: bool,
    status_message: Option<(String, bool)>, // (text, is_error)
}

impl DupFinderApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_visuals(&cc.egui_ctx);
        Self {
            config: ScanConfig::default(),
            drives: available_drives(),
            scanning: false,
            scan_started_at: None,
            progress: Arc::new(ScanProgress::default()),
            result_rx: None,
            groups: Vec::new(),
            summary: None,
            last_cancelled: false,
            search_query: String::new(),
            sort_by: SortBy::WastedSpaceDesc,
            collapsed_groups: rustc_hash::FxHashSet::default(),
            confirm_delete_open: false,
            status_message: None,
        }
    }

    fn add_root(&mut self, path: PathBuf) {
        if !is_scannable_dir(&path) {
            return;
        }
        if !self.config.roots.iter().any(|p| p == &path) {
            self.config.roots.push(path);
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.config.roots.is_empty() || self.scanning {
            return;
        }

        self.groups.clear();
        self.summary = None;
        self.last_cancelled = false;
        self.collapsed_groups.clear();
        self.status_message = None;
        self.confirm_delete_open = false;

        let progress = Arc::new(ScanProgress::default());
        self.progress = Arc::clone(&progress);
        self.scanning = true;
        self.scan_started_at = Some(Instant::now());

        let config = self.config.clone();
        let (tx, rx) = mpsc::channel();
        self.result_rx = Some(rx);
        let repaint_ctx = ctx.clone();

        std::thread::spawn(move || {
            let result = run_scan(&config, progress);
            let _ = tx.send(result);
            repaint_ctx.request_repaint();
        });
    }

    fn poll_scan(&mut self) {
        if let Some(rx) = &self.result_rx {
            if let Ok(res) = rx.try_recv() {
                self.scanning = false;
                self.result_rx = None;
                self.last_cancelled = res.cancelled;
                self.summary = Some(res.summary);
                self.groups = res.groups;
                sort_groups(&mut self.groups, self.sort_by);
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        if self.scanning {
            return;
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for path in dropped {
            if path.is_dir() {
                self.add_root(path);
            } else if let Some(parent) = path.parent() {
                self.add_root(parent.to_path_buf());
            }
        }
    }

    fn selected_stats(&self) -> (usize, u64, usize) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        let mut all_selected_groups = 0usize;
        for g in &self.groups {
            if g.all_selected() {
                all_selected_groups += 1;
            }
            for f in &g.files {
                if f.selected {
                    count += 1;
                    bytes = bytes.saturating_add(f.size);
                }
            }
        }
        (count, bytes, all_selected_groups)
    }

    fn build_display_rows(&self) -> Vec<DisplayRow> {
        let query = self.search_query.trim().to_ascii_lowercase();
        let mut rows = Vec::with_capacity(self.groups.len() * 3);

        for (group_idx, group) in self.groups.iter().enumerate() {
            if !query.is_empty() {
                let matches = group.files.iter().any(|f| {
                    f.path
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(&query)
                });
                if !matches {
                    continue;
                }
            }

            rows.push(DisplayRow::GroupHeader { group_idx });
            if !self.collapsed_groups.contains(&group.hash_hex) {
                for file_idx in 0..group.files.len() {
                    rows.push(DisplayRow::FileItem {
                        group_idx,
                        file_idx,
                    });
                }
            }
        }
        rows
    }
}

impl eframe::App for DupFinderApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan();
        self.handle_dropped_files(ctx);

        if self.scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        }

        // Top panel: Folder selection & scan controls
        egui::TopBottomPanel::top("top_controls")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                self.render_top_panel(ui, ctx);
            });

        // Bottom status bar
        egui::TopBottomPanel::bottom("bottom_bar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(14, 6)),
            )
            .show(ctx, |ui| {
                self.render_bottom_bar(ui);
            });

        // Main content area
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                if self.scanning {
                    self.render_progress_panel(ui);
                } else if !self.groups.is_empty() {
                    self.render_results_panel(ui);
                } else {
                    self.render_empty_state(ui);
                }
            });

        // Delete confirmation modal
        if self.confirm_delete_open {
            self.render_confirm_delete_modal(ctx);
        }
    }
}

impl DupFinderApp {
    fn render_top_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Row 1: Title + Action button
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("DupFinder")
                    .size(20.0)
                    .strong()
                    .color(Color32::from_rgb(125, 211, 252)),
            );
            ui.label(
                RichText::new("Ultra-fast BLAKE3 duplicate finder")
                    .size(12.5)
                    .color(Color32::from_gray(150)),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.scanning {
                    let cancel_btn = egui::Button::new(
                        RichText::new("Cancel Scan").strong().color(Color32::WHITE),
                    )
                    .fill(Color32::from_rgb(185, 28, 28))
                    .min_size(Vec2::new(120.0, 30.0));
                    if ui.add(cancel_btn).clicked() {
                        self.progress.request_cancel();
                    }
                } else {
                    let can_scan = !self.config.roots.is_empty();
                    let scan_btn = egui::Button::new(
                        RichText::new("Start Scan").strong().color(Color32::WHITE),
                    )
                    .fill(if can_scan {
                        Color32::from_rgb(2, 132, 199)
                    } else {
                        Color32::from_gray(60)
                    })
                    .min_size(Vec2::new(120.0, 30.0));
                    if ui.add_enabled(can_scan, scan_btn).clicked() {
                        self.start_scan(ctx);
                    }
                }
            });
        });

        ui.add_space(6.0);

        // Row 2: Target folders & quick drive pills
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!self.scanning, |ui| {
                if ui.button("+ Add Folder").clicked() {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        self.add_root(folder);
                    }
                }

                ui.separator();
                ui.label(RichText::new("Drives:").color(Color32::from_gray(160)));
                let drives = self.drives.clone();
                for drive in drives {
                    let label = drive.to_string_lossy().to_string();
                    let already = self.config.roots.iter().any(|r| r == &drive);
                    let btn = egui::Button::new(&label).fill(if already {
                        Color32::from_rgb(30, 58, 95)
                    } else {
                        Color32::from_gray(42)
                    });
                    if ui.add(btn).clicked() {
                        if already {
                            self.config.roots.retain(|r| r != &drive);
                        } else {
                            self.add_root(drive);
                        }
                    }
                }

                if !self.config.roots.is_empty() {
                    ui.separator();
                    if ui.small_button("Clear Targets").clicked() {
                        self.config.roots.clear();
                    }
                }
            });
        });

        // Selected target chips
        if !self.config.roots.is_empty() {
            ui.add_space(4.0);
            let mut remove_idx = None;
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Targets:").color(Color32::from_gray(160)));
                for (idx, root) in self.config.roots.iter().enumerate() {
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(30, 41, 59))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(51, 65, 85)))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(root.to_string_lossy())
                                        .size(12.0)
                                        .color(Color32::from_rgb(226, 232, 240)),
                                );
                                if !self.scanning && ui.small_button("×").clicked() {
                                    remove_idx = Some(idx);
                                }
                            });
                        });
                }
            });
            if let Some(idx) = remove_idx {
                self.config.roots.remove(idx);
            }
        }

        ui.add_space(6.0);

        // Row 3: Scan options
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!self.scanning, |ui| {
                ui.label(RichText::new("Min Size:").color(Color32::from_gray(170)));
                let current_min_label = MIN_SIZE_PRESETS
                    .iter()
                    .find(|(_, v)| *v == self.config.min_size)
                    .map(|(l, _)| *l)
                    .unwrap_or("Custom");
                egui::ComboBox::from_id_salt("min_size_combo")
                    .selected_text(current_min_label)
                    .width(135.0)
                    .show_ui(ui, |ui| {
                        for &(label, val) in MIN_SIZE_PRESETS {
                            ui.selectable_value(&mut self.config.min_size, val, label);
                        }
                    });

                ui.separator();
                ui.label(RichText::new("I/O:").color(Color32::from_gray(170)));
                egui::ComboBox::from_id_salt("io_profile_combo")
                    .selected_text(self.config.io_profile.label())
                    .width(135.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.config.io_profile,
                            IoProfile::AutoSsd,
                            IoProfile::AutoSsd.label(),
                        );
                        ui.selectable_value(
                            &mut self.config.io_profile,
                            IoProfile::HddGentle,
                            IoProfile::HddGentle.label(),
                        );
                    });

                ui.separator();
                ui.checkbox(&mut self.config.skip_system_dirs, "Skip System Folders")
                    .on_hover_text("Skips $RECYCLE.BIN, System Volume Information, WinSxS, etc.");
                ui.checkbox(&mut self.config.skip_dev_dirs, "Skip Dev Folders")
                    .on_hover_text("Skips .git, node_modules, target, .venv, __pycache__, etc.");
            });
        });
    }

    fn render_progress_panel(&self, ui: &mut egui::Ui) {
        let stage = self.progress.get_stage();
        let files_scanned = self.progress.files_scanned.load(Ordering::Relaxed);
        let cand_total = self.progress.candidates_total.load(Ordering::Relaxed);
        let cand_done = self.progress.candidates_done.load(Ordering::Relaxed);
        let bytes_hashed = self.progress.bytes_hashed.load(Ordering::Relaxed);
        let elapsed = self
            .scan_started_at
            .map(|t| t.elapsed())
            .unwrap_or_default();

        ui.vertical_centered(|ui| {
            ui.add_space(60.0);
            ui.label(
                RichText::new(stage.label())
                    .size(18.0)
                    .strong()
                    .color(Color32::from_rgb(125, 211, 252)),
            );
            ui.add_space(12.0);

            let bar_width = (ui.available_width() * 0.65).clamp(260.0, 560.0);
            if matches!(stage, ScanStage::PartialHashing | ScanStage::FullHashing) && cand_total > 0
            {
                let frac = (cand_done as f32 / cand_total as f32).clamp(0.0, 1.0);
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(bar_width)
                        .show_percentage()
                        .animate(true),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("{cand_done} / {cand_total} candidate files"))
                        .color(Color32::from_gray(180)),
                );
            } else {
                ui.add(egui::Spinner::new().size(28.0));
            }

            ui.add_space(18.0);

            let throughput = if elapsed.as_secs_f64() > 0.1 && bytes_hashed > 0 {
                let mb_s = (bytes_hashed as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
                format!("{mb_s:.1} MB/s")
            } else {
                "—".to_string()
            };

            egui::Frame::NONE
                .fill(Color32::from_rgb(24, 30, 42))
                .stroke(Stroke::new(1.0, Color32::from_rgb(45, 55, 72)))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(20, 14))
                .show(ui, |ui| {
                    egui::Grid::new("progress_stats_grid")
                        .spacing([28.0, 8.0])
                        .show(ui, |ui| {
                            ui.label(RichText::new("Files Scanned").color(Color32::from_gray(150)));
                            ui.label(RichText::new("Data Hashed").color(Color32::from_gray(150)));
                            ui.label(RichText::new("Throughput").color(Color32::from_gray(150)));
                            ui.label(RichText::new("Elapsed").color(Color32::from_gray(150)));
                            ui.end_row();

                            ui.label(RichText::new(format!("{files_scanned}")).strong().size(15.0));
                            ui.label(
                                RichText::new(format_bytes(bytes_hashed))
                                    .strong()
                                    .size(15.0),
                            );
                            ui.label(RichText::new(throughput).strong().size(15.0));
                            ui.label(
                                RichText::new(format_duration(elapsed))
                                    .strong()
                                    .size(15.0),
                            );
                            ui.end_row();
                        });
                });
        });
    }

    fn render_empty_state(&self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(70.0);
            if let Some(summary) = &self.summary {
                if self.last_cancelled {
                    ui.label(
                        RichText::new("Scan Cancelled")
                            .size(20.0)
                            .strong()
                            .color(Color32::from_rgb(251, 191, 36)),
                    );
                } else {
                    ui.label(
                        RichText::new("No Duplicates Found!")
                            .size(20.0)
                            .strong()
                            .color(Color32::from_rgb(74, 222, 128)),
                    );
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "Scanned {} files in {} · Hashed {} · Skipped {} hardlinks",
                        summary.total_files_seen,
                        format_duration(summary.elapsed),
                        format_bytes(summary.total_bytes_hashed),
                        summary.skipped_hardlinks,
                    ))
                    .color(Color32::from_gray(170)),
                );
            } else {
                ui.label(
                    RichText::new("Select one or more folders or drives to scan")
                        .size(18.0)
                        .strong()
                        .color(Color32::from_gray(220)),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Tip: You can click a drive above, use '+ Add Folder', or drag & drop folders onto this window.",
                    )
                    .color(Color32::from_gray(150)),
                );
                ui.add_space(20.0);
                ui.label(
                    RichText::new(
                        "4-Stage Pipeline: Parallel Walk → Size Filter → 8 KB Head+Tail Hash → SIMD BLAKE3",
                    )
                    .size(12.5)
                    .color(Color32::from_rgb(100, 160, 210)),
                );
            }
        });
    }

    fn render_results_panel(&mut self, ui: &mut egui::Ui) {
        // Summary strip
        let total_groups = self.groups.len();
        let redundant_files: usize = self
            .groups
            .iter()
            .map(|g| g.files.len().saturating_sub(1))
            .sum();
        let reclaimable_bytes: u64 = self.groups.iter().map(|g| g.wasted_bytes()).sum();
        let (sel_count, sel_bytes, all_sel_groups) = self.selected_stats();

        ui.horizontal_wrapped(|ui| {
            stat_chip(ui, "Groups", &total_groups.to_string(), Color32::from_rgb(56, 189, 248));
            stat_chip(
                ui,
                "Redundant Files",
                &redundant_files.to_string(),
                Color32::from_rgb(251, 191, 36),
            );
            stat_chip(
                ui,
                "Reclaimable",
                &format_bytes(reclaimable_bytes),
                Color32::from_rgb(74, 222, 128),
            );
            if let Some(summary) = &self.summary {
                stat_chip(
                    ui,
                    "Scan Time",
                    &format_duration(summary.elapsed),
                    Color32::from_rgb(192, 132, 252),
                );
            }
        });

        ui.add_space(8.0);

        // Action & filter toolbar
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Filter by filename or path...")
                    .desired_width(220.0),
            );

            ui.separator();

            let prev_sort = self.sort_by;
            egui::ComboBox::from_id_salt("sort_combo")
                .selected_text(format!("Sort: {}", self.sort_by.label()))
                .width(190.0)
                .show_ui(ui, |ui| {
                    for opt in [
                        SortBy::WastedSpaceDesc,
                        SortBy::FileSizeDesc,
                        SortBy::CountDesc,
                        SortBy::NameAsc,
                    ] {
                        ui.selectable_value(&mut self.sort_by, opt, opt.label());
                    }
                });
            if self.sort_by != prev_sort {
                sort_groups(&mut self.groups, self.sort_by);
            }

            ui.separator();

            ui.menu_button("Auto-Select ▾", |ui| {
                for rule in [
                    AutoSelectRule::KeepOldest,
                    AutoSelectRule::KeepNewest,
                    AutoSelectRule::KeepShortestPath,
                    AutoSelectRule::KeepLongestPath,
                ] {
                    if ui.button(rule.label()).clicked() {
                        apply_auto_select(&mut self.groups, rule);
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.button("Clear Selection").clicked() {
                    clear_selection(&mut self.groups);
                    ui.close_menu();
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if sel_count > 0 {
                    format!("Recycle Selected ({sel_count} · {})", format_bytes(sel_bytes))
                } else {
                    "Recycle Selected".to_string()
                };
                let btn = egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                    .fill(if sel_count > 0 {
                        Color32::from_rgb(220, 38, 38)
                    } else {
                        Color32::from_gray(55)
                    });
                if ui.add_enabled(sel_count > 0, btn).clicked() {
                    self.confirm_delete_open = true;
                }
            });
        });

        if all_sel_groups > 0 {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "⚠ Warning: {all_sel_groups} group(s) have ALL copies selected! Uncheck at least 1 copy to avoid losing the file."
                ))
                .size(12.5)
                .color(Color32::from_rgb(251, 191, 36)),
            );
        }

        ui.add_space(6.0);
        ui.separator();

        // Virtualized row list for smooth 60fps even with 100k+ duplicates
        let rows = self.build_display_rows();
        let row_height = 28.0;
        let mut toggle_collapse: Option<String> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_height, rows.len(), |ui, row_range| {
                for row_idx in row_range {
                    match rows[row_idx] {
                        DisplayRow::GroupHeader { group_idx } => {
                            let group = &self.groups[group_idx];
                            let is_collapsed = self.collapsed_groups.contains(&group.hash_hex);
                            let all_sel = group.all_selected();
                            let file_name = group
                                .files
                                .first()
                                .and_then(|f| f.path.file_name())
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| "Duplicate Group".to_string());

                            let bg = if all_sel {
                                Color32::from_rgb(78, 30, 30)
                            } else {
                                Color32::from_rgb(28, 37, 52)
                            };

                            egui::Frame::NONE
                                .fill(bg)
                                .corner_radius(4.0)
                                .inner_margin(egui::Margin::symmetric(8, 4))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        let arrow = if is_collapsed { "▶" } else { "▼" };
                                        if ui.small_button(arrow).clicked() {
                                            toggle_collapse = Some(group.hash_hex.clone());
                                        }
                                        ui.label(
                                            RichText::new(&file_name)
                                                .strong()
                                                .color(Color32::from_rgb(241, 245, 249)),
                                        );
                                        ui.label(
                                            RichText::new(format!(
                                                "{} copies · {} each · {} wasted",
                                                group.files.len(),
                                                format_bytes(group.size),
                                                format_bytes(group.wasted_bytes()),
                                            ))
                                            .size(12.0)
                                            .color(Color32::from_rgb(148, 163, 184)),
                                        );
                                        if all_sel {
                                            ui.label(
                                                RichText::new("ALL SELECTED")
                                                    .size(11.0)
                                                    .strong()
                                                    .color(Color32::from_rgb(252, 165, 165)),
                                            );
                                        } else {
                                            let sel = group.selected_count();
                                            if sel > 0 {
                                                ui.label(
                                                    RichText::new(format!("{sel} selected"))
                                                        .size(11.0)
                                                        .color(Color32::from_rgb(251, 191, 36)),
                                                );
                                            }
                                        }
                                    });
                                });
                        }
                        DisplayRow::FileItem {
                            group_idx,
                            file_idx,
                        } => {
                            let file = &mut self.groups[group_idx].files[file_idx];
                            ui.horizontal(|ui| {
                                ui.add_space(22.0);
                                ui.checkbox(&mut file.selected, "");

                                let path_str = file.path.to_string_lossy().to_string();
                                let path_color = if file.selected {
                                    Color32::from_rgb(252, 165, 165)
                                } else {
                                    Color32::from_rgb(215, 222, 232)
                                };

                                ui.label(
                                    RichText::new(format_timestamp(file.modified))
                                        .size(11.5)
                                        .monospace()
                                        .color(Color32::from_gray(140)),
                                );

                                if ui
                                    .small_button("Folder")
                                    .on_hover_text("Reveal in Windows Explorer")
                                    .clicked()
                                {
                                    reveal_in_explorer(&file.path);
                                }

                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&path_str).size(12.5).color(path_color),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(path_str);
                            });
                        }
                    }
                }
            });

        if let Some(hash) = toggle_collapse {
            if !self.collapsed_groups.remove(&hash) {
                self.collapsed_groups.insert(hash);
            }
        }
    }

    fn render_confirm_delete_modal(&mut self, ctx: &egui::Context) {
        let (sel_count, sel_bytes, all_sel_groups) = self.selected_stats();
        let mut open = self.confirm_delete_open;
        let mut do_delete = false;

        egui::Window::new("Confirm Move to Recycle Bin")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.label(format!(
                    "Move {sel_count} selected file(s) ({}) to the Windows Recycle Bin?",
                    format_bytes(sel_bytes)
                ));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("You can restore these files from the Recycle Bin if needed.")
                        .size(12.0)
                        .color(Color32::from_gray(160)),
                );

                if all_sel_groups > 0 {
                    ui.add_space(8.0);
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(80, 28, 28))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!(
                                    "⚠ Caution: {all_sel_groups} group(s) have EVERY copy selected! Proceeding will send all copies of those files to the Recycle Bin."
                                ))
                                .color(Color32::from_rgb(254, 202, 202)),
                            );
                        });
                }

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.confirm_delete_open = false;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let confirm_btn = egui::Button::new(
                            RichText::new("Move to Recycle Bin")
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(Color32::from_rgb(220, 38, 38));
                        if ui.add(confirm_btn).clicked() {
                            do_delete = true;
                        }
                    });
                });
            });

        if !open {
            self.confirm_delete_open = false;
        }

        if do_delete {
            self.confirm_delete_open = false;
            let report = move_selected_to_recycle_bin(&mut self.groups);
            if report.failed.is_empty() {
                self.status_message = Some((
                    format!(
                        "Moved {} file(s) to Recycle Bin · Freed {}",
                        report.deleted_count,
                        format_bytes(report.freed_bytes)
                    ),
                    false,
                ));
            } else {
                self.status_message = Some((
                    format!(
                        "Moved {} file(s) ({}), {} failed (locked or access denied)",
                        report.deleted_count,
                        format_bytes(report.freed_bytes),
                        report.failed.len()
                    ),
                    true,
                ));
            }
        }
    }

    fn render_bottom_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some((msg, is_err)) = &self.status_message {
                let color = if *is_err {
                    Color32::from_rgb(251, 191, 36)
                } else {
                    Color32::from_rgb(74, 222, 128)
                };
                ui.label(RichText::new(msg).size(12.0).color(color));
            } else if let Some(summary) = &self.summary {
                ui.label(
                    RichText::new(format!(
                        "Scanned {} files · Hashed {} · {} hardlinks skipped · {} access errors",
                        summary.total_files_seen,
                        format_bytes(summary.total_bytes_hashed),
                        summary.skipped_hardlinks,
                        summary.skipped_errors,
                    ))
                    .size(11.5)
                    .color(Color32::from_gray(145)),
                );
            } else {
                ui.label(
                    RichText::new("Ready · Native Rust + SIMD BLAKE3")
                        .size(11.5)
                        .color(Color32::from_gray(145)),
                );
            }
        });
    }
}

fn stat_chip(ui: &mut egui::Ui, label: &str, value: &str, accent: Color32) {
    egui::Frame::NONE
        .fill(Color32::from_rgb(24, 30, 42))
        .stroke(Stroke::new(1.0, Color32::from_rgb(45, 55, 72)))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).size(11.5).color(Color32::from_gray(150)));
                ui.label(RichText::new(value).size(13.0).strong().color(accent));
            });
        });
}

fn configure_visuals(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = Color32::from_rgb(15, 20, 29);
    visuals.panel_fill = Color32::from_rgb(15, 20, 29);
    visuals.extreme_bg_color = Color32::from_rgb(10, 14, 22);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(24, 31, 44);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(30, 41, 59);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 58, 82);
    visuals.widgets.active.bg_fill = Color32::from_rgb(56, 74, 104);
    ctx.set_visuals(visuals);
}
