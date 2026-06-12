//! Tab：一个文件/源/合并视图（FR-7），独立持有过滤、搜索、高亮、
//! 列布局与滚动位置。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui::{self, Align, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use lv_core::archive::ArchiveConfig;
use lv_core::export::{export, ExportFormat};
use lv_core::filter::{Combine, CompiledFilter, FilterSpec, TextCond};
use lv_core::highlight::{HighlightRule, HighlightSet, MatchField};
use lv_core::ingest::IngestHandle;
use lv_core::model::level_name;
use lv_core::parse::ts::parse_rfc3339;
use lv_core::search::{next_hit, prev_hit, run_search, SearchSpec};
use lv_core::source::SourceHandle;
use lv_core::store::LogStore;
use lv_core::view::TabView;

use crate::fmt::{format_pid, format_time, local_tz_offset_min, TimeMode};
use crate::i18n::Texts;

#[derive(Clone, Debug)]
pub enum TabKind {
    File {
        paths: Vec<PathBuf>,
    },
    Udp {
        bind: String,
        port: u16,
        archive: Option<ArchiveConfig>,
    },
    Merged {
        members: Vec<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnsVisible {
    pub time: bool,
    pub host: bool,
    pub app: bool,
    pub pid: bool,
    pub level: bool,
    pub tag: bool,
    pub source: bool,
}

impl Default for ColumnsVisible {
    fn default() -> Self {
        Self {
            time: true,
            host: true,
            app: true,
            pid: false,
            level: true,
            tag: true,
            source: false,
        }
    }
}

/// ui 期间收集、帧末执行的动作（避免与 store 锁冲突）。
enum Action {
    Clear,
    Export(ExportFormat, PathBuf),
}

pub struct Tab {
    pub id: u64,
    pub title: String,
    pub kind: TabKind,
    pub store: Arc<Mutex<LogStore>>,
    pub source: Option<SourceHandle>,
    pub ingest: Option<IngestHandle>,
    pub paused: Arc<AtomicBool>,
    pub live: bool,

    // 过滤
    pub filter: FilterSpec,
    compiled: Option<CompiledFilter>,
    pub filter_dirty: bool,
    pub view: TabView,
    pub view_rev: u64,
    pub show_filter: bool,

    // 搜索
    pub search: SearchSpec,
    pub search_dirty: bool,
    pub search_hits: Vec<u32>,
    pub search_err: Option<String>,
    pub search_cursor: Option<u32>,
    last_search_rev: u64,
    last_search_at: Instant,
    pub focus_search: bool,

    // 高亮
    pub rules: Vec<HighlightRule>,
    hl: Option<HighlightSet>,
    pub hl_dirty: bool,
    pub show_hl_editor: bool,

    // 显示
    pub cols: ColumnsVisible,
    pub compact: bool,
    pub wrap: bool,
    pub time_mode: TimeMode,
    pub follow_tail: bool,
    pub selected: Option<usize>,
    scroll_to: Option<usize>,
    pub goto_text: String,
    pub show_dashboard: bool,
    pub dash_threshold: f64,

    pub toast: Option<(String, Instant)>,
    actions: Vec<Action>,
    rate_samples: VecDeque<(Instant, u64)>,
    pub rate: f64,
}

impl Tab {
    pub fn new(id: u64, title: String, kind: TabKind, live: bool) -> Self {
        Self {
            id,
            title,
            kind,
            store: Arc::new(Mutex::new(LogStore::new())),
            source: None,
            ingest: None,
            paused: Arc::new(AtomicBool::new(false)),
            live,
            filter: FilterSpec::default(),
            compiled: None,
            filter_dirty: true,
            view: TabView::new(live),
            view_rev: 0,
            show_filter: false,
            search: SearchSpec::default(),
            search_dirty: false,
            search_hits: Vec::new(),
            search_err: None,
            search_cursor: None,
            last_search_rev: 0,
            last_search_at: Instant::now(),
            focus_search: false,
            rules: Vec::new(),
            hl: None,
            hl_dirty: true,
            show_hl_editor: false,
            cols: ColumnsVisible::default(),
            compact: true,
            wrap: false,
            time_mode: TimeMode::default(),
            follow_tail: live,
            selected: None,
            scroll_to: None,
            goto_text: String::new(),
            show_dashboard: false,
            dash_threshold: 10.0,
            toast: None,
            actions: Vec::new(),
            rate_samples: VecDeque::new(),
            rate: 0.0,
        }
    }

    /// 每帧数据维护：过滤视图、搜索、速率。在 ui 之前调用。
    pub fn tick(&mut self) {
        // 速率采样（来自源计数器）
        if let Some(src) = &self.source {
            let n = src.received.load(Ordering::Relaxed);
            let now = Instant::now();
            self.rate_samples.push_back((now, n));
            while self
                .rate_samples
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t).as_secs_f64() > 3.0)
            {
                self.rate_samples.pop_front();
            }
            if let (Some((t0, n0)), Some((t1, n1))) =
                (self.rate_samples.front(), self.rate_samples.back())
            {
                let dt = t1.duration_since(*t0).as_secs_f64();
                self.rate = if dt > 0.1 {
                    (n1 - n0) as f64 / dt
                } else {
                    0.0
                };
            }
        }

        let store = self.store.lock().unwrap();
        if self.filter_dirty || self.compiled.is_none() {
            self.compiled = Some(CompiledFilter::compile(self.filter.clone(), &store));
            self.view.rebuild(&store, self.compiled.as_ref().unwrap());
            self.filter_dirty = false;
            self.view_rev += 1;
            self.search_dirty = true;
        } else {
            let cf = self.compiled.as_mut().unwrap();
            cf.refresh_syms(&store);
            if self.view.update_incremental(&store, cf) {
                self.view_rev += 1;
            }
        }
        // 搜索：参数变化立即跑；视图增长则节流重跑
        let need = self.search_dirty
            || (!self.search.query.is_empty()
                && self.last_search_rev != self.view_rev
                && self.last_search_at.elapsed().as_millis() > 300);
        if need {
            let r = run_search(&store, &self.view.seqs, &self.search);
            self.search_hits = r.hits;
            self.search_err = r.error;
            self.search_dirty = false;
            self.last_search_rev = self.view_rev;
            self.last_search_at = Instant::now();
            if self
                .search_cursor
                .is_some_and(|c| self.search_hits.binary_search(&c).is_err())
            {
                self.search_cursor = None;
            }
        }
        if self.hl_dirty {
            self.hl = Some(HighlightSet::compile(self.rules.clone()));
            self.hl_dirty = false;
        }
        drop(store);

        // 帧末动作
        for a in std::mem::take(&mut self.actions) {
            match a {
                Action::Clear => {
                    self.store.lock().unwrap().clear();
                    self.filter_dirty = true;
                    self.search_hits.clear();
                    self.selected = None;
                }
                Action::Export(fmt, path) => {
                    let store = self.store.lock().unwrap();
                    let res = std::fs::File::create(&path).map_err(anyhow::Error::from).and_then(
                        |f| {
                            let mut w = std::io::BufWriter::new(f);
                            export(&store, &self.view.seqs, fmt, &mut w)
                        },
                    );
                    drop(store);
                    self.toast = Some((
                        match res {
                            Ok(n) => format!("✔ {} ({n})", path.display()),
                            Err(e) => format!("✘ {e}"),
                        },
                        Instant::now(),
                    ));
                }
            }
        }
    }

    pub fn stop_sources(&mut self) {
        if let Some(mut s) = self.source.take() {
            s.stop();
        }
        self.ingest.take();
    }

    pub fn is_loading(&self) -> bool {
        match &self.ingest {
            Some(h) => !h.stats.load_done.load(Ordering::Relaxed) && !self.live,
            None => false,
        }
    }

    // ---------- UI ----------

    pub fn ui(&mut self, ui: &mut egui::Ui, t: &Texts, dark: bool) {
        self.toolbar(ui, t);
        if self.show_filter {
            self.filter_panel(ui, t);
        }
        self.search_bar(ui, t);
        ui.separator();
        self.status_bar(ui, t);
        self.table(ui, t, dark);
        if self.show_hl_editor {
            self.highlight_editor(ui.ctx().clone(), t);
        }
        // toast 自动消失
        if let Some((msg, at)) = &self.toast {
            if at.elapsed().as_secs() < 5 {
                let msg = msg.clone();
                egui::Area::new(egui::Id::new(("toast", self.id)))
                    .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -40.0])
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(msg));
                    });
            } else {
                self.toast = None;
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, t: &Texts) {
        ui.horizontal_wrapped(|ui| {
            ui.toggle_value(&mut self.show_filter, format!("🔍 {}", t.filter));
            ui.toggle_value(&mut self.show_dashboard, format!("📊 {}", t.dashboard));
            ui.toggle_value(&mut self.follow_tail, format!("⏬ {}", t.follow_tail));
            if self.live {
                let paused = self.paused.load(Ordering::Relaxed);
                let label = if paused {
                    format!("▶ {}", t.resume)
                } else {
                    format!("⏸ {}", t.pause)
                };
                if ui.button(label).clicked() {
                    self.paused.store(!paused, Ordering::Relaxed);
                }
            }
            if ui.button(format!("🗑 {}", t.clear)).clicked() {
                self.actions.push(Action::Clear);
            }
            ui.menu_button(format!("💾 {}", t.export), |ui| {
                for (label, fmt) in [
                    (t.export_text, ExportFormat::Text),
                    (t.export_json, ExportFormat::Json),
                    (t.export_csv, ExportFormat::Csv),
                ] {
                    if ui.button(label).clicked() {
                        ui.close();
                        if let Some(path) = rfd::FileDialog::new()
                            .set_file_name(format!("export.{}", fmt.extension()))
                            .save_file()
                        {
                            self.actions.push(Action::Export(fmt, path));
                        }
                    }
                }
            });
            ui.menu_button(format!("☰ {}", t.columns), |ui| {
                ui.checkbox(&mut self.cols.time, t.col_time);
                ui.checkbox(&mut self.cols.host, t.col_host);
                ui.checkbox(&mut self.cols.app, t.col_app);
                ui.checkbox(&mut self.cols.pid, t.col_pid);
                ui.checkbox(&mut self.cols.level, t.col_level);
                ui.checkbox(&mut self.cols.tag, t.col_tag);
                ui.checkbox(&mut self.cols.source, t.col_source);
            });
            ui.menu_button(format!("🕒 {}", t.time_mode), |ui| {
                ui.radio_value(&mut self.time_mode, TimeMode::AbsOriginal, t.time_abs_orig);
                ui.radio_value(&mut self.time_mode, TimeMode::AbsLocal, t.time_abs_local);
                ui.radio_value(&mut self.time_mode, TimeMode::RelFirst, t.time_rel_first);
                ui.radio_value(&mut self.time_mode, TimeMode::RelPrev, t.time_rel_prev);
            });
            if ui
                .selectable_label(self.compact, t.density_compact)
                .clicked()
            {
                self.compact = !self.compact;
            }
            ui.toggle_value(&mut self.wrap, t.wrap_lines);
            ui.toggle_value(&mut self.show_hl_editor, format!("🎨 {}", t.highlight));
            // 跳转
            ui.separator();
            ui.label(t.goto);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.goto_text)
                    .hint_text(t.goto_hint)
                    .desired_width(140.0),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.do_goto();
            }
        });
    }

    fn do_goto(&mut self) {
        let q = self.goto_text.trim().to_owned();
        if q.is_empty() {
            return;
        }
        // 行号
        if let Ok(n) = q.parse::<usize>() {
            if n >= 1 && n <= self.view.len() {
                self.scroll_to = Some(n - 1);
                self.selected = Some(n - 1);
                self.follow_tail = false;
            }
            return;
        }
        // 时间：完整 RFC3339 或 HH:MM[:SS]（取首行日期）
        let store = self.store.lock().unwrap();
        let target_us = if let Some((us, _)) = parse_rfc3339(&q) {
            Some(us)
        } else {
            self.parse_time_of_day(&q, &store)
        };
        if let Some(us) = target_us {
            // 视图按显示顺序近似递增：二分
            let pos = self.view.seqs.partition_point(|&sq| {
                store.meta_by_seq(sq).map(|m| m.ts < us).unwrap_or(true)
            });
            let pos = pos.min(self.view.len().saturating_sub(1));
            self.scroll_to = Some(pos);
            self.selected = Some(pos);
            self.follow_tail = false;
        }
    }

    fn parse_time_of_day(&self, q: &str, store: &LogStore) -> Option<i64> {
        let first = store.meta_by_seq(*self.view.seqs.first()?)?;
        let parts: Vec<&str> = q.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return None;
        }
        let h: u32 = parts[0].parse().ok()?;
        let mi: u32 = parts[1].parse().ok()?;
        let se: u32 = if parts.len() == 3 {
            parts[2].parse().ok()?
        } else {
            0
        };
        use chrono::{Datelike, TimeZone};
        let off = chrono::FixedOffset::east_opt(first.tz_offset_min as i32 * 60)?;
        let dt = match off.timestamp_micros(first.ts) {
            chrono::LocalResult::Single(d) => d,
            _ => return None,
        };
        let nd = chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), dt.day())?;
        let ndt = nd.and_hms_opt(h, mi, se)?;
        Some(ndt.and_utc().timestamp_micros() - first.tz_offset_min as i64 * 60_000_000)
    }

    fn search_bar(&mut self, ui: &mut egui::Ui, t: &Texts) {
        ui.horizontal(|ui| {
            ui.label(format!("🔎 {}", t.search));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.search.query)
                    .hint_text(t.search_hint)
                    .desired_width(240.0),
            );
            if self.focus_search {
                resp.request_focus();
                self.focus_search = false;
            }
            if resp.changed() {
                self.search_dirty = true;
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.jump_search(true);
                resp.request_focus();
            }
            if ui.checkbox(&mut self.search.is_regex, t.filter_regex).changed() {
                self.search_dirty = true;
            }
            if ui
                .checkbox(&mut self.search.case_sensitive, t.filter_case)
                .changed()
            {
                self.search_dirty = true;
            }
            if ui.button(format!("⬆ {}", t.search_prev)).clicked() {
                self.jump_search(false);
            }
            if ui.button(format!("⬇ {}", t.search_next)).clicked() {
                self.jump_search(true);
            }
            if !self.search.query.is_empty() {
                let cur = self
                    .search_cursor
                    .and_then(|c| self.search_hits.binary_search(&c).ok())
                    .map(|i| i + 1)
                    .unwrap_or(0);
                ui.label(format!("{}: {}/{}", t.search_count, cur, self.search_hits.len()));
            }
            if let Some(e) = &self.search_err {
                ui.colored_label(Color32::RED, e);
            }
        });
    }

    /// 快捷键入口（F3 / Shift+F3）。
    pub fn jump_search_pub(&mut self, forward: bool) {
        self.jump_search(forward);
    }

    fn jump_search(&mut self, forward: bool) {
        if self.search_hits.is_empty() {
            return;
        }
        let from = self
            .search_cursor
            .map(|c| if forward { c.saturating_add(1) } else { c.saturating_sub(1) })
            .or(self.selected.map(|s| s as u32))
            .unwrap_or(0);
        let target = if forward {
            next_hit(&self.search_hits, from)
        } else {
            prev_hit(&self.search_hits, from.saturating_add(1))
        };
        if let Some(row) = target {
            self.search_cursor = Some(row);
            self.scroll_to = Some(row as usize);
            self.selected = Some(row as usize);
            self.follow_tail = false;
        }
    }

    // ---------- 过滤面板 ----------

    fn filter_panel(&mut self, ui: &mut egui::Ui, t: &Texts) {
        let mut dirty = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            // 级别
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(t.filter_level).strong());
                for i in 0..8u8 {
                    let mut v = self.filter.levels[i as usize];
                    if ui.toggle_value(&mut v, level_name(i)).changed() {
                        self.filter.levels[i as usize] = v;
                        dirty = true;
                    }
                }
                if ui.small_button("≥err").clicked() {
                    self.filter.set_min_severity(3);
                    dirty = true;
                }
                if ui.small_button("≥warning").clicked() {
                    self.filter.set_min_severity(4);
                    dirty = true;
                }
                if ui.small_button("all").clicked() {
                    self.filter.levels = [true; 8];
                    dirty = true;
                }
                ui.separator();
                if ui
                    .checkbox(&mut self.filter.show_unparsed, t.filter_show_unparsed)
                    .changed()
                {
                    dirty = true;
                }
            });
            // facet 维度：tag / host / app
            let store = self.store.lock().unwrap();
            let facet = |counts: &std::collections::HashMap<u32, u64>, store: &LogStore| {
                let mut v: Vec<(String, u64)> = counts
                    .iter()
                    .filter(|(_, c)| **c > 0)
                    .map(|(id, c)| (store.syms.get(*id).to_owned(), *c))
                    .filter(|(name, _)| !name.is_empty())
                    .collect();
                v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                v.truncate(30);
                v
            };
            let tags = facet(&store.tag_counts, &store);
            let hosts = facet(&store.host_counts, &store);
            let apps = facet(&store.app_counts, &store);
            drop(store);
            dirty |= facet_row(ui, t, t.filter_tags, &tags, &mut self.filter.include_tags, &mut self.filter.exclude_tags);
            dirty |= facet_row(ui, t, t.filter_hosts, &hosts, &mut self.filter.include_hosts, &mut self.filter.exclude_hosts);
            dirty |= facet_row(ui, t, t.filter_apps, &apps, &mut self.filter.include_apps, &mut self.filter.exclude_apps);

            // pid + 时间范围
            ui.horizontal(|ui| {
                ui.label(RichText::new(t.filter_pid).strong());
                let mut pid_s = self.filter.pid.map(|p| p.to_string()).unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut pid_s).desired_width(70.0))
                    .changed()
                {
                    self.filter.pid = pid_s.trim().parse().ok();
                    dirty = true;
                }
                ui.separator();
                ui.label(RichText::new(t.filter_time_from).strong());
                dirty |= time_edit(ui, &mut self.filter.time_from_us, self.id, 0);
                ui.label(RichText::new(t.filter_time_to).strong());
                dirty |= time_edit(ui, &mut self.filter.time_to_us, self.id, 1);
            });

            // 文本条件
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(t.filter_text).strong());
                let mut combine_or = self.filter.combine == Combine::Or;
                if ui.selectable_label(!combine_or, t.filter_and).clicked() {
                    combine_or = false;
                }
                if ui.selectable_label(combine_or, t.filter_or).clicked() {
                    combine_or = true;
                }
                let new_combine = if combine_or { Combine::Or } else { Combine::And };
                if new_combine != self.filter.combine {
                    self.filter.combine = new_combine;
                    dirty = true;
                }
                if ui.button(t.filter_add_cond).clicked() {
                    self.filter.texts.push(TextCond::contains(""));
                    dirty = true;
                }
                if ui.button(t.filter_clear).clicked() {
                    self.filter = FilterSpec::default();
                    dirty = true;
                }
            });
            let mut remove: Option<usize> = None;
            for (i, c) in self.filter.texts.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::TextEdit::singleline(&mut c.query).desired_width(220.0))
                        .changed()
                    {
                        dirty = true;
                    }
                    dirty |= ui.checkbox(&mut c.is_regex, t.filter_regex).changed();
                    dirty |= ui.checkbox(&mut c.case_sensitive, t.filter_case).changed();
                    dirty |= ui.checkbox(&mut c.exclude, t.filter_exclude).changed();
                    if ui.button("✖").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.filter.texts.remove(i);
                dirty = true;
            }
            if let Some(cf) = &self.compiled {
                for e in &cf.errors {
                    ui.colored_label(Color32::RED, e);
                }
            }
        });
        if dirty {
            self.filter_dirty = true;
        }
    }

    // ---------- 表格 ----------

    fn table(&mut self, ui: &mut egui::Ui, t: &Texts, dark: bool) {
        let store = self.store.lock().unwrap();
        let total = self.view.len();
        let row_h = if self.compact { 18.0 } else { 24.0 };
        let local_off = local_tz_offset_min();
        let first_ts = self
            .view
            .seqs
            .first()
            .and_then(|q| store.meta_by_seq(*q))
            .map(|m| m.ts)
            .unwrap_or(0);

        // 滚轮上滚 → 取消跟随
        if ui.rect_contains_pointer(ui.available_rect_before_wrap())
            && ui.input(|i| i.smooth_scroll_delta.y > 0.0)
        {
            self.follow_tail = false;
        }

        let mut builder = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .min_scrolled_height(120.0)
            .max_scroll_height(f32::INFINITY)
            .column(Column::auto()); // #
        if self.cols.time {
            builder = builder.column(Column::auto());
        }
        if self.cols.host {
            builder = builder.column(Column::initial(80.0).clip(true));
        }
        if self.cols.app {
            builder = builder.column(Column::initial(80.0).clip(true));
        }
        if self.cols.pid {
            builder = builder.column(Column::initial(50.0).clip(true));
        }
        if self.cols.level {
            builder = builder.column(Column::auto());
        }
        if self.cols.tag {
            builder = builder.column(Column::initial(80.0).clip(true));
        }
        if self.cols.source {
            builder = builder.column(Column::initial(120.0).clip(true));
        }
        builder = builder.column(Column::remainder().clip(true)); // msg

        if let Some(row) = self.scroll_to.take() {
            builder = builder.scroll_to_row(row, Some(Align::Center));
        } else if self.follow_tail && total > 0 {
            builder = builder.scroll_to_row(total - 1, Some(Align::BOTTOM));
        }

        let mut clicked_row: Option<usize> = None;
        let mut quick: Option<(QuickTarget, String, bool)> = None; // (字段, 值, 排除?)
        let mut copy_text: Option<String> = None;

        let header_h = 20.0;
        let selected = self.selected;
        let hits = &self.search_hits;
        let cursor = self.search_cursor;
        let hl = self.hl.as_ref();
        let view = &self.view.seqs;
        let cols = self.cols;
        let time_mode = self.time_mode;
        let wrap = self.wrap;

        let table = builder.header(header_h, |mut header| {
            header.col(|ui| {
                ui.label(RichText::new(t.col_row).strong());
            });
            if cols.time {
                header.col(|ui| {
                    ui.label(RichText::new(t.col_time).strong());
                });
            }
            if cols.host {
                header.col(|ui| {
                    ui.label(RichText::new(t.col_host).strong());
                });
            }
            if cols.app {
                header.col(|ui| {
                    ui.label(RichText::new(t.col_app).strong());
                });
            }
            if cols.pid {
                header.col(|ui| {
                    ui.label(RichText::new(t.col_pid).strong());
                });
            }
            if cols.level {
                header.col(|ui| {
                    ui.label(RichText::new(t.col_level).strong());
                });
            }
            if cols.tag {
                header.col(|ui| {
                    ui.label(RichText::new(t.col_tag).strong());
                });
            }
            if cols.source {
                header.col(|ui| {
                    ui.label(RichText::new(t.col_source).strong());
                });
            }
            header.col(|ui| {
                ui.label(RichText::new(t.col_msg).strong());
            });
        });

        table.body(|body| {
            body.rows(row_h, total, |mut row| {
                let vi = row.index();
                let seq = view[vi];
                let Some(m) = store.meta_by_seq(seq).copied() else {
                    row.col(|_| {});
                    return;
                };
                row.set_selected(selected == Some(vi));

                // 行颜色：高亮规则 > level 默认色
                let rule = hl.and_then(|h| h.match_record(&store, &m));
                let level_fg = level_color(m.level, m.is_parsed(), dark);
                let fg = rule
                    .and_then(|r| r.fg.map(|c| Color32::from_rgb(c[0], c[1], c[2])))
                    .unwrap_or(level_fg);
                let bg = rule.and_then(|r| r.bg.map(|c| Color32::from_rgb(c[0], c[1], c[2])));
                let bold = rule.map(|r| r.bold).unwrap_or(false);
                let is_hit = !hits.is_empty() && hits.binary_search(&(vi as u32)).is_ok();
                let is_cur = cursor == Some(vi as u32);
                let bg = if is_cur {
                    Some(if dark {
                        Color32::from_rgb(90, 80, 0)
                    } else {
                        Color32::from_rgb(255, 240, 120)
                    })
                } else if is_hit {
                    Some(if dark {
                        Color32::from_rgb(60, 60, 20)
                    } else {
                        Color32::from_rgb(255, 250, 190)
                    })
                } else {
                    bg
                };

                let cell = |ui: &mut egui::Ui, text: &str, mono: bool, wrap_this: bool| {
                    if let Some(bg) = bg {
                        ui.painter()
                            .rect_filled(ui.max_rect().expand(1.0), 0.0, bg);
                    }
                    let mut rt = RichText::new(text).color(fg);
                    if mono {
                        rt = rt.monospace();
                    }
                    if bold {
                        rt = rt.strong();
                    }
                    let label = egui::Label::new(rt);
                    let label = if wrap_this {
                        label.wrap()
                    } else {
                        label.truncate()
                    };
                    ui.add(label)
                };

                row.col(|ui| {
                    cell(ui, &(vi + 1).to_string(), true, false);
                });
                if cols.time {
                    let prev_ts = vi
                        .checked_sub(1)
                        .and_then(|p| view.get(p))
                        .and_then(|q| store.meta_by_seq(*q))
                        .map(|p| p.ts);
                    row.col(|ui| {
                        cell(
                            ui,
                            &format_time(&m, time_mode, first_ts, prev_ts, local_off),
                            true,
                            false,
                        );
                    });
                }
                if cols.host {
                    row.col(|ui| {
                        let v = store.syms.get(m.host);
                        let r = cell(ui, v, false, false);
                        quick_menu(&r, t, QuickTarget::Host, v, &mut quick, &mut copy_text);
                    });
                }
                if cols.app {
                    row.col(|ui| {
                        let v = store.syms.get(m.app);
                        let r = cell(ui, v, false, false);
                        quick_menu(&r, t, QuickTarget::App, v, &mut quick, &mut copy_text);
                    });
                }
                if cols.pid {
                    row.col(|ui| {
                        cell(ui, &format_pid(&m), true, false);
                    });
                }
                if cols.level {
                    row.col(|ui| {
                        let v = if m.is_parsed() { level_name(m.level) } else { "?" };
                        cell(ui, v, false, false);
                    });
                }
                if cols.tag {
                    row.col(|ui| {
                        let v = store.syms.get(m.tag);
                        let r = cell(ui, v, false, false);
                        quick_menu(&r, t, QuickTarget::Tag, v, &mut quick, &mut copy_text);
                    });
                }
                if cols.source {
                    row.col(|ui| {
                        cell(ui, store.source_name(m.source), false, false);
                    });
                }
                row.col(|ui| {
                    let msg = if m.is_parsed() {
                        store.msg_text(&m)
                    } else {
                        store.raw_text(&m)
                    };
                    let r = cell(ui, msg, true, wrap);
                    r.context_menu(|ui| {
                        if ui.button(t.copy_cell).clicked() {
                            copy_text = Some(msg.to_owned());
                            ui.close();
                        }
                        if ui.button(t.copy_row).clicked() {
                            copy_text = Some(store.raw_text(&m).to_owned());
                            ui.close();
                        }
                        if ui.button(t.copy_row_fields).clicked() {
                            copy_text = Some(format!(
                                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                                format_time(&m, TimeMode::AbsOriginal, first_ts, None, local_off),
                                store.syms.get(m.host),
                                store.syms.get(m.app),
                                format_pid(&m),
                                level_name(m.level),
                                store.syms.get(m.tag),
                                store.msg_text(&m),
                            ));
                            ui.close();
                        }
                    });
                });

                if row.response().clicked() {
                    clicked_row = Some(vi);
                }
            });
        });
        drop(store);

        if let Some(vi) = clicked_row {
            self.selected = Some(vi);
            self.follow_tail = false;
        }
        if let Some(txt) = copy_text {
            ui.ctx().copy_text(txt);
        }
        if let Some((target, value, exclude)) = quick {
            let (inc, exc) = match target {
                QuickTarget::Tag => (&mut self.filter.include_tags, &mut self.filter.exclude_tags),
                QuickTarget::Host => (
                    &mut self.filter.include_hosts,
                    &mut self.filter.exclude_hosts,
                ),
                QuickTarget::App => (&mut self.filter.include_apps, &mut self.filter.exclude_apps),
            };
            if exclude {
                if !exc.contains(&value) {
                    exc.push(value);
                }
            } else if !inc.contains(&value) {
                inc.push(value);
            }
            self.filter_dirty = true;
            self.show_filter = true;
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, t: &Texts) {
        ui.horizontal(|ui| {
            let store = self.store.lock().unwrap();
            let total = store.len();
            let unparsed = store.unparsed_count;
            let evicted = store.evicted_rows;
            drop(store);
            ui.label(format!("{}: {}", t.status_rows, group_digits(total as u64)));
            ui.label(format!(
                "{}: {}",
                t.status_filtered,
                group_digits(self.view.len() as u64)
            ));
            if unparsed > 0 {
                ui.label(format!("{}: {}", t.status_unparsed, group_digits(unparsed)));
            }
            if evicted > 0 {
                ui.colored_label(
                    Color32::YELLOW,
                    format!("{}: {}", t.status_evicted, group_digits(evicted)),
                );
            }
            if let Some(src) = &self.source {
                let dropped = src.dropped.load(Ordering::Relaxed);
                if dropped > 0 {
                    ui.colored_label(
                        Color32::RED,
                        format!("{}: {}", t.status_dropped, group_digits(dropped)),
                    );
                }
                if self.live {
                    ui.label(format!("{:.0} {}", self.rate, t.status_rate));
                }
            }
            if let Some(h) = &self.ingest {
                let aerr = h.stats.archive_errors.load(Ordering::Relaxed);
                if aerr > 0 {
                    ui.colored_label(
                        Color32::RED,
                        format!("{}: {}", t.status_archive_err, group_digits(aerr)),
                    );
                }
                if let Some(e) = h.stats.errors.lock().unwrap().last() {
                    ui.colored_label(Color32::RED, e);
                }
            }
            if self.is_loading() {
                ui.spinner();
                ui.label(t.status_loading);
            }
        });
    }

    // ---------- 高亮编辑器 ----------

    fn highlight_editor(&mut self, ctx: egui::Context, t: &Texts) {
        let mut open = self.show_hl_editor;
        let mut dirty = false;
        egui::Window::new(format!("{} — {}", t.highlight_rules, self.title))
            .id(egui::Id::new(("hl", self.id)))
            .open(&mut open)
            .default_width(560.0)
            .show(&ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(t.rule_add).clicked() {
                        self.rules.push(HighlightRule {
                            name: format!("rule{}", self.rules.len() + 1),
                            fg: Some([230, 80, 80]),
                            ..Default::default()
                        });
                        dirty = true;
                    }
                    if ui.button(t.rules_import).clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("JSON", &["json"])
                            .pick_file()
                        {
                            match std::fs::read_to_string(&p)
                                .map_err(anyhow::Error::from)
                                .and_then(|s| lv_core::highlight::rules_from_json(&s))
                            {
                                Ok(rules) => {
                                    self.rules = rules;
                                    dirty = true;
                                }
                                Err(e) => {
                                    self.toast = Some((format!("✘ {e}"), Instant::now()));
                                }
                            }
                        }
                    }
                    if ui.button(t.rules_export).clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_file_name("highlight-rules.json")
                            .save_file()
                        {
                            let json = lv_core::highlight::rules_to_json(&self.rules);
                            if let Err(e) = std::fs::write(&p, json) {
                                self.toast = Some((format!("✘ {e}"), Instant::now()));
                            }
                        }
                    }
                });
                ui.separator();
                let mut remove: Option<usize> = None;
                let mut swap: Option<(usize, usize)> = None;
                let n = self.rules.len();
                for (i, r) in self.rules.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        dirty |= ui.checkbox(&mut r.enabled, "").changed();
                        dirty |= ui
                            .add(
                                egui::TextEdit::singleline(&mut r.name)
                                    .hint_text(t.rule_name)
                                    .desired_width(80.0),
                            )
                            .changed();
                        dirty |= ui
                            .add(
                                egui::TextEdit::singleline(&mut r.query)
                                    .hint_text(t.rule_query)
                                    .desired_width(160.0),
                            )
                            .changed();
                        egui::ComboBox::from_id_salt(("hl-field", self.id, i))
                            .selected_text(field_label(r.field, t))
                            .width(70.0)
                            .show_ui(ui, |ui| {
                                for f in [
                                    MatchField::Msg,
                                    MatchField::Raw,
                                    MatchField::Tag,
                                    MatchField::App,
                                    MatchField::Host,
                                ] {
                                    dirty |= ui
                                        .selectable_value(&mut r.field, f, field_label(f, t))
                                        .changed();
                                }
                            });
                        dirty |= ui.checkbox(&mut r.is_regex, t.filter_regex).changed();
                        // 颜色
                        let mut fg = r.fg.unwrap_or([220, 220, 220]);
                        if ui.color_edit_button_srgb(&mut fg).changed() {
                            r.fg = Some(fg);
                            dirty = true;
                        }
                        let mut bg_on = r.bg.is_some();
                        if ui.checkbox(&mut bg_on, t.rule_bg).changed() {
                            r.bg = if bg_on { Some([60, 40, 40]) } else { None };
                            dirty = true;
                        }
                        if let Some(bg) = &mut r.bg {
                            if ui.color_edit_button_srgb(bg).changed() {
                                dirty = true;
                            }
                        }
                        dirty |= ui.checkbox(&mut r.bold, t.rule_bold).changed();
                        if ui.small_button("⬆").clicked() && i > 0 {
                            swap = Some((i, i - 1));
                        }
                        if ui.small_button("⬇").clicked() && i + 1 < n {
                            swap = Some((i, i + 1));
                        }
                        if ui.small_button("✖").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some((a, b)) = swap {
                    self.rules.swap(a, b);
                    dirty = true;
                }
                if let Some(i) = remove {
                    self.rules.remove(i);
                    dirty = true;
                }
                if let Some(set) = &self.hl {
                    for e in &set.errors {
                        ui.colored_label(Color32::RED, e);
                    }
                }
            });
        self.show_hl_editor = open;
        if dirty {
            self.hl_dirty = true;
        }
    }
}

#[derive(Clone, Copy)]
enum QuickTarget {
    Tag,
    Host,
    App,
}

fn quick_menu(
    resp: &egui::Response,
    t: &Texts,
    target: QuickTarget,
    value: &str,
    out: &mut Option<(QuickTarget, String, bool)>,
    copy: &mut Option<String>,
) {
    if value.is_empty() {
        return;
    }
    resp.clone().context_menu(|ui| {
        if ui
            .button(format!("{} = {}", t.quick_filter_include, value))
            .clicked()
        {
            *out = Some((target, value.to_owned(), false));
            ui.close();
        }
        if ui
            .button(format!("{} ≠ {}", t.quick_filter_exclude, value))
            .clicked()
        {
            *out = Some((target, value.to_owned(), true));
            ui.close();
        }
        if ui.button(t.copy_cell).clicked() {
            *copy = Some(value.to_owned());
            ui.close();
        }
    });
}

/// facet 行：值 chips，左键切换"包含"，右键菜单加"排除"。
fn facet_row(
    ui: &mut egui::Ui,
    t: &Texts,
    label: &str,
    values: &[(String, u64)],
    include: &mut Vec<String>,
    exclude: &mut Vec<String>,
) -> bool {
    if values.is_empty() {
        return false;
    }
    let mut dirty = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).strong());
        for (name, count) in values {
            let inc = include.contains(name);
            let exc = exclude.contains(name);
            let text = if exc {
                format!("✖ {name} ({count})")
            } else {
                format!("{name} ({count})")
            };
            let resp = ui.selectable_label(inc, text);
            if resp.clicked() {
                if inc {
                    include.retain(|v| v != name);
                } else {
                    include.push(name.clone());
                    exclude.retain(|v| v != name);
                }
                dirty = true;
            }
            resp.context_menu(|ui| {
                if ui.button(t.quick_filter_exclude).clicked() {
                    if exc {
                        exclude.retain(|v| v != name);
                    } else {
                        exclude.push(name.clone());
                        include.retain(|v| v != name);
                    }
                    dirty = true;
                    ui.close();
                }
            });
        }
        if (!include.is_empty() || !exclude.is_empty()) && ui.small_button("✖").clicked() {
            include.clear();
            exclude.clear();
            dirty = true;
        }
    });
    dirty
}

/// 时间范围编辑：RFC3339 文本框。
fn time_edit(ui: &mut egui::Ui, slot: &mut Option<i64>, tab_id: u64, which: u8) -> bool {
    let id = egui::Id::new(("time-edit", tab_id, which));
    let mut text: String = ui
        .ctx()
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| {
            slot.map(|us| lv_core::parse::ts::format_rfc3339_ms(us, 0))
                .unwrap_or_default()
        });
    let resp = ui.add(
        egui::TextEdit::singleline(&mut text)
            .hint_text("2026-06-12T10:16:41Z")
            .desired_width(210.0),
    );
    let mut dirty = false;
    if resp.changed() {
        let trimmed = text.trim();
        let new = if trimmed.is_empty() {
            None
        } else {
            parse_rfc3339(trimmed).map(|(us, _)| us)
        };
        if trimmed.is_empty() || new.is_some() {
            if *slot != new {
                *slot = new;
                dirty = true;
            }
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(id, text));
    dirty
}

fn field_label(f: MatchField, t: &Texts) -> &'static str {
    match f {
        MatchField::Msg => t.field_msg,
        MatchField::Raw => t.field_raw,
        MatchField::Tag => t.field_tag,
        MatchField::App => t.field_app,
        MatchField::Host => t.field_host,
    }
}

pub fn level_color(level: u8, parsed: bool, dark: bool) -> Color32 {
    if !parsed {
        return if dark {
            Color32::from_gray(140)
        } else {
            Color32::from_gray(110)
        };
    }
    match level {
        0..=2 => {
            if dark {
                Color32::from_rgb(255, 100, 160)
            } else {
                Color32::from_rgb(180, 0, 90)
            }
        }
        3 => {
            if dark {
                Color32::from_rgb(255, 90, 90)
            } else {
                Color32::from_rgb(200, 30, 30)
            }
        }
        4 => {
            if dark {
                Color32::from_rgb(240, 200, 60)
            } else {
                Color32::from_rgb(170, 130, 0)
            }
        }
        5 => {
            if dark {
                Color32::from_rgb(120, 200, 220)
            } else {
                Color32::from_rgb(0, 120, 150)
            }
        }
        7 => {
            if dark {
                Color32::from_gray(150)
            } else {
                Color32::from_gray(120)
            }
        }
        _ => {
            if dark {
                Color32::from_gray(220)
            } else {
                Color32::from_gray(40)
            }
        }
    }
}

fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
