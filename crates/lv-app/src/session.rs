//! 会话持久化（R2）：设置（语言/主题/已存过滤器）+ 打开的 Tab 及其
//! 视图状态（过滤、高亮、列布局、密度、时间模式），重启后恢复。
//! 合并 Tab 依赖运行期成员，不做恢复。

use std::path::PathBuf;

use lv_core::archive::ArchiveConfig;
use lv_core::filter::FilterSpec;
use lv_core::highlight::HighlightRule;

use crate::app::Settings;
use crate::fmt::TimeMode;
use crate::tab::ColumnsVisible;

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub enum SessionKind {
    File { paths: Vec<PathBuf> },
    Udp { bind: String, port: u16, archive: Option<ArchiveConfig> },
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(default)]
pub struct SessionTab {
    pub kind: Option<SessionKind>,
    pub title: String,
    pub filter: FilterSpec,
    pub rules: Vec<HighlightRule>,
    pub cols: ColumnsVisible,
    pub compact: bool,
    pub wrap: bool,
    pub time_mode: TimeMode,
    pub show_filter: bool,
    pub show_dashboard: bool,
}

impl Default for SessionTab {
    fn default() -> Self {
        Self {
            kind: None,
            title: String::new(),
            filter: FilterSpec::default(),
            rules: Vec::new(),
            cols: ColumnsVisible::default(),
            compact: true,
            wrap: false,
            time_mode: TimeMode::AbsOriginal,
            show_filter: false,
            show_dashboard: false,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct Session {
    pub settings: Settings,
    pub tabs: Vec<SessionTab>,
    pub active: usize,
}

fn session_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logviewer")
        .join("session.json")
}

pub fn load() -> Option<Session> {
    let s = std::fs::read_to_string(session_path()).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save(session: &Session) {
    let path = session_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(session) {
        let _ = std::fs::write(path, json);
    }
}
