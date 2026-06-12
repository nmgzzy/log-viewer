//! 应用外壳：顶栏菜单、MDI 标签栏（FR-7）、UDP/合并对话框、快捷键。

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};

use lv_core::archive::{ArchiveConfig, ArchiveSplit};
use lv_core::filter::SavedFilter;
use lv_core::ingest::{spawn_ingest, IngestOpts};
use lv_core::merge::{merge_snapshot, MergeInput};
use lv_core::parse::ParserCtx;
use lv_core::source::file::{expand_and_order, is_gz, spawn as spawn_file, FileSourceConfig};
use lv_core::source::udp::{spawn as spawn_udp, UdpSourceConfig};

use crate::fonts::install_cjk_fonts;
use crate::i18n::{texts, Lang, Texts};
use crate::session::{self, Session, SessionKind, SessionTab};
use crate::tab::{Tab, TabKind};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub lang: Lang,
    pub dark: bool,
    pub saved_filters: Vec<SavedFilter>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lang: Lang::default(),
            dark: true,
            saved_filters: Vec::new(),
        }
    }
}

struct UdpDialog {
    bind: String,
    port: String,
    archive_on: bool,
    dir: String,
    per_host: bool,
    error: Option<String>,
}

impl Default for UdpDialog {
    fn default() -> Self {
        let dir = default_archive_dir();
        Self {
            bind: "0.0.0.0".into(),
            port: "514".into(),
            archive_on: true,
            dir: dir.display().to_string(),
            per_host: false,
            error: None,
        }
    }
}

pub fn default_archive_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logviewer")
        .join("archive")
}

pub struct LogViewerApp {
    pub settings: Settings,
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_id: u64,
    udp_dialog: Option<UdpDialog>,
    merge_dialog: Option<Vec<bool>>,
    about_open: bool,
    filter_name_input: String,
    visuals_applied: bool,
    last_session_save: std::time::Instant,
    last_tab_count: usize,
}

impl LogViewerApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
        let mut app = Self {
            settings: Settings::default(),
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            udp_dialog: None,
            merge_dialog: None,
            about_open: false,
            filter_name_input: String::new(),
            visuals_applied: false,
            last_session_save: std::time::Instant::now(),
            last_tab_count: 0,
        };
        app.restore_session();
        // 命令行参数：logviewer [--udp 端口] <文件/目录>...
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--udp" {
                if let Some(port) = args.next().and_then(|s| s.parse::<u16>().ok()) {
                    let archive = Some(ArchiveConfig {
                        dir: default_archive_dir(),
                        prefix: format!("udp-{port}"),
                        ..Default::default()
                    });
                    let _ = app.start_udp("0.0.0.0".into(), port, archive);
                }
                continue;
            }
            let p = PathBuf::from(&arg);
            if p.exists() {
                let title = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or(arg);
                app.open_file_tab(vec![p], title);
            }
        }
        app
    }

    // ---------- 会话持久化 ----------

    fn restore_session(&mut self) {
        let Some(sess) = session::load() else { return };
        self.settings = sess.settings;
        for st in sess.tabs {
            let created = match &st.kind {
                Some(SessionKind::File { paths }) => {
                    let before = self.tabs.len();
                    self.open_file_tab(paths.clone(), st.title.clone());
                    self.tabs.len() > before
                }
                Some(SessionKind::Udp { bind, port, archive }) => self
                    .start_udp(bind.clone(), *port, archive.clone())
                    .is_ok(),
                None => false,
            };
            if created {
                if let Some(tab) = self.tabs.last_mut() {
                    tab.filter = st.filter;
                    tab.rules = st.rules;
                    tab.cols = st.cols;
                    tab.compact = st.compact;
                    tab.wrap = st.wrap;
                    tab.time_mode = st.time_mode;
                    tab.show_filter = st.show_filter;
                    tab.show_dashboard = st.show_dashboard;
                    tab.filter_dirty = true;
                    tab.hl_dirty = true;
                }
            }
        }
        self.active = sess.active.min(self.tabs.len().saturating_sub(1));
    }

    fn save_session(&self) {
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let kind = match &tab.kind {
                    TabKind::File { paths } => Some(SessionKind::File {
                        paths: paths.clone(),
                    }),
                    TabKind::Udp {
                        bind,
                        port,
                        archive,
                    } => Some(SessionKind::Udp {
                        bind: bind.clone(),
                        port: *port,
                        archive: archive.clone(),
                    }),
                    TabKind::Merged { .. } => None, // 合并 Tab 不恢复
                };
                kind.map(|kind| SessionTab {
                    kind: Some(kind),
                    title: tab.title.clone(),
                    filter: tab.filter.clone(),
                    rules: tab.rules.clone(),
                    cols: tab.cols,
                    compact: tab.compact,
                    wrap: tab.wrap,
                    time_mode: tab.time_mode,
                    show_filter: tab.show_filter,
                    show_dashboard: tab.show_dashboard,
                })
            })
            .collect();
        session::save(&Session {
            settings: Settings {
                lang: self.settings.lang,
                dark: self.settings.dark,
                saved_filters: self.settings.saved_filters.clone(),
            },
            tabs,
            active: self.active,
        });
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    // ---------- Tab 创建 ----------

    pub fn open_file_tab(&mut self, inputs: Vec<PathBuf>, title: String) {
        let files = expand_and_order(&inputs);
        if files.is_empty() {
            return;
        }
        let id = self.next_id();
        let mut tab = Tab::new(id, title.clone(), TabKind::File { paths: files.clone() }, false);
        let follow = files.last().map(|p| !is_gz(p)).unwrap_or(false);
        let handle = spawn_file(FileSourceConfig {
            paths: files,
            follow,
        });
        let source_id = tab
            .store
            .lock()
            .unwrap()
            .add_source(format!("file:{title}"));
        let ingest = spawn_ingest(
            handle.rx.clone(),
            tab.store.clone(),
            IngestOpts {
                source_id,
                live: false,
                ctx: ParserCtx::from_local_now(),
                archive: None,
                paused: tab.paused.clone(),
            },
        );
        tab.source = Some(handle);
        tab.ingest = Some(ingest);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn open_udp_tab(&mut self, d: &UdpDialog) -> Result<(), String> {
        let port: u16 = d
            .port
            .trim()
            .parse()
            .map_err(|_| "端口无效 / bad port".to_string())?;
        let archive = d.archive_on.then(|| ArchiveConfig {
            dir: PathBuf::from(d.dir.trim()),
            prefix: format!("udp-{port}"),
            split: if d.per_host {
                ArchiveSplit::PerHost
            } else {
                ArchiveSplit::Unified
            },
            ..Default::default()
        });
        self.start_udp(d.bind.trim().to_owned(), port, archive)
    }

    fn start_udp(
        &mut self,
        bind: String,
        port: u16,
        archive: Option<ArchiveConfig>,
    ) -> Result<(), String> {
        let cfg = UdpSourceConfig {
            bind: bind.clone(),
            port,
        };
        let (handle, local) = spawn_udp(cfg).map_err(|e| e.to_string())?;
        let title = format!("udp:{local}");
        let id = self.next_id();
        let mut tab = Tab::new(
            id,
            title.clone(),
            TabKind::Udp {
                bind,
                port,
                archive: archive.clone(),
            },
            true,
        );
        let source_id = tab.store.lock().unwrap().add_source(title);
        let ingest = spawn_ingest(
            handle.rx.clone(),
            tab.store.clone(),
            IngestOpts {
                source_id,
                live: true,
                ctx: ParserCtx::from_local_now(),
                archive,
                paused: tab.paused.clone(),
            },
        );
        tab.source = Some(handle);
        tab.ingest = Some(ingest);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    fn create_merged_tab(&mut self, member_idx: Vec<usize>, t: &Texts) {
        if member_idx.len() < 2 {
            return;
        }
        let id = self.next_id();
        let titles: Vec<String> = member_idx
            .iter()
            .map(|&i| self.tabs[i].title.clone())
            .collect();
        let title = format!("{} ({})", t.merged_tab_name, titles.join("+"));
        let mut tab = Tab::new(
            id,
            title,
            TabKind::Merged {
                members: titles.clone(),
            },
            true,
        );
        // 1) 快照合并现有内容
        {
            let guards: Vec<_> = member_idx
                .iter()
                .map(|&i| self.tabs[i].store.lock().unwrap())
                .collect();
            let inputs: Vec<MergeInput> = guards
                .iter()
                .zip(&titles)
                .map(|(g, name)| MergeInput {
                    store: g,
                    name: name.clone(),
                })
                .collect();
            let mut target = tab.store.lock().unwrap();
            merge_snapshot(&mut target, &inputs, &ParserCtx::from_local_now());
        }
        // 2) 订阅成员的实时流
        let (tx, rx) = crossbeam_channel::unbounded();
        for &i in &member_idx {
            if let Some(h) = &self.tabs[i].ingest {
                h.taps.lock().unwrap().push(tx.clone());
            }
        }
        drop(tx);
        let source_id = {
            let mut s = tab.store.lock().unwrap();
            s.add_source("merged-live")
        };
        let ingest = spawn_ingest(
            rx,
            tab.store.clone(),
            IngestOpts {
                source_id,
                live: true,
                ctx: ParserCtx::from_local_now(),
                archive: None,
                paused: tab.paused.clone(),
            },
        );
        tab.ingest = Some(ingest);
        tab.filter_dirty = true;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    fn close_tab(&mut self, idx: usize) {
        let mut tab = self.tabs.remove(idx);
        tab.stop_sources();
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
    }

    // ---------- UI ----------

    fn top_bar(&mut self, root: &mut egui::Ui) {
        let t = texts(self.settings.lang);
        egui::TopBottomPanel::top("top").show_inside(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.menu_button(t.menu_file, |ui| {
                    if ui.button(t.open_files).clicked() {
                        ui.close();
                        self.pick_files(t);
                    }
                    if ui.button(t.open_folder).clicked() {
                        ui.close();
                        self.pick_folder();
                    }
                    if ui.button(t.open_udp).clicked() {
                        ui.close();
                        self.udp_dialog = Some(UdpDialog::default());
                    }
                    ui.separator();
                    if ui.button(t.merge_tabs).clicked() {
                        ui.close();
                        self.merge_dialog = Some(vec![false; self.tabs.len()]);
                    }
                });
                ui.menu_button(t.menu_view, |ui| {
                    if ui.radio(self.settings.dark, t.theme_dark).clicked() {
                        self.settings.dark = true;
                        self.visuals_applied = false;
                    }
                    if ui.radio(!self.settings.dark, t.theme_light).clicked() {
                        self.settings.dark = false;
                        self.visuals_applied = false;
                    }
                });
                if ui.button(t.language).clicked() {
                    self.settings.lang = match self.settings.lang {
                        Lang::Zh => Lang::En,
                        Lang::En => Lang::Zh,
                    };
                }
                // 已存过滤器（应用到当前 Tab）
                if !self.tabs.is_empty() {
                    let mut apply: Option<lv_core::filter::FilterSpec> = None;
                    let mut delete: Option<usize> = None;
                    let mut save_current = false;
                    ui.menu_button(format!("📁 {}", t.filter_saved), |ui| {
                        for (i, f) in self.settings.saved_filters.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.button(&f.name).clicked() {
                                    apply = Some(f.spec.clone());
                                    ui.close();
                                }
                                if ui.small_button("✖").clicked() {
                                    delete = Some(i);
                                }
                            });
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.filter_name_input)
                                    .hint_text(t.filter_name_hint)
                                    .desired_width(120.0),
                            );
                            if ui.button(t.filter_save_as).clicked()
                                && !self.filter_name_input.trim().is_empty()
                            {
                                save_current = true;
                                ui.close();
                            }
                        });
                        ui.separator();
                        if ui.button(t.filter_import).clicked() {
                            ui.close();
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter("JSON", &["json"])
                                .pick_file()
                            {
                                if let Ok(s) = std::fs::read_to_string(&p) {
                                    if let Ok(mut fs) = lv_core::filter::filters_from_json(&s) {
                                        self.settings.saved_filters.append(&mut fs);
                                    }
                                }
                            }
                        }
                        if ui.button(t.filter_export_btn).clicked() {
                            ui.close();
                            if let Some(p) = rfd::FileDialog::new()
                                .set_file_name("filters.json")
                                .save_file()
                            {
                                let _ = std::fs::write(
                                    &p,
                                    lv_core::filter::filters_to_json(
                                        &self.settings.saved_filters,
                                    ),
                                );
                            }
                        }
                    });
                    if let Some(spec) = apply {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            tab.filter = spec;
                            tab.filter_dirty = true;
                            tab.show_filter = true;
                        }
                    }
                    if let Some(i) = delete {
                        self.settings.saved_filters.remove(i);
                    }
                    if save_current {
                        if let Some(tab) = self.tabs.get(self.active) {
                            self.settings.saved_filters.push(SavedFilter {
                                name: self.filter_name_input.trim().to_owned(),
                                spec: tab.filter.clone(),
                            });
                            self.filter_name_input.clear();
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("ℹ").clicked() {
                        self.about_open = !self.about_open;
                    }
                });
            });
        });
    }

    fn pick_files(&mut self, _t: &Texts) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("logs", &["log", "txt", "gz", "1", "2", "3", "jsonl"])
            .add_filter("*", &["*"])
            .pick_files()
        {
            // 每个文件一个 Tab（FR-7）
            for p in paths {
                let title = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string());
                self.open_file_tab(vec![p], title);
            }
        }
    }

    fn pick_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            let title = dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.display().to_string());
            // 目录 = 一个轮转集 = 一个 Tab，按时间序拼接（FR-S1）
            self.open_file_tab(vec![dir], format!("{title}/"));
        }
    }

    fn tab_bar(&mut self, root: &mut egui::Ui) {
        let t = texts(self.settings.lang);
        egui::TopBottomPanel::top("tabs").show_inside(root, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut close: Option<usize> = None;
                    for i in 0..self.tabs.len() {
                        let selected = i == self.active;
                        let title = self.tabs[i].title.clone();
                        let label = ui.selectable_label(selected, RichText::new(&title));
                        if label.clicked() {
                            self.active = i;
                        }
                        label.context_menu(|ui| {
                            if ui.button(t.close_tab).clicked() {
                                close = Some(i);
                                ui.close();
                            }
                        });
                        if ui
                            .small_button(RichText::new("✖").size(10.0))
                            .on_hover_text(t.close_tab)
                            .clicked()
                        {
                            close = Some(i);
                        }
                        ui.separator();
                    }
                    if let Some(i) = close {
                        self.close_tab(i);
                    }
                });
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let t = texts(self.settings.lang);
        // UDP
        if let Some(d) = &mut self.udp_dialog {
            let mut open = true;
            let mut start = false;
            let mut cancel = false;
            egui::Window::new(t.udp_title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    egui::Grid::new("udp-grid").num_columns(2).show(ui, |ui| {
                        ui.label(t.udp_bind);
                        ui.text_edit_singleline(&mut d.bind);
                        ui.end_row();
                        ui.label(t.udp_port);
                        ui.text_edit_singleline(&mut d.port);
                        ui.end_row();
                        ui.label(t.udp_archive);
                        ui.checkbox(&mut d.archive_on, "");
                        ui.end_row();
                        if d.archive_on {
                            ui.label(t.udp_archive_dir);
                            ui.text_edit_singleline(&mut d.dir);
                            ui.end_row();
                            ui.label("");
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut d.per_host, false, t.udp_split_unified);
                                ui.radio_value(&mut d.per_host, true, t.udp_split_per_host);
                            });
                            ui.end_row();
                        }
                    });
                    if let Some(e) = &d.error {
                        ui.colored_label(Color32::RED, format!("{}: {e}", t.udp_err_bind));
                    }
                    ui.horizontal(|ui| {
                        if ui.button(t.udp_start).clicked() {
                            start = true;
                        }
                        if ui.button(t.udp_cancel).clicked() {
                            cancel = true;
                        }
                    });
                });
            if start {
                let d2 = UdpDialog {
                    bind: d.bind.clone(),
                    port: d.port.clone(),
                    archive_on: d.archive_on,
                    dir: d.dir.clone(),
                    per_host: d.per_host,
                    error: None,
                };
                match self.open_udp_tab(&d2) {
                    Ok(()) => self.udp_dialog = None,
                    Err(e) => {
                        if let Some(d) = &mut self.udp_dialog {
                            d.error = Some(e);
                        }
                    }
                }
            } else if cancel || !open {
                self.udp_dialog = None;
            }
        }
        // 合并
        if let Some(sel) = &mut self.merge_dialog {
            let mut open = true;
            let mut create = false;
            sel.resize(self.tabs.len(), false);
            egui::Window::new(t.merge_title)
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(t.merge_pick_hint);
                    for (i, tab) in self.tabs.iter().enumerate() {
                        if matches!(tab.kind, TabKind::Merged { .. }) {
                            continue;
                        }
                        ui.checkbox(&mut sel[i], &tab.title);
                    }
                    let n = sel.iter().filter(|v| **v).count();
                    ui.add_enabled_ui(n >= 2, |ui| {
                        if ui.button(t.merge_create).clicked() {
                            create = true;
                        }
                    });
                });
            if create {
                let members: Vec<usize> = sel
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| v.then_some(i))
                    .collect();
                self.merge_dialog = None;
                self.create_merged_tab(members, t);
            } else if !open {
                self.merge_dialog = None;
            }
        }
        // 关于
        if self.about_open {
            let mut open = self.about_open;
            egui::Window::new(t.about)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!("{} v{}", t.app_title, env!("CARGO_PKG_VERSION")));
                    ui.label("uf_log syslog viewer · RFC5424 / RFC3164 / JSON");
                });
            self.about_open = open;
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let active = self.active;
        if let Some(tab) = self.tabs.get_mut(active) {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
                tab.focus_search = true;
            }
            let (f3, shift) = ctx.input(|i| (i.key_pressed(egui::Key::F3), i.modifiers.shift));
            if f3 {
                tab.jump_search_pub(!shift);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            let t = texts(self.settings.lang);
            self.pick_files(t);
        }
    }
}

impl eframe::App for LogViewerApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        if !self.visuals_applied {
            ctx.set_visuals(if self.settings.dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            self.visuals_applied = true;
        }
        self.shortcuts(&ctx);
        self.top_bar(root);
        if !self.tabs.is_empty() {
            self.tab_bar(root);
        }
        self.dialogs(&ctx);

        let t = texts(self.settings.lang);
        let dark = self.settings.dark;
        let active = self.active.min(self.tabs.len().saturating_sub(1));
        egui::CentralPanel::default().show_inside(root, |ui| {
            if self.tabs.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new(t.no_tabs_hint).size(16.0).weak());
                });
                return;
            }
            let tab = &mut self.tabs[active];
            tab.tick();
            tab.ui(ui, t, dark);
        });

        // 实时源/加载中 → 持续重绘；空闲时事件驱动（§7.1 空闲占用低）
        let busy = self
            .tabs
            .iter()
            .any(|tb| (tb.live && tb.ingest.is_some()) || tb.is_loading());
        if busy {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        // 会话持久化：关闭时 + 标签页变化时 + 周期兜底
        let closing = ctx.input(|i| i.viewport().close_requested());
        let tabs_changed = self.last_tab_count != self.tabs.len();
        if closing || tabs_changed || self.last_session_save.elapsed().as_secs() >= 30 {
            self.save_session();
            self.last_session_save = std::time::Instant::now();
            self.last_tab_count = self.tabs.len();
        }
    }
}
