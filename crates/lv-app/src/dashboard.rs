//! 仪表盘面板（FR-9）：时间桶堆叠柱状图、错误率、Top 维度、速率、
//! 阈值高亮；点击柱状图下钻为时间范围过滤。

use std::time::Instant;

use eframe::egui::{self, Color32, RichText};
use egui_plot::{Bar, BarChart, Legend, Plot};

use lv_core::stats::{compute_dash, Bucket, DashStats};

use crate::i18n::Texts;
use crate::tab::Tab;

pub struct DashState {
    pub stats: DashStats,
    last_compute: Instant,
    last_rev: u64,
}

impl Default for DashState {
    fn default() -> Self {
        Self {
            stats: DashStats::default(),
            last_compute: Instant::now(),
            last_rev: u64::MAX,
        }
    }
}

pub fn dashboard_ui(tab: &mut Tab, ui: &mut egui::Ui, t: &Texts) {
    // 节流重算：视图变化且距上次 > 500ms（首帧立即）
    let s: DashStats = {
        let need = tab.dash.last_rev == u64::MAX
            || (tab.dash.last_rev != tab.view_rev
                && tab.dash.last_compute.elapsed().as_millis() > 500);
        if need {
            let store = tab.store.lock().unwrap();
            tab.dash.stats = compute_dash(&store, 120);
            drop(store);
            tab.dash.last_compute = Instant::now();
            tab.dash.last_rev = tab.view_rev;
        }
        tab.dash.stats.clone()
    };

    ui.horizontal(|ui| {
        // 左：堆叠柱状图
        let plot_w = (ui.available_width() * 0.6).max(280.0);
        ui.vertical(|ui| {
            ui.set_width(plot_w);
            ui.label(RichText::new(t.dash_levels).strong());
            let to_x = |i: usize| (s.start_us + i as i64 * s.bucket_us) as f64 / 1e6;
            let w = (s.bucket_us as f64 / 1e6 * 0.9).max(0.001);
            let bars = |f: fn(&Bucket) -> u32, color: Color32, name: &str| {
                BarChart::new(
                    name.to_owned(),
                    s.buckets
                        .iter()
                        .enumerate()
                        .map(|(i, b)| Bar::new(to_x(i), f(b) as f64).width(w))
                        .collect(),
                )
                .color(color)
            };
            let c_err = bars(|b| b.err, Color32::from_rgb(230, 70, 70), "err+");
            let c_warn =
                bars(|b| b.warn, Color32::from_rgb(230, 190, 60), "warning").stack_on(&[&c_err]);
            let c_info = bars(|b| b.info, Color32::from_rgb(110, 160, 220), "info")
                .stack_on(&[&c_err, &c_warn]);
            let c_dbg = bars(|b| b.debug, Color32::from_gray(140), "debug")
                .stack_on(&[&c_err, &c_warn, &c_info]);
            let mut clicked_x: Option<f64> = None;
            Plot::new(("dash-plot", tab.id))
                .height(150.0)
                .legend(Legend::default())
                .allow_drag(false)
                .allow_zoom(false)
                .allow_scroll(false)
                .show(ui, |plot_ui| {
                    plot_ui.bar_chart(c_err);
                    plot_ui.bar_chart(c_warn);
                    plot_ui.bar_chart(c_info);
                    plot_ui.bar_chart(c_dbg);
                    if plot_ui.response().clicked() {
                        clicked_x = plot_ui.pointer_coordinate().map(|p| p.x);
                    }
                });
            ui.label(RichText::new(t.dash_click_hint).weak().small());
            // 下钻：点击桶 → 时间范围过滤（FR-9）
            if let Some(x) = clicked_x {
                if s.bucket_us > 0 && !s.buckets.is_empty() {
                    let us = (x * 1e6) as i64;
                    let idx =
                        ((us - s.start_us) / s.bucket_us).clamp(0, s.buckets.len() as i64 - 1);
                    let from = s.start_us + idx * s.bucket_us;
                    tab.filter.time_from_us = Some(from);
                    tab.filter.time_to_us = Some(from + s.bucket_us);
                    tab.filter_dirty = true;
                    tab.show_filter = true;
                }
            }
        });
        ui.separator();
        // 右：指标 + Top 列表
        ui.vertical(|ui| {
            let err_pct = s.err_rate * 100.0;
            let over = err_pct > tab.dash_threshold;
            ui.horizontal(|ui| {
                ui.label(format!("{}: {}", t.dash_total, s.total_rows));
                let txt = format!("{}: {:.2}%", t.dash_err_rate, err_pct);
                if over {
                    ui.colored_label(
                        Color32::from_rgb(255, 70, 70),
                        RichText::new(txt).strong(),
                    );
                } else {
                    ui.label(txt);
                }
                if tab.live {
                    ui.label(format!("{}: {:.0}/s", t.dash_rate_now, tab.rate));
                }
            });
            ui.horizontal(|ui| {
                ui.label(t.dash_threshold);
                ui.add(
                    egui::DragValue::new(&mut tab.dash_threshold)
                        .range(0.0..=100.0)
                        .speed(0.5),
                );
            });
            ui.separator();
            ui.columns(3, |cols| {
                if let Some(v) = top_list(&mut cols[0], t.dash_top_tags, &s.top_tags) {
                    if !tab.filter.include_tags.contains(&v) {
                        tab.filter.include_tags.push(v);
                        tab.filter_dirty = true;
                        tab.show_filter = true;
                    }
                }
                if let Some(v) = top_list(&mut cols[1], t.dash_top_apps, &s.top_apps) {
                    if !tab.filter.include_apps.contains(&v) {
                        tab.filter.include_apps.push(v);
                        tab.filter_dirty = true;
                        tab.show_filter = true;
                    }
                }
                if let Some(v) = top_list(&mut cols[2], t.dash_top_hosts, &s.top_hosts) {
                    if !tab.filter.include_hosts.contains(&v) {
                        tab.filter.include_hosts.push(v);
                        tab.filter_dirty = true;
                        tab.show_filter = true;
                    }
                }
            });
        });
    });
    ui.separator();
}

/// 渲染 Top 列表；点击某项返回其值（用于下钻过滤）。
fn top_list(ui: &mut egui::Ui, title: &str, items: &[(String, u64)]) -> Option<String> {
    let mut clicked = None;
    ui.label(RichText::new(title).strong());
    for (name, count) in items {
        if ui
            .add(egui::Label::new(format!("{name} · {count}")).sense(egui::Sense::click()))
            .clicked()
        {
            clicked = Some(name.clone());
        }
    }
    clicked
}
