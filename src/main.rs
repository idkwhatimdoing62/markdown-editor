#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod export;
#[cfg(target_os = "windows")]
mod file_association;
mod html_image;
mod io;
mod markdown;
mod preview;
mod search;
mod single_instance;
mod storage;
mod theme;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod web_preview;
mod window_close;
mod window_session;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc, mpsc,
    mpsc::{Receiver, Sender},
};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::containers::scroll_area::ScrollAreaOutput;
use markdown::{Block, ParsedDocument};
use notify::Watcher;
use theme::{ThemePackage, ThemeSpec};

#[cfg(target_os = "macos")]
const PRIMARY_SHORTCUT: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const PRIMARY_SHORTCUT: &str = "Ctrl";

const EXTERNAL_POLL_INTERVAL: f64 = 0.35;
const PREVIEW_REFRESH_DEBOUNCE_SECONDS: f64 = 0.2;
const EXTERNAL_STABLE_DELAY: f64 = 0.45;

fn main() -> eframe::Result {
    let launch = LaunchOptions::from_env();
    let restore_previous_window = launch.should_restore_window();
    let instance_requests = if !launch.uses_single_instance() {
        None
    } else {
        match single_instance::acquire(launch.open_paths.clone()) {
            single_instance::Acquisition::Primary(receiver) => Some(receiver),
            single_instance::Acquisition::Forwarded => return Ok(()),
            single_instance::Acquisition::Unavailable(error) => {
                rfd::MessageDialog::new()
                    .set_title("Markdown 编辑器与预览器")
                    .set_description(&error)
                    .set_level(rfd::MessageLevel::Error)
                    .show();
                return Ok(());
            }
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([720.0, 480.0])
            .with_icon(app_icon())
            .with_title("Markdown 编辑器与预览器"),
        ..Default::default()
    };
    let draft_window_id = launch.force_new_window.then_some(std::process::id());
    let previous_window = restore_previous_window
        .then(|| window_session::load(draft_window_id))
        .flatten();
    eframe::run_native(
        "markdown-editor",
        options,
        Box::new(move |cc| {
            let mut app = MdEditorApp::new(cc, draft_window_id, restore_previous_window);
            app.instance_requests = instance_requests;
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            if let Some(report_path) = launch.benchmark_report {
                app.view_mode = ViewMode::Preview;
                app.benchmark_probe = Some(BenchmarkProbe {
                    started: Instant::now(),
                    report_path,
                    completed: false,
                });
            }
            if let Some(session) = previous_window {
                app.restore_window_session(session);
            }
            for path in launch.open_paths {
                app.open_path(&path);
            }
            Ok(Box::new(app))
        }),
    )
}

struct LaunchOptions {
    open_paths: Vec<PathBuf>,
    force_new_window: bool,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    benchmark_report: Option<PathBuf>,
}

impl LaunchOptions {
    fn from_env() -> Self {
        Self::from_args(std::env::args().skip(1))
    }

    fn from_args(arguments: impl IntoIterator<Item = String>) -> Self {
        let mut open_paths = Vec::new();
        let mut force_new_window = false;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let mut benchmark_report = None;
        let mut args = arguments.into_iter();
        while let Some(argument) = args.next() {
            if argument == "--new-window" {
                force_new_window = true;
                continue;
            }
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            if argument == "--benchmark-webview-report" {
                benchmark_report = args.next().map(PathBuf::from);
                continue;
            }
            if !argument.starts_with('-') {
                let path = PathBuf::from(argument);
                let absolute = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .map(|directory| directory.join(&path))
                        .unwrap_or(path)
                };
                open_paths.push(absolute.canonicalize().unwrap_or(absolute));
            }
        }
        Self {
            open_paths,
            force_new_window,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            benchmark_report,
        }
    }

    fn uses_single_instance(&self) -> bool {
        !self.force_new_window && !self.is_benchmark()
    }

    fn should_restore_window(&self) -> bool {
        self.open_paths.is_empty() && !self.force_new_window && !self.is_benchmark()
    }

    fn is_benchmark(&self) -> bool {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            self.benchmark_report.is_some()
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            false
        }
    }
}

fn app_icon() -> Arc<egui::IconData> {
    let image = image::load_from_memory(include_bytes!("../assets/app-icon-256.png"))
        .expect("内置应用图标应为有效 PNG")
        .into_rgba8();
    let (width, height) = image.dimensions();
    Arc::new(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum DocStatus {
    Unsaved,
    Saved,
    Modified,
    Conflict,
    SaveFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Write,
    Preview,
    Split,
}

#[derive(Clone)]
struct DocumentTab {
    id: u64,
    text: String,
    path: Option<PathBuf>,
    disk_snapshot: Vec<u8>,
    status: DocStatus,
    document: ParsedDocument,
    document_revision: u64,
    parse_pending: bool,
    status_note: String,
    conflict: Option<PathBuf>,
    draft_last_write: f64,
    last_edit_time: f64,
    prev_editor_ratio: f32,
    prev_preview_ratio: f32,
    preview_source_position: f32,
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    last_caret_line: usize,
}

struct PendingExternalChange {
    stamp: io::FileStamp,
    bytes: Vec<u8>,
    first_seen: f64,
}

struct ExternalFileWatcher {
    watcher: notify::RecommendedWatcher,
    receiver: Receiver<notify::Result<notify::Event>>,
    watched: HashSet<PathBuf>,
}

impl ExternalFileWatcher {
    fn new() -> Option<Self> {
        let (sender, receiver) = mpsc::channel();
        let watcher = notify::recommended_watcher(move |result| {
            let _ = sender.send(result);
        })
        .ok()?;
        Some(Self {
            watcher,
            receiver,
            watched: HashSet::new(),
        })
    }

    fn watch(&mut self, path: &Path) {
        if self.watched.contains(path) {
            return;
        }
        if self
            .watcher
            .watch(path, notify::RecursiveMode::NonRecursive)
            .is_ok()
        {
            self.watched.insert(path.to_path_buf());
        }
    }

    fn unwatch(&mut self, path: &Path) {
        if self.watched.remove(path) {
            let _ = self.watcher.unwatch(path);
        }
    }

    fn drain_changed_paths(&self) -> HashSet<PathBuf> {
        self.receiver
            .try_iter()
            .filter_map(Result::ok)
            .flat_map(|event| event.paths)
            .collect()
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct BenchmarkProbe {
    started: Instant,
    report_path: PathBuf,
    completed: bool,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserDocumentKey {
    tab_id: u64,
    document_revision: u64,
    document_source_hash: u64,
    theme_revision: u64,
    body_font_size_bits: u32,
    base_directory: Option<PathBuf>,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl BrowserDocumentKey {
    fn same_render_context(&self, other: &Self) -> bool {
        self.tab_id == other.tab_id
            && self.theme_revision == other.theme_revision
            && self.base_directory == other.base_directory
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn source_hash(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct BrowserDocumentCache {
    key: BrowserDocumentKey,
    document: Arc<web_preview::PreviewDocument>,
    parsed_document: Arc<ParsedDocument>,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct RenderRequest {
    key: BrowserDocumentKey,
    document: ParsedDocument,
    previous: Option<Arc<web_preview::PreviewDocument>>,
    previous_parsed: Option<Arc<ParsedDocument>>,
    css: String,
    base_directory: Option<PathBuf>,
    font_size_override: Option<f32>,
    dark_mode_css: Option<String>,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct RenderResult {
    key: BrowserDocumentKey,
    document: Arc<web_preview::PreviewDocument>,
    parsed_document: Arc<ParsedDocument>,
    metrics: RenderTelemetry,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn render_result_matches_current(
    result: &RenderResult,
    current_key: &BrowserDocumentKey,
    current_text: &str,
) -> bool {
    result.key == *current_key && result.parsed_document.source() == current_text
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Debug, Clone, Copy, Default)]
struct RenderTelemetry {
    elapsed_ms: f64,
    replaced_blocks: usize,
    replaced_virtual_chunks: usize,
    full_render: bool,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Default)]
struct RenderMetrics {
    last: RenderTelemetry,
    samples_ms: VecDeque<f64>,
    full_render_count: usize,
    webview_navigation_count: usize,
    replaced_blocks: usize,
    replaced_virtual_chunks: usize,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl RenderMetrics {
    fn record(&mut self, telemetry: RenderTelemetry) {
        self.last = telemetry;
        self.full_render_count += usize::from(telemetry.full_render);
        self.replaced_blocks += telemetry.replaced_blocks;
        self.replaced_virtual_chunks += telemetry.replaced_virtual_chunks;
        self.samples_ms.push_back(telemetry.elapsed_ms);
        if self.samples_ms.len() > 128 {
            self.samples_ms.pop_front();
        }
    }

    fn edit_p95_ms(&self) -> f64 {
        if self.samples_ms.is_empty() {
            return 0.0;
        }
        let mut samples = self.samples_ms.iter().copied().collect::<Vec<_>>();
        samples.sort_by(f64::total_cmp);
        let index = ((samples.len() - 1) * 95).div_ceil(100);
        samples[index]
    }

    fn record_webview_navigation(&mut self) {
        self.webview_navigation_count += 1;
    }

    fn snapshot_json(&self) -> serde_json::Value {
        serde_json::json!({
            "last_render_ms": self.last.elapsed_ms,
            "edit_p95_ms": self.edit_p95_ms(),
            "render_fallback_count": self.full_render_count,
            "webview_navigation_count": self.webview_navigation_count,
            "replaced_blocks": self.replaced_blocks,
            "replaced_virtual_chunks": self.replaced_virtual_chunks,
            "sample_count": self.samples_ms.len(),
        })
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct RenderWorker {
    requests: Sender<RenderRequest>,
    results: Receiver<RenderResult>,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl RenderWorker {
    fn new() -> Self {
        let (request_sender, request_receiver) = mpsc::channel::<RenderRequest>();
        let (result_sender, result_receiver) = mpsc::channel::<RenderResult>();
        std::thread::spawn(move || {
            while let Ok(mut request) = request_receiver.recv() {
                while let Ok(newer) = request_receiver.try_recv() {
                    request = newer;
                }
                let started = Instant::now();
                let parsed_document = Arc::new(request.document.clone());
                let mut full_render = false;
                let document = if let Some(document) =
                    web_preview::preview_document_virtual_incremental(
                        request.previous.as_deref(),
                        request.previous_parsed.as_deref(),
                        &request.document,
                        request.base_directory.as_deref(),
                    ) {
                    document
                } else if let Some(document) = web_preview::preview_document_incremental(
                    request.previous.as_deref(),
                    request.previous_parsed.as_deref(),
                    &request.document,
                    request.base_directory.as_deref(),
                ) {
                    document
                } else if let Some(document) = web_preview::preview_document_with_previous(
                    request.previous.as_deref(),
                    &request.document,
                ) {
                    document
                } else {
                    full_render = true;
                    web_preview::preview_document(
                        &request.document,
                        &request.css,
                        request.base_directory.as_deref(),
                        request.font_size_override,
                        request.dark_mode_css.as_deref(),
                    )
                };
                let update_stats = document.update_stats();
                let metrics = RenderTelemetry {
                    elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                    replaced_blocks: update_stats.replaced_blocks,
                    replaced_virtual_chunks: update_stats.replaced_virtual_chunks,
                    full_render,
                };
                let document = Arc::new(document);
                if result_sender
                    .send(RenderResult {
                        key: request.key,
                        document,
                        parsed_document,
                        metrics,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            requests: request_sender,
            results: result_receiver,
        }
    }

    fn submit(&self, request: RenderRequest) {
        let _ = self.requests.send(request);
    }

    fn try_recv(&self) -> Result<RenderResult, mpsc::TryRecvError> {
        self.results.try_recv()
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct ParseRequest {
    tab_id: u64,
    revision: u64,
    text: String,
    previous: Option<ParsedDocument>,
}

struct ParseResult {
    tab_id: u64,
    revision: u64,
    document: ParsedDocument,
}

struct ParseWorker {
    requests: Sender<ParseRequest>,
    results: Receiver<ParseResult>,
}

impl ParseWorker {
    fn new() -> Self {
        let (request_sender, request_receiver) = mpsc::channel::<ParseRequest>();
        let (result_sender, result_receiver) = mpsc::channel::<ParseResult>();
        std::thread::spawn(move || {
            while let Ok(mut request) = request_receiver.recv() {
                // If edits arrived while parsing was busy, skip queued intermediate
                // revisions and parse only the newest snapshot.
                while let Ok(newer) = request_receiver.try_recv() {
                    request = newer;
                }
                let document = request
                    .previous
                    .as_ref()
                    .and_then(|previous| {
                        markdown::parse_document_incremental(previous, &request.text)
                    })
                    .unwrap_or_else(|| {
                        markdown::parse_document_with_previous(
                            request.previous.as_ref(),
                            &request.text,
                        )
                    });
                let result = ParseResult {
                    tab_id: request.tab_id,
                    revision: request.revision,
                    document,
                };
                if result_sender.send(result).is_err() {
                    break;
                }
            }
        });
        Self {
            requests: request_sender,
            results: result_receiver,
        }
    }

    fn submit(&self, request: ParseRequest) {
        let _ = self.requests.send(request);
    }

    fn try_recv(&self) -> Result<ParseResult, mpsc::TryRecvError> {
        self.results.try_recv()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalChangeResult {
    Unchanged,
    Reloaded,
    Reconciled,
    Conflict,
}

enum ExternalProbe {
    Waiting,
    Stable(Vec<u8>),
    Missing(String),
}

impl DocumentTab {
    fn blank(id: u64) -> Self {
        Self {
            id,
            text: String::new(),
            path: None,
            disk_snapshot: Vec::new(),
            status: DocStatus::Unsaved,
            document: markdown::parse_document(""),
            document_revision: 0,
            parse_pending: false,
            status_note: String::new(),
            conflict: None,
            draft_last_write: 0.0,
            last_edit_time: f64::INFINITY,
            prev_editor_ratio: 0.0,
            prev_preview_ratio: 0.0,
            preview_source_position: 0.0,
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            last_caret_line: 0,
        }
    }

    fn from_file(id: u64, path: PathBuf, text: String, snapshot: Vec<u8>) -> Self {
        let document = markdown::parse_document(&text);
        Self {
            id,
            text,
            path: Some(path),
            disk_snapshot: snapshot,
            status: DocStatus::Saved,
            document,
            document_revision: 1,
            parse_pending: false,
            status_note: String::new(),
            conflict: None,
            draft_last_write: 0.0,
            last_edit_time: f64::INFINITY,
            prev_editor_ratio: 0.0,
            prev_preview_ratio: 0.0,
            preview_source_position: 0.0,
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            last_caret_line: 0,
        }
    }
}

fn document_is_dirty(
    path: Option<&PathBuf>,
    text: &str,
    snapshot: &[u8],
    status: &DocStatus,
) -> bool {
    matches!(status, DocStatus::Conflict)
        || match path {
            Some(_) => !snapshot_matches_text(snapshot, text),
            None => !text.is_empty(),
        }
}

fn snapshot_matches_text(snapshot: &[u8], text: &str) -> bool {
    io::decode_markdown_bytes(snapshot).is_ok_and(|snapshot_text| snapshot_text == text)
}

fn apply_external_bytes(
    tab: &mut DocumentTab,
    bytes: Vec<u8>,
) -> Result<ExternalChangeResult, io::ReadError> {
    if bytes == tab.disk_snapshot {
        return Ok(ExternalChangeResult::Unchanged);
    }
    let disk_text = io::decode_markdown_bytes(&bytes)?;
    if disk_text == tab.text {
        tab.disk_snapshot = bytes;
        tab.status = DocStatus::Saved;
        tab.conflict = None;
        tab.status_note = "已同步外部保存".to_string();
        return Ok(ExternalChangeResult::Reconciled);
    }

    let has_local_changes = !snapshot_matches_text(&tab.disk_snapshot, &tab.text)
        || matches!(tab.status, DocStatus::Conflict);
    if has_local_changes {
        tab.status = DocStatus::Conflict;
        tab.conflict = tab.path.clone();
        tab.status_note = "检测到外部修改，本地未保存内容已保留".to_string();
        return Ok(ExternalChangeResult::Conflict);
    }

    tab.text = disk_text;
    tab.disk_snapshot = bytes;
    tab.document = markdown::parse_document(&tab.text);
    tab.document_revision = tab.document_revision.wrapping_add(1);
    tab.parse_pending = false;
    tab.status = DocStatus::Saved;
    tab.conflict = None;
    tab.status_note = format!("已自动加载外部修改 {}", clock_time());
    Ok(ExternalChangeResult::Reloaded)
}

fn restore_draft_tab(draft: io::DraftTab) -> DocumentTab {
    let stored_snapshot = draft.disk_snapshot().unwrap_or_default();
    let (disk_snapshot, status, conflict, status_note) = match draft.path.as_ref() {
        Some(path) => match io::read_snapshot_checked(path) {
            Ok(current) if snapshot_matches_text(&current, &draft.text) => (
                current,
                DocStatus::Saved,
                None,
                "草稿内容已与磁盘一致".to_string(),
            ),
            Ok(current) if current == stored_snapshot => (
                stored_snapshot,
                DocStatus::Modified,
                None,
                "已恢复未保存草稿".to_string(),
            ),
            Ok(_) => (
                stored_snapshot,
                DocStatus::Conflict,
                Some(path.clone()),
                "恢复草稿时检测到磁盘文件已变化".to_string(),
            ),
            Err(error) => (
                stored_snapshot,
                DocStatus::Modified,
                None,
                format!(
                    "已恢复草稿；原文件暂时无法读取：{}",
                    describe_read_error(&error)
                ),
            ),
        },
        None => (
            stored_snapshot,
            DocStatus::Modified,
            None,
            "已恢复未命名草稿".to_string(),
        ),
    };
    let document = markdown::parse_document(&draft.text);
    DocumentTab {
        id: draft.id,
        text: draft.text,
        path: draft.path,
        disk_snapshot,
        status,
        document,
        document_revision: 1,
        parse_pending: false,
        status_note,
        conflict,
        draft_last_write: 0.0,
        last_edit_time: f64::NEG_INFINITY,
        prev_editor_ratio: 0.0,
        prev_preview_ratio: 0.0,
        preview_source_position: 0.0,
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        last_caret_line: 0,
    }
}

fn document_label(id: u64, path: Option<&PathBuf>, dirty: bool) -> String {
    let title = path
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("未命名 {id}"));
    if dirty {
        format!("{title}  •")
    } else {
        title
    }
}

fn shortened_tab_title(title: &str) -> String {
    const LIMIT: usize = 22;
    if title.chars().count() <= LIMIT {
        return title.to_string();
    }
    let head = title.chars().take(12).collect::<String>();
    let tail = title
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head}…{tail}")
}

const CHROME_FONT_SIZE: f32 = 16.0;
const CHROME_CONTROL_HEIGHT: f32 = 34.0;
const CHROME_BAR_HEIGHT: f32 = 40.0;

fn document_tab_button(
    ui: &mut egui::Ui,
    id: u64,
    title: &str,
    dirty: bool,
    selected: bool,
) -> (bool, bool) {
    let title = shortened_tab_title(title);
    let font = egui::FontId::new(CHROME_FONT_SIZE, egui::FontFamily::Proportional);
    let text_color = if selected {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().widgets.inactive.fg_stroke.color
    };
    let natural_galley = ui
        .painter()
        .layout_no_wrap(title.clone(), font.clone(), text_color);
    let width = (natural_galley.size().x + 58.0).clamp(96.0, 220.0);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, CHROME_CONTROL_HEIGHT),
        egui::Sense::hover(),
    );
    let tab_response = ui.interact(
        rect,
        ui.id().with(("document-tab", id)),
        egui::Sense::click(),
    );
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 15.0, rect.center().y),
        egui::vec2(22.0, 22.0),
    );
    let close_response = ui.interact(
        close_rect,
        ui.id().with(("document-tab-close", id)),
        egui::Sense::click(),
    );
    let hovered = tab_response.hovered() || close_response.hovered();

    if selected {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().window_fill);
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 8.0, rect.bottom() - 1.0),
                egui::pos2(rect.right() - 8.0, rect.bottom() - 1.0),
            ],
            egui::Stroke::new(2.0, ui.visuals().strong_text_color()),
        );
    } else if hovered {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            ui.visuals().widgets.hovered.weak_bg_fill,
        );
    }

    // Long CJK filenames can be much wider than their character count suggests.
    // Keep the title inside its own lane so it can never cover the dirty marker
    // or the close button.
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 12.0, rect.top()),
        egui::pos2(
            close_rect.left() - if dirty { 12.0 } else { 4.0 },
            rect.bottom(),
        ),
    );
    let mut title_job = egui::text::LayoutJob::simple(title, font, text_color, text_rect.width());
    title_job.wrap = egui::text::TextWrapping::truncate_at_width(text_rect.width());
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(title_job));
    let text_pos = egui::pos2(
        text_rect.left(),
        text_rect.center().y - galley.size().y / 2.0,
    );
    ui.painter()
        .with_clip_rect(text_rect.intersect(ui.clip_rect()))
        .galley(text_pos, galley, text_color);

    if dirty {
        ui.painter().circle_filled(
            egui::pos2(close_rect.left() - 5.0, rect.center().y),
            2.5,
            ui.visuals().warn_fg_color,
        );
    }
    // Draw the close icon as geometry instead of a font glyph. Some CJK font
    // fallbacks do not contain U+00D7, which used to make the button invisible.
    let close_color = if close_response.hovered() {
        ui.visuals().strong_text_color()
    } else if selected {
        ui.visuals().widgets.inactive.fg_stroke.color
    } else {
        ui.visuals().weak_text_color()
    };
    let close_center = close_rect.center();
    let close_half = if close_response.hovered() { 4.5 } else { 4.0 };
    let close_stroke = egui::Stroke::new(
        if close_response.hovered() { 1.8 } else { 1.45 },
        close_color,
    );
    ui.painter().line_segment(
        [
            close_center + egui::vec2(-close_half, -close_half),
            close_center + egui::vec2(close_half, close_half),
        ],
        close_stroke,
    );
    ui.painter().line_segment(
        [
            close_center + egui::vec2(-close_half, close_half),
            close_center + egui::vec2(close_half, -close_half),
        ],
        close_stroke,
    );

    let close_clicked = close_response.clicked();
    if close_response.hovered() {
        close_response.on_hover_text("关闭标签");
    }
    (tab_response.clicked() && !close_clicked, close_clicked)
}

fn chrome_nav_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(46.0, CHROME_CONTROL_HEIGHT),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            ui.visuals().widgets.hovered.weak_bg_fill,
        );
    }
    let color = if selected {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().widgets.inactive.fg_stroke.color
    };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::new(CHROME_FONT_SIZE, egui::FontFamily::Proportional),
        color,
    );
    if selected {
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 12.0, rect.bottom() - 1.0),
                egui::pos2(rect.right() - 12.0, rect.bottom() - 1.0),
            ],
            egui::Stroke::new(1.5, ui.visuals().strong_text_color()),
        );
    }
    response
}

fn heading_title(inlines: &[markdown::Inline]) -> String {
    fn append(inlines: &[markdown::Inline], output: &mut String) {
        for inline in inlines {
            match inline {
                markdown::Inline::Text(text) | markdown::Inline::Code(text) => {
                    output.push_str(text)
                }
                markdown::Inline::Emphasis(children)
                | markdown::Inline::Strong(children)
                | markdown::Inline::Strikethrough(children)
                | markdown::Inline::Link { children, .. } => append(children, output),
                markdown::Inline::Image { alt, .. } => output.push_str(alt),
                markdown::Inline::SoftBreak | markdown::Inline::HardBreak => output.push(' '),
            }
        }
    }

    let mut title = String::new();
    append(inlines, &mut title);
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn reading_headings(blocks: &[Block]) -> Vec<(u8, String)> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Heading { level, inlines } => Some((*level, heading_title(inlines))),
            _ => None,
        })
        .filter(|(_, title)| !title.is_empty())
        .collect()
}

fn reading_toc(ui: &mut egui::Ui, blocks: &[Block]) -> Option<usize> {
    let headings = reading_headings(blocks);
    ui.add_space(10.0);
    ui.label(
        egui::RichText::new("章节目录")
            .size(15.0)
            .strong()
            .color(ui.visuals().strong_text_color()),
    );
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(5.0);

    if headings.is_empty() {
        ui.label(
            egui::RichText::new("当前文档没有标题")
                .size(13.0)
                .color(ui.visuals().weak_text_color()),
        );
        return None;
    }

    let mut target = None;
    egui::ScrollArea::vertical()
        .id_salt("reading_toc_scroll")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            for (index, (level, title)) in headings.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_space((level.saturating_sub(1) as f32) * 12.0);
                    let response = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(title)
                                    .size(13.5)
                                    .color(ui.visuals().text_color()),
                            )
                            .frame(false)
                            .truncate(),
                        )
                        .on_hover_text(title);
                    if response.clicked() {
                        target = Some(index);
                    }
                });
            }
        });
    target
}

fn chrome_icon_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(28.0, CHROME_CONTROL_HEIGHT),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            ui.visuals().widgets.hovered.weak_bg_fill,
        );
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::new(CHROME_FONT_SIZE, egui::FontFamily::Proportional),
        ui.visuals().widgets.inactive.fg_stroke.color,
    );
    response
}

struct MdEditorApp {
    tabs: Vec<DocumentTab>,
    active_tab: usize,
    next_tab_id: u64,
    workspace_empty: bool,
    search_open: bool,
    search_query: String,
    search_results: search::SearchResults,
    search_tab_id: Option<u64>,
    search_document_revision: u64,
    search_focus_requested: bool,
    search_scroll_requested: bool,
    search_backwards: bool,
    pending_close: Option<usize>,
    window_close_guard: window_close::CloseGuard,
    recovery: Option<io::DraftSession>,
    dark: bool,
    editor_focused: bool,
    view_mode: ViewMode,
    focus_mode: bool,
    show_status: bool,
    body_font_size: f32,
    theme_package: Option<ThemePackage>,
    theme_revision: u64,
    auto_reload_external: bool,
    last_external_poll: f64,
    external_watcher: Option<ExternalFileWatcher>,
    observed_file_stamps: HashMap<PathBuf, io::FileStamp>,
    pending_external_changes: HashMap<PathBuf, PendingExternalChange>,
    parse_worker: ParseWorker,
    instance_requests: Option<Receiver<single_instance::OpenRequest>>,
    draft_window_id: Option<u32>,
    persisted_window_session: Option<window_session::WindowSession>,
    window_session_initialized: bool,
    pending_preview_restore: Option<(u64, f32)>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    browser_preview: web_preview::BrowserPreview,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    benchmark_probe: Option<BenchmarkProbe>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    browser_document_cache: Option<BrowserDocumentCache>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    render_worker: RenderWorker,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    render_pending: Option<BrowserDocumentKey>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    render_metrics: RenderMetrics,
}

impl std::ops::Deref for MdEditorApp {
    type Target = DocumentTab;

    fn deref(&self) -> &Self::Target {
        &self.tabs[self.active_tab]
    }
}

impl std::ops::DerefMut for MdEditorApp {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tabs[self.active_tab]
    }
}

impl MdEditorApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        draft_window_id: Option<u32>,
        restore_previous_window: bool,
    ) -> Self {
        setup_fonts(&cc.egui_ctx);
        let theme_package = theme::load_saved();
        let built_in_theme = ThemePackage::built_in_sspai();
        let initial_body_font_size = theme_package
            .as_ref()
            .map(ThemePackage::recommended_body_font_size)
            .unwrap_or_else(|| built_in_theme.recommended_body_font_size());
        let initial_dark = theme::load_dark_mode();
        let initial_theme = theme_package
            .as_ref()
            .and_then(|t| t.spec(initial_dark).ok())
            .or_else(|| built_in_theme.spec(initial_dark).ok())
            .unwrap_or_else(|| ThemeSpec::fallback(initial_dark));
        apply_visuals(&cc.egui_ctx, initial_dark, &initial_theme);
        let recovery = restore_previous_window.then(io::load_draft).flatten();
        let initial_tab = DocumentTab::blank(1);
        Self {
            tabs: vec![initial_tab],
            active_tab: 0,
            next_tab_id: 2,
            workspace_empty: true,
            search_open: false,
            search_query: String::new(),
            search_results: search::SearchResults::default(),
            search_tab_id: None,
            search_document_revision: 0,
            search_focus_requested: false,
            search_scroll_requested: false,
            search_backwards: false,
            pending_close: None,
            window_close_guard: window_close::CloseGuard::default(),
            recovery,
            dark: initial_dark,
            editor_focused: false,
            view_mode: ViewMode::Write,
            focus_mode: false,
            show_status: true,
            body_font_size: initial_body_font_size,
            theme_package,
            theme_revision: 1,
            auto_reload_external: true,
            last_external_poll: f64::NEG_INFINITY,
            external_watcher: ExternalFileWatcher::new(),
            observed_file_stamps: HashMap::new(),
            pending_external_changes: HashMap::new(),
            parse_worker: ParseWorker::new(),
            instance_requests: None,
            draft_window_id,
            persisted_window_session: None,
            window_session_initialized: false,
            pending_preview_restore: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            browser_preview: web_preview::BrowserPreview::default(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            benchmark_probe: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            browser_document_cache: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            render_worker: RenderWorker::new(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            render_pending: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            render_metrics: RenderMetrics::default(),
        }
    }

    fn activate_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if let Some(source_position) = self.browser_preview.source_position() {
            self.tabs[self.active_tab].preview_source_position = source_position;
        }
        let tab = &self.tabs[index];
        self.pending_preview_restore = Some((tab.id, tab.preview_source_position));
        self.active_tab = index;
        self.editor_focused = false;
    }

    fn queue_document_parse(&mut self, now: f64) {
        if self.text == self.document.source() {
            return;
        }
        self.document_revision = self.document_revision.wrapping_add(1);
        self.parse_pending = true;
        self.last_edit_time = now;
        self.refresh_status();
        self.parse_worker.submit(ParseRequest {
            tab_id: self.id,
            revision: self.document_revision,
            text: self.text.clone(),
            previous: Some(self.document.clone()),
        });
    }

    fn poll_document_parse_results(&mut self, ctx: &egui::Context) {
        let mut active_document_changed = false;
        let mut any_result = false;
        while let Ok(result) = self.parse_worker.try_recv() {
            any_result = true;
            let Some(index) = self.tabs.iter().position(|tab| tab.id == result.tab_id) else {
                continue;
            };
            let tab = &mut self.tabs[index];
            if result.revision != tab.document_revision || tab.text != result.document.source() {
                // A newer edit is already pending. Keep the gate closed until its
                // matching result arrives.
                continue;
            }
            tab.document = result.document;
            tab.parse_pending = false;
            if index == self.active_tab {
                self.browser_document_cache = None;
                self.render_pending = None;
            }
            active_document_changed |= index == self.active_tab;
        }
        if active_document_changed {
            self.refresh_status();
        }
        if any_result {
            ctx.request_repaint();
        }
    }

    fn switch_tab(&mut self, index: usize) {
        if self.pending_close.is_some() || index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        self.activate_tab(index);
    }

    fn push_tab(&mut self, tab: DocumentTab) {
        let path = tab.path.clone();
        self.tabs.push(tab);
        if let Some(path) = path {
            self.watch_external_path(&path);
        }
        self.activate_tab(self.tabs.len() - 1);
    }

    fn watch_external_path(&mut self, path: &Path) {
        if let Some(watcher) = &mut self.external_watcher {
            watcher.watch(path);
        }
    }

    fn unwatch_external_path(&mut self, path: &Path) {
        if let Some(watcher) = &mut self.external_watcher {
            watcher.unwatch(path);
        }
    }

    fn new_tab(&mut self) {
        if self.workspace_empty {
            self.workspace_empty = false;
            self.view_mode = ViewMode::Write;
            self.editor_focused = true;
            return;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.push_tab(DocumentTab::blank(id));
    }

    fn has_open_document(&self) -> bool {
        !self.workspace_empty
    }

    fn visible_tab_count(&self) -> usize {
        if self.workspace_empty {
            0
        } else {
            self.tabs.len()
        }
    }

    fn open_search(&mut self) {
        if !self.has_open_document() {
            return;
        }
        self.search_open = true;
        self.search_focus_requested = true;
        self.refresh_search();
    }

    fn close_search(&mut self) {
        self.search_open = false;
        self.search_focus_requested = false;
        self.search_scroll_requested = false;
    }

    fn refresh_search(&mut self) {
        if !self.has_open_document() {
            self.search_results = search::SearchResults::default();
            self.search_tab_id = None;
            return;
        }
        self.search_results = search::SearchResults::new(&self.text, &self.search_query);
        self.search_tab_id = Some(self.id);
        self.search_document_revision = self.document_revision;
        self.search_scroll_requested = self.search_open;
        self.search_backwards = false;
    }

    fn refresh_search_if_needed(&mut self) {
        if self.search_open
            && (self.search_tab_id != Some(self.id)
                || self.search_document_revision != self.document_revision)
        {
            self.refresh_search();
        }
    }

    fn preview_search_has_match(&self) -> bool {
        self.search_query.is_empty()
            || !search::SearchResults::new(
                &markdown::plain_text(self.document.blocks()),
                &self.search_query,
            )
            .ranges()
            .is_empty()
    }

    fn search_next(&mut self) {
        if self.search_results.next().is_some() {
            self.search_backwards = false;
            self.search_scroll_requested = true;
        }
    }

    fn search_previous(&mut self) {
        if self.search_results.previous().is_some() {
            self.search_backwards = true;
            self.search_scroll_requested = true;
        }
    }

    fn is_active_dirty(&self) -> bool {
        document_is_dirty(
            self.path.as_ref(),
            &self.text,
            &self.disk_snapshot,
            &self.status,
        )
    }

    fn is_tab_dirty(&self, index: usize) -> bool {
        let tab = &self.tabs[index];
        document_is_dirty(
            tab.path.as_ref(),
            &tab.text,
            &tab.disk_snapshot,
            &tab.status,
        )
    }

    fn tab_title(&self, index: usize) -> String {
        let tab = &self.tabs[index];
        document_label(tab.id, tab.path.as_ref(), false)
    }

    fn request_close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if index != self.active_tab && !self.is_tab_dirty(index) {
            self.close_tab_now(index);
            return;
        }
        if index != self.active_tab {
            self.switch_tab(index);
        }
        if self.is_active_dirty() {
            self.pending_close = Some(self.active_tab);
        } else {
            self.close_tab_now(self.active_tab);
        }
    }

    fn prepare_window_close(&mut self) -> window_close::CloseAction {
        let unsaved_documents = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(index, _)| self.is_tab_dirty(*index))
            .map(|(index, tab)| window_close::UnsavedDocument {
                tab_id: window_close::TabId::from(tab.id),
                title: self.tab_title(index),
            })
            .collect();
        self.window_close_guard.request_close(unsaved_documents)
    }

    fn save_all_for_window_close(&mut self) -> Result<(), window_close::TabId> {
        let tab_ids = self
            .window_close_guard
            .unsaved_documents()
            .iter()
            .map(|document| document.tab_id)
            .collect::<Vec<_>>();
        for tab_id in tab_ids {
            let tab_id_value = u64::from(tab_id);
            let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id_value) else {
                return Err(tab_id);
            };
            self.activate_tab(index);
            self.save();
            if self.is_tab_dirty(index) {
                return Err(tab_id);
            }
        }
        Ok(())
    }

    fn handle_window_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        // Persist the last visible appearance as a final guard for window-manager close
        // requests. The toggle writes eagerly, while this covers an immediate shutdown.
        if let Err(error) = theme::save_dark_mode(self.dark) {
            self.status_note = format!("外观偏好保存失败：{error}");
        }
        match self.prepare_window_close() {
            window_close::CloseAction::Allow => {
                if self.recovery.is_none() {
                    io::clear_draft_for_window(self.draft_window_id);
                }
            }
            window_close::CloseAction::Confirm | window_close::CloseAction::KeepOpen => {
                self.pending_close = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
    }

    fn close_tab_now(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let old_active = self.active_tab;
        let removed_path = self.tabs[index].path.clone();
        self.tabs.remove(index);
        if let Some(path) = removed_path {
            self.unwatch_external_path(&path);
        }
        self.pending_close = None;
        if self.tabs.is_empty() {
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            self.tabs.push(DocumentTab::blank(id));
            self.active_tab = 0;
            self.editor_focused = false;
            self.workspace_empty = true;
            self.focus_mode = false;
            self.pending_preview_restore = None;
            self.close_search();
        } else {
            let new_active = if index < old_active {
                old_active - 1
            } else if index == old_active {
                index.min(self.tabs.len() - 1)
            } else {
                old_active
            };
            self.activate_tab(new_active);
        }
        let _ = self.persist_draft_session();
    }

    fn finish_pending_close_if_saved(&mut self) {
        if matches!(self.status, DocStatus::Saved)
            && let Some(index) = self.pending_close
            && index == self.active_tab
        {
            self.close_tab_now(index);
        }
    }

    fn theme_spec(&self) -> ThemeSpec {
        self.theme_package
            .as_ref()
            .and_then(|t| t.spec(self.dark).ok())
            .or_else(|| ThemePackage::built_in_sspai().spec(self.dark).ok())
            .unwrap_or_else(|| ThemeSpec::fallback(self.dark))
    }

    fn apply_current_theme(&self, ctx: &egui::Context) {
        apply_visuals(ctx, self.dark, &self.theme_spec());
    }

    fn toggle_dark_mode(&mut self, ctx: &egui::Context) {
        self.dark = !self.dark;
        self.theme_revision = self.theme_revision.wrapping_add(1);
        self.apply_current_theme(ctx);
        if let Err(error) = theme::save_dark_mode(self.dark) {
            self.status_note = format!("外观偏好保存失败：{error}");
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn current_browser_document_key(&self) -> BrowserDocumentKey {
        let base_directory = self
            .path
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(Path::to_path_buf);
        BrowserDocumentKey {
            tab_id: self.tabs.get(self.active_tab).map_or(0, |tab| tab.id),
            document_revision: self.document_revision,
            document_source_hash: source_hash(&self.text),
            theme_revision: self.theme_revision,
            body_font_size_bits: self.body_font_size.to_bits(),
            base_directory,
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn make_render_request(
        &self,
        key: BrowserDocumentKey,
        previous: Option<Arc<web_preview::PreviewDocument>>,
        previous_parsed: Option<Arc<ParsedDocument>>,
    ) -> RenderRequest {
        let built_in = ThemePackage::built_in_sspai();
        let package = self.theme_package.as_ref().unwrap_or(&built_in);
        let base_css = package
            .browser_css()
            .unwrap_or(theme::BUILT_IN_SSPAI_CSS)
            .to_string();
        let dark_mode_css = self.dark.then(|| theme::dark_mode_css(&self.theme_spec()));
        let default_size = package.recommended_body_font_size();
        let font_size_override =
            ((self.body_font_size - default_size).abs() > 0.01).then_some(self.body_font_size);
        RenderRequest {
            key,
            document: self.document.clone(),
            previous,
            previous_parsed,
            css: base_css,
            base_directory: self
                .path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            font_size_override,
            dark_mode_css,
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn browser_base_font_size(&self) -> f32 {
        let built_in = ThemePackage::built_in_sspai();
        self.theme_package
            .as_ref()
            .unwrap_or(&built_in)
            .recommended_body_font_size()
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn previous_browser_render_documents(
        &self,
        key: &BrowserDocumentKey,
    ) -> (
        Option<Arc<web_preview::PreviewDocument>>,
        Option<Arc<ParsedDocument>>,
    ) {
        self.browser_document_cache
            .as_ref()
            // Incremental preview functions deliberately reuse the previous HTML shell.
            // That shell contains the theme CSS, so it is only safe when the render
            // context is unchanged.  In particular, a light/dark toggle increments
            // `theme_revision`; carrying the old document across that boundary would
            // silently put the previous appearance back into the WebView.
            .filter(|cache| cache.key.same_render_context(key))
            .map(|cache| {
                (
                    Some(Arc::clone(&cache.document)),
                    Some(Arc::clone(&cache.parsed_document)),
                )
            })
            .unwrap_or((None, None))
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn queue_browser_render(&mut self, key: BrowserDocumentKey) {
        // Parsing runs on a background worker.  While it is pending `self.text`
        // already belongs to the new revision, but `self.document` is still the
        // previous parsed tree.  Never submit that tree under the new key: the
        // worker result would otherwise look current to the UI and briefly
        // replace the preview with stale content.
        if key != self.current_browser_document_key() || self.document.source() != self.text {
            return;
        }
        if self.render_pending.as_ref() == Some(&key) {
            return;
        }
        let (previous, previous_parsed) = self.previous_browser_render_documents(&key);
        self.render_worker
            .submit(self.make_render_request(key.clone(), previous, previous_parsed));
        self.render_pending = Some(key);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn poll_browser_render_results(&mut self, ctx: &egui::Context) {
        let current_key = self.current_browser_document_key();
        let mut changed = false;
        while let Ok(result) = self.render_worker.try_recv() {
            // The key protects the tab/revision, while this source check also
            // protects against a request that captured an older ParsedDocument
            // before parsing caught up with the current text.
            if render_result_matches_current(&result, &current_key, &self.text) {
                self.render_metrics.record(result.metrics);
                self.browser_document_cache = Some(BrowserDocumentCache {
                    key: result.key.clone(),
                    document: result.document,
                    parsed_document: result.parsed_document,
                });
                if self.render_pending.as_ref() == Some(&result.key) {
                    self.render_pending = None;
                }
                changed = true;
            }
        }
        if changed {
            ctx.request_repaint();
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn browser_document(
        &mut self,
        defer_document_refresh: bool,
    ) -> Arc<web_preview::PreviewDocument> {
        let key = self.current_browser_document_key();
        if let Some(cache) = &self.browser_document_cache
            && cache.key == key
        {
            return Arc::clone(&cache.document);
        }
        if defer_document_refresh
            && let Some(cache) = &self.browser_document_cache
            && cache.key.same_render_context(&key)
        {
            return Arc::clone(&cache.document);
        }

        let document_ready = self.text == self.document.source();
        if let Some(cache) = &self.browser_document_cache
            && cache.key.same_render_context(&key)
        {
            let document = Arc::clone(&cache.document);
            let source_matches = cache.key.document_revision == key.document_revision
                && cache.key.document_source_hash == key.document_source_hash;
            if document_ready && !source_matches {
                self.queue_browser_render(key.clone());
            }
            if source_matches && let Some(cache) = self.browser_document_cache.as_mut() {
                // Keep the cached HTML while applying font-size through the
                // WebView CSS variables; this avoids a full navigation.
                cache.key = key;
            }
            return document;
        }

        let request = self.make_render_request(key.clone(), None, None);
        let document = Arc::new(web_preview::preview_document_placeholder(
            &request.css,
            request.font_size_override,
            request.dark_mode_css.as_deref(),
        ));
        self.queue_browser_render(key.clone());
        self.browser_document_cache = Some(BrowserDocumentCache {
            key,
            document: Arc::clone(&document),
            parsed_document: Arc::new(self.document.clone()),
        });
        document
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn release_browser_preview(&mut self) {
        self.browser_preview.close();
        self.browser_document_cache = None;
        self.render_pending = None;
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn finish_benchmark_probe(&mut self, ready: web_preview::WebViewReady) {
        let source_bytes = self.text.len();
        let block_count = self.document.blocks().len();
        let event_count = self.document.events().len();
        let font_requests = self.browser_preview.font_asset_request_counts();
        let Some(probe) = &mut self.benchmark_probe else {
            return;
        };
        if probe.completed {
            return;
        }
        let report = serde_json::json!({
            "schema_version": 1,
            "pid": std::process::id(),
            "source_bytes": source_bytes,
            "blocks": block_count,
            "events": event_count,
            "startup_to_webview_ready_ms": probe.started.elapsed().as_secs_f64() * 1000.0,
            "content_height_css_px": ready.content_height,
            "viewport_height_css_px": ready.viewport_height,
            "dom_element_count": ready.element_count,
            "local_image_requests_before_ready": self.browser_preview.local_image_request_count(),
            "mermaid_runtime_requests_before_ready": self.browser_preview.mermaid_runtime_request_count(),
            "font_asset_requests_before_ready": {
                "jetbrains_regular": font_requests[0],
                "jetbrains_bold": font_requests[1],
                "lxgw_regular": font_requests[2],
                "lxgw_medium": font_requests[3],
            },
            "font_asset_bytes_before_ready": self.browser_preview.font_asset_requested_bytes(),
            "render_metrics": self.render_metrics.snapshot_json(),
            "error": ready.error,
        });
        let result = (|| -> Result<(), String> {
            if let Some(parent) = probe.report_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let json = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
            std::fs::write(&probe.report_path, json).map_err(|error| error.to_string())
        })();
        probe.completed = result.is_ok();
        self.status_note = match result {
            Ok(()) => format!("WebView 基准完成：{}", probe.report_path.display()),
            Err(error) => format!("WebView 基准写入失败：{error}"),
        };
    }

    fn import_theme(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Theme Package", &["json", "css", "zip"])
            .pick_file()
        else {
            return;
        };
        let result = ThemePackage::from_file(&path).and_then(|package| {
            theme::save_imported(&package)?;
            Ok(package)
        });
        match result {
            Ok(package) => {
                let name = package.name.clone();
                self.body_font_size = package.recommended_body_font_size();
                self.theme_package = Some(package);
                self.theme_revision = self.theme_revision.wrapping_add(1);
                self.apply_current_theme(ctx);
                self.status_note = format!("已加载主题：{name}");
            }
            Err(e) => self.status = DocStatus::SaveFailed(e),
        }
    }

    fn remove_theme(&mut self, ctx: &egui::Context) {
        self.theme_package = None;
        self.theme_revision = self.theme_revision.wrapping_add(1);
        self.body_font_size = ThemePackage::built_in_sspai().recommended_body_font_size();
        theme::clear_saved();
        self.apply_current_theme(ctx);
        self.status_note = "已移除外部主题，恢复少数派经典".to_string();
    }

    fn refresh_status(&mut self) {
        self.status = match &self.path {
            Some(_) => {
                if self.disk_snapshot.as_slice() == self.text.as_bytes() {
                    DocStatus::Saved
                } else {
                    DocStatus::Modified
                }
            }
            None => {
                if self.text.is_empty() {
                    DocStatus::Unsaved
                } else {
                    DocStatus::Modified
                }
            }
        };
    }

    fn probe_external_change(
        &mut self,
        path: &Path,
        snapshot: &[u8],
        now: f64,
        force_read: bool,
    ) -> ExternalProbe {
        let stamp = match io::file_stamp(path) {
            Ok(stamp) => stamp,
            Err(error) => {
                self.observed_file_stamps.remove(path);
                self.pending_external_changes.remove(path);
                return ExternalProbe::Missing(describe_read_error(&error));
            }
        };

        let stamp_changed = self.observed_file_stamps.get(path) != Some(&stamp);
        if stamp_changed || force_read {
            self.observed_file_stamps
                .insert(path.to_path_buf(), stamp.clone());
            match io::read_snapshot_checked(path) {
                Ok(bytes) if bytes.as_slice() == snapshot => {
                    self.pending_external_changes.remove(path);
                }
                Ok(bytes) => {
                    let changed = self
                        .pending_external_changes
                        .get(path)
                        .is_none_or(|pending| pending.stamp != stamp || pending.bytes != bytes);
                    if changed {
                        self.pending_external_changes.insert(
                            path.to_path_buf(),
                            PendingExternalChange {
                                stamp,
                                bytes,
                                first_seen: now,
                            },
                        );
                    }
                }
                Err(error) => return ExternalProbe::Missing(describe_read_error(&error)),
            }
            return ExternalProbe::Waiting;
        }

        let is_stable = self
            .pending_external_changes
            .get(path)
            .is_some_and(|pending| {
                pending.stamp == stamp && now - pending.first_seen >= EXTERNAL_STABLE_DELAY
            });
        if is_stable && let Some(pending) = self.pending_external_changes.remove(path) {
            return ExternalProbe::Stable(pending.bytes);
        }
        ExternalProbe::Waiting
    }

    fn poll_external_changes(&mut self, ctx: &egui::Context, now: f64) {
        if !self.auto_reload_external {
            return;
        }
        ctx.request_repaint_after(Duration::from_secs_f64(EXTERNAL_POLL_INTERVAL));
        let changed_paths = self
            .external_watcher
            .as_ref()
            .map(ExternalFileWatcher::drain_changed_paths)
            .unwrap_or_default();
        if now - self.last_external_poll < EXTERNAL_POLL_INTERVAL && changed_paths.is_empty() {
            return;
        }
        self.last_external_poll = now;
        let mut draft_state_changed = false;

        let active_path = self.path.clone();
        if let Some(path) = active_path {
            let snapshot = self.disk_snapshot.clone();
            match self.probe_external_change(&path, &snapshot, now, changed_paths.contains(&path)) {
                ExternalProbe::Stable(bytes) => {
                    let active_index = self.active_tab;
                    match apply_external_bytes(&mut self.tabs[active_index], bytes) {
                        Ok(ExternalChangeResult::Unchanged) => {}
                        Ok(_) => draft_state_changed = true,
                        Err(error) => {
                            self.status_note = format!(
                                "检测到外部修改，但无法加载：{}",
                                describe_read_error(&error)
                            );
                        }
                    }
                }
                ExternalProbe::Missing(error) => {
                    let note = format!("无法监视磁盘文件：{error}");
                    if self.status_note != note {
                        self.status_note = note;
                    }
                }
                ExternalProbe::Waiting => {}
            }
        }

        for index in 0..self.tabs.len() {
            if index == self.active_tab {
                continue;
            }
            let Some(path) = self.tabs[index].path.clone() else {
                continue;
            };
            let snapshot = self.tabs[index].disk_snapshot.clone();
            match self.probe_external_change(&path, &snapshot, now, changed_paths.contains(&path)) {
                ExternalProbe::Stable(bytes) => {
                    match apply_external_bytes(&mut self.tabs[index], bytes) {
                        Ok(ExternalChangeResult::Unchanged) => {}
                        Ok(_) => draft_state_changed = true,
                        Err(error) => {
                            self.tabs[index].status_note = format!(
                                "检测到外部修改，但无法加载：{}",
                                describe_read_error(&error)
                            );
                        }
                    }
                }
                ExternalProbe::Missing(error) => {
                    self.tabs[index].status_note = format!("无法监视磁盘文件：{error}");
                }
                ExternalProbe::Waiting => {}
            }
        }
        if draft_state_changed {
            let _ = self.persist_draft_session();
        }
    }

    fn draft_session(&self) -> Option<io::DraftSession> {
        let drafts = self
            .tabs
            .iter()
            .filter(|tab| {
                document_is_dirty(
                    tab.path.as_ref(),
                    &tab.text,
                    &tab.disk_snapshot,
                    &tab.status,
                ) && (!tab.text.is_empty() || tab.path.is_some())
            })
            .map(|tab| {
                io::DraftTab::new(
                    tab.id,
                    tab.path.clone(),
                    tab.text.clone(),
                    &tab.disk_snapshot,
                )
            })
            .collect::<Vec<_>>();
        if drafts.is_empty() {
            return None;
        }
        let current_id = self.tabs[self.active_tab].id;
        let active_tab_id = if drafts.iter().any(|draft| draft.id == current_id) {
            current_id
        } else {
            drafts[0].id
        };
        Some(io::DraftSession::new(active_tab_id, drafts))
    }

    fn window_session(&self) -> Option<window_session::WindowSession> {
        let paths = self
            .tabs
            .iter()
            .filter_map(|tab| tab.path.clone())
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return None;
        }
        let active_path = self.tabs[self.active_tab].path.clone();
        Some(window_session::WindowSession::new(paths, active_path))
    }

    fn persist_window_session_if_changed(&mut self) {
        // Explicit secondary windows use a process-scoped draft. They are intentionally
        // excluded from the single "last main window" restored on ordinary startup.
        if self.draft_window_id.is_some() {
            return;
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if self.benchmark_probe.is_some() {
            return;
        }

        let current = self.window_session();
        if self.window_session_initialized && current == self.persisted_window_session {
            return;
        }
        let result = match current.as_ref() {
            Some(session) => window_session::save(None, session),
            None => {
                window_session::clear(None);
                Ok(())
            }
        };
        match result {
            Ok(()) => {
                self.persisted_window_session = current;
                self.window_session_initialized = true;
            }
            Err(error) => self.status_note = format!("窗口会话保存失败：{error}"),
        }
    }

    fn persist_draft_session(&self) -> std::io::Result<()> {
        if let Some(session) = self.draft_session() {
            io::save_draft_for_window(self.draft_window_id, &session)
        } else {
            io::clear_draft_for_window(self.draft_window_id);
            Ok(())
        }
    }

    fn autosave_draft(&mut self, now: f64) {
        let dirty_indices = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                (document_is_dirty(
                    tab.path.as_ref(),
                    &tab.text,
                    &tab.disk_snapshot,
                    &tab.status,
                ) && (!tab.text.is_empty() || tab.path.is_some()))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if dirty_indices.is_empty() {
            return;
        }
        let all_idle = dirty_indices.iter().all(|&index| {
            let edited = self.tabs[index].last_edit_time;
            edited == f64::NEG_INFINITY || (edited.is_finite() && now - edited > 30.0)
        });
        let write_due = dirty_indices
            .iter()
            .any(|&index| now - self.tabs[index].draft_last_write > 30.0);
        if all_idle
            && write_due
            && let Some(session) = self.draft_session()
        {
            match io::save_draft_for_window(self.draft_window_id, &session) {
                Ok(()) => {
                    for index in dirty_indices {
                        self.tabs[index].draft_last_write = now;
                    }
                }
                Err(error) => {
                    self.status_note = format!("草稿会话保存失败：{error}");
                }
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let has_open_document = self.has_open_document();
        let new_window = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::N,
        );
        let new_tab = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N);
        let open = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
        let find = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::F);
        let save = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S);
        let save_as = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::S,
        );
        let close_tab = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);
        if ctx.input_mut(|i| i.consume_shortcut(&new_window)) {
            self.open_new_window();
        } else if ctx.input_mut(|i| i.consume_shortcut(&new_tab)) {
            self.new_tab();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&open)) {
            self.open_file();
        }
        if has_open_document && ctx.input_mut(|i| i.consume_shortcut(&find)) {
            self.open_search();
        }
        if self.search_open
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.close_search();
        }
        if has_open_document && ctx.input_mut(|i| i.consume_shortcut(&save)) {
            self.save();
        }
        if has_open_document && ctx.input_mut(|i| i.consume_shortcut(&save_as)) {
            self.save_as();
        }
        if has_open_document && ctx.input_mut(|i| i.consume_shortcut(&close_tab)) {
            self.request_close_tab(self.active_tab);
        }
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Tab,
            )
        }) && self.tabs.len() > 1
        {
            let previous = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
            self.switch_tab(previous);
        } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Tab))
            && self.tabs.len() > 1
        {
            self.switch_tab((self.active_tab + 1) % self.tabs.len());
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Slash)) {
            self.view_mode = if self.view_mode == ViewMode::Preview {
                ViewMode::Write
            } else {
                ViewMode::Preview
            };
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F8)) {
            self.focus_mode = !self.focus_mode;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num1)) {
            self.view_mode = ViewMode::Write;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num2)) {
            self.view_mode = ViewMode::Preview;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num3)) {
            self.view_mode = ViewMode::Split;
        }
    }

    fn open_new_window(&mut self) {
        let result = std::env::current_exe().and_then(|executable| {
            Command::new(executable)
                .arg("--new-window")
                .spawn()
                .map(|_| ())
        });
        self.status_note = match result {
            Ok(()) => "已打开新窗口".to_string(),
            Err(error) => format!("无法打开新窗口：{error}"),
        };
    }

    fn open_file(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("Markdown", &["md", "markdown", "txt"])
            .pick_files()
        else {
            return;
        };
        for path in paths {
            self.open_path(&path);
        }
    }

    fn open_dropped_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }

        let mut opened = 0usize;
        let mut ignored = 0usize;
        for path in paths {
            if path.is_file() && has_supported_text_extension(&path) {
                self.open_path(&path);
                opened += 1;
            } else {
                ignored += 1;
            }
        }

        self.status_note = match (opened, ignored) {
            (0, _) => "未找到可打开的 Markdown 或文本文件".to_string(),
            (opened, 0) => format!("已打开 {opened} 个文件"),
            (opened, ignored) => {
                format!("已打开 {opened} 个文件，忽略 {ignored} 个不支持的项目")
            }
        };
    }

    fn apply_instance_request(&mut self, request: single_instance::OpenRequest) -> bool {
        self.open_dropped_paths(request.paths);
        request.focus_window
    }

    fn handle_instance_requests(&mut self, ctx: &egui::Context) {
        let requests = self
            .instance_requests
            .as_ref()
            .map(|receiver| receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        if requests.is_empty() {
            return;
        }
        for request in requests {
            if self.apply_instance_request(request) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
    }

    fn open_path(&mut self, path: &PathBuf) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.path.as_ref() == Some(path))
        {
            self.activate_tab(index);
            self.status_note = format!("已切换到 {}", path.display());
            return;
        }
        match io::read_markdown(path) {
            Ok(text) => {
                let snapshot = io::read_snapshot(path).unwrap_or_default();
                let replace_blank = self.tabs.len() == 1
                    && self.tabs[0].path.is_none()
                    && self.tabs[0].text.is_empty()
                    && self.tabs[0].disk_snapshot.is_empty()
                    && matches!(self.tabs[0].status, DocStatus::Unsaved);
                if replace_blank {
                    let id = self.tabs[0].id;
                    self.tabs[0] = DocumentTab::from_file(id, path.clone(), text, snapshot);
                    self.watch_external_path(path);
                    self.activate_tab(0);
                } else {
                    let id = self.next_tab_id;
                    self.next_tab_id += 1;
                    self.push_tab(DocumentTab::from_file(id, path.clone(), text, snapshot));
                }
                self.workspace_empty = false;
                self.status_note = format!("已打开 {}", path.display());
            }
            Err(e) => {
                self.status =
                    DocStatus::SaveFailed(format!("无法读取文件：{}", describe_read_error(&e)));
            }
        }
    }

    fn save(&mut self) {
        if self.path.is_none() {
            self.save_as();
            return;
        }
        let path = self.path.clone().expect("已检查文档路径");
        match io::save_with_conflict_check(&path, &self.text, &self.disk_snapshot) {
            Ok(bytes) => {
                self.disk_snapshot = bytes;
                self.path = Some(path);
                self.status = DocStatus::Saved;
                self.status_note = format!("已保存 {}", clock_time());
                if let Err(error) = self.persist_draft_session() {
                    self.status_note = format!("文档已保存；草稿会话更新失败：{error}");
                }
            }
            Err(io::SaveError::ExternalModified) => {
                self.conflict = Some(path);
                self.status = DocStatus::Conflict;
            }
            Err(io::SaveError::Io(e)) => {
                self.status = DocStatus::SaveFailed(format!("保存失败：{}", e));
            }
        }
    }

    fn save_as(&mut self) -> bool {
        let Some(path) = pick_save_path() else {
            return false;
        };
        match io::save_overwrite(&path, &self.text) {
            Ok(bytes) => {
                self.disk_snapshot = bytes;
                self.path = Some(path.clone());
                self.watch_external_path(&path);
                self.status = DocStatus::Saved;
                self.conflict = None;
                self.status_note = format!("已保存 {}", clock_time());
                if let Err(error) = self.persist_draft_session() {
                    self.status_note = format!("文档已保存；草稿会话更新失败：{error}");
                }
                true
            }
            Err(e) => {
                self.status = DocStatus::SaveFailed(format!("保存失败：{}", e));
                false
            }
        }
    }

    fn resolve_overwrite(&mut self) {
        if let Some(path) = self.conflict.take() {
            match io::save_overwrite(&path, &self.text) {
                Ok(bytes) => {
                    self.disk_snapshot = bytes;
                    self.path = Some(path);
                    self.status = DocStatus::Saved;
                    self.status_note = "已覆盖保存".to_string();
                    if let Err(error) = self.persist_draft_session() {
                        self.status_note = format!("文档已保存；草稿会话更新失败：{error}");
                    }
                    self.finish_pending_close_if_saved();
                }
                Err(e) => self.status = DocStatus::SaveFailed(format!("保存失败：{}", e)),
            }
        }
    }

    fn resolve_save_as(&mut self) {
        if self.save_as() {
            self.conflict = None;
            self.finish_pending_close_if_saved();
        }
    }

    fn resolve_reload(&mut self) {
        if let Some(path) = self.conflict.take() {
            match io::read_markdown(&path) {
                Ok(text) => {
                    self.text = text;
                    self.path = Some(path.clone());
                    self.disk_snapshot = io::read_snapshot(&path).unwrap_or_default();
                    self.document = markdown::parse_document(&self.text);
                    self.document_revision = self.document_revision.wrapping_add(1);
                    self.parse_pending = false;
                    self.browser_document_cache = None;
                    self.render_pending = None;
                    self.status = DocStatus::Saved;
                    self.status_note = "已重新载入磁盘内容".to_string();
                    if let Err(error) = self.persist_draft_session() {
                        self.status_note = format!("磁盘内容已载入；草稿会话更新失败：{error}");
                    }
                    self.finish_pending_close_if_saved();
                }
                Err(e) => {
                    self.status =
                        DocStatus::SaveFailed(format!("无法读取文件：{}", describe_read_error(&e)));
                }
            }
        }
    }

    fn export_html(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("HTML", &["html"])
            .set_file_name("导出.html")
            .save_file()
        else {
            return;
        };
        let title = self.export_title();
        let options = self.export_options(&title);
        match export::export_html(&path, &self.document, options) {
            Ok(()) => self.status_note = format!("已导出 HTML：{}", path.display()),
            Err(e) => self.status = DocStatus::SaveFailed(format!("导出失败：{}", e)),
        }
    }

    fn export_pdf(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .set_file_name("导出.pdf")
            .save_file()
        else {
            return;
        };
        let title = self.export_title();
        let options = self.export_options(&title);
        match export::export_pdf(&path, &self.document, options) {
            Ok(()) => self.status_note = format!("已导出 PDF：{}", path.display()),
            Err(e) => self.status = DocStatus::SaveFailed(format!("导出失败：{}", e)),
        }
    }

    fn export_title(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_stem)
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("未命名文档")
            .to_string()
    }

    fn export_options<'a>(&'a self, title: &'a str) -> export::ExportOptions<'a> {
        let package = self.theme_package.as_ref();
        let theme_css = package
            .and_then(ThemePackage::browser_css)
            .unwrap_or(theme::BUILT_IN_SSPAI_CSS);
        let default_size = package
            .map(ThemePackage::recommended_body_font_size)
            .unwrap_or_else(|| ThemePackage::built_in_sspai().recommended_body_font_size());
        export::ExportOptions {
            title,
            theme_css,
            dark_mode: self.dark,
            theme_spec: self.theme_spec(),
            base_directory: self.path.as_deref().and_then(Path::parent),
            body_font_size: ((self.body_font_size - default_size).abs() > 0.01)
                .then_some(self.body_font_size),
        }
    }

    #[allow(dead_code)]
    fn menu_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("文件", |ui| {
                if ui
                    .button(format!("打开…    {PRIMARY_SHORTCUT}+O"))
                    .clicked()
                {
                    ui.close();
                    self.open_file();
                }
                if ui.button(format!("保存    {PRIMARY_SHORTCUT}+S")).clicked() {
                    ui.close();
                    self.save();
                }
                if ui
                    .button(format!("另存为…    {PRIMARY_SHORTCUT}+Shift+S"))
                    .clicked()
                {
                    ui.close();
                    self.save_as();
                }
                ui.separator();
                if ui.button("导出 HTML…").clicked() {
                    ui.close();
                    self.export_html();
                }
                if ui.button("导出 PDF…").clicked() {
                    ui.close();
                    self.export_pdf();
                }
                ui.separator();
                if ui.button("退出").clicked() {
                    ui.close();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("编辑", |ui| {
                if ui.button("复制渲染内容").clicked() {
                    ctx.copy_text(markdown::plain_text(self.document.blocks()));
                    self.status_note = "已复制渲染内容".to_string();
                    ui.close();
                }
                if ui.button("复制 HTML").clicked() {
                    ctx.copy_text(export::render_html(&self.document));
                    self.status_note = "已复制 HTML".to_string();
                    ui.close();
                }
            });
            ui.menu_button("视图", |ui| {
                ui.selectable_value(
                    &mut self.view_mode,
                    ViewMode::Write,
                    format!("写作模式     {PRIMARY_SHORTCUT}+1"),
                );
                ui.selectable_value(
                    &mut self.view_mode,
                    ViewMode::Preview,
                    format!("阅读模式     {PRIMARY_SHORTCUT}+2"),
                );
                ui.selectable_value(
                    &mut self.view_mode,
                    ViewMode::Split,
                    format!("分栏模式     {PRIMARY_SHORTCUT}+3"),
                );
                ui.separator();
                ui.label(egui::RichText::new("编辑与正文字号").weak().size(12.0));
                ui.add(
                    egui::Slider::new(&mut self.body_font_size, 12.0..=22.0)
                        .step_by(0.5)
                        .suffix(" px"),
                );
                if ui.small_button("恢复默认 15.5 px").clicked() {
                    self.body_font_size = 15.5;
                }
                ui.separator();
                ui.checkbox(&mut self.show_status, "显示状态栏");
                let theme = if self.dark {
                    "浅色外观"
                } else {
                    "深色外观"
                };
                if ui.button(theme).clicked() {
                    self.toggle_dark_mode(ui.ctx());
                    ui.close();
                }
            });
            ui.menu_button("视图", |ui| {
                ui.selectable_value(
                    &mut self.view_mode,
                    ViewMode::Write,
                    format!("写作模式    {PRIMARY_SHORTCUT}+1"),
                );
                ui.selectable_value(
                    &mut self.view_mode,
                    ViewMode::Preview,
                    format!("阅读模式    {PRIMARY_SHORTCUT}+2"),
                );
                ui.selectable_value(
                    &mut self.view_mode,
                    ViewMode::Split,
                    format!("分栏模式    {PRIMARY_SHORTCUT}+3"),
                );
                ui.separator();
                ui.checkbox(&mut self.focus_mode, "专注模式    F8");
                ui.checkbox(&mut self.show_status, "显示状态栏");
                ui.separator();
                let label = if self.dark {
                    "切换到亮色主题"
                } else {
                    "切换到暗色主题"
                };
                if ui.button(label).clicked() {
                    self.toggle_dark_mode(ctx);
                    ui.close();
                }
            });
        });
    }

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let has_open_document = self.has_open_document();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), CHROME_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().button_padding = egui::vec2(7.0, 4.0);
                ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
                ui.visuals_mut().widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                ui.menu_button(egui::RichText::new("文件").size(CHROME_FONT_SIZE), |ui| {
                    if ui
                        .button(format!("新建窗口   {PRIMARY_SHORTCUT}+Shift+N"))
                        .clicked()
                    {
                        ui.close();
                        self.open_new_window();
                    }
                    if ui
                        .button(format!("新建标签   {PRIMARY_SHORTCUT}+N"))
                        .clicked()
                    {
                        ui.close();
                        self.new_tab();
                    }
                    if ui
                        .button(format!("打开…     {PRIMARY_SHORTCUT}+O"))
                        .clicked()
                    {
                        ui.close();
                        self.open_file();
                    }
                    if ui
                        .add_enabled(
                            has_open_document,
                            egui::Button::new(format!("保存       {PRIMARY_SHORTCUT}+S")),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.save();
                    }
                    if ui
                        .add_enabled(
                            has_open_document,
                            egui::Button::new(format!("另存为…   {PRIMARY_SHORTCUT}+Shift+S")),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.save_as();
                    }
                    if ui
                        .add_enabled(
                            has_open_document,
                            egui::Button::new(format!("关闭标签   {PRIMARY_SHORTCUT}+W")),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.request_close_tab(self.active_tab);
                    }
                    ui.separator();
                    if ui
                        .add_enabled(has_open_document, egui::Button::new("导出 HTML…"))
                        .clicked()
                    {
                        ui.close();
                        self.export_html();
                    }
                    if ui
                        .add_enabled(has_open_document, egui::Button::new("导出 PDF…"))
                        .clicked()
                    {
                        ui.close();
                        self.export_pdf();
                    }
                    #[cfg(target_os = "windows")]
                    {
                        ui.separator();
                        if ui.button("设为 Markdown 默认应用…").clicked() {
                            ui.close();
                            match file_association::register_and_open_default_apps() {
                                Ok(()) => {
                                    self.status_note =
                                        "已打开系统设置，请确认 .md 与 .markdown 的默认应用"
                                            .to_string();
                                }
                                Err(error) => {
                                    self.status_note = format!("无法打开默认应用设置：{error}");
                                }
                            }
                        }
                    }
                });
                ui.menu_button(egui::RichText::new("编辑").size(CHROME_FONT_SIZE), |ui| {
                    if ui
                        .add_enabled(
                            has_open_document,
                            egui::Button::new(format!("查找…       {PRIMARY_SHORTCUT}+F")),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.open_search();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(has_open_document, egui::Button::new("复制渲染内容"))
                        .clicked()
                    {
                        ui.close();
                        ui.ctx()
                            .copy_text(markdown::plain_text(self.document.blocks()));
                    }
                    if ui
                        .add_enabled(has_open_document, egui::Button::new("复制 HTML"))
                        .clicked()
                    {
                        ui.close();
                        ui.ctx().copy_text(export::render_html(&self.document));
                    }
                });
                ui.menu_button(egui::RichText::new("视图").size(CHROME_FONT_SIZE), |ui| {
                    ui.set_min_width(230.0);
                    ui.label(egui::RichText::new("文档主题").weak().size(12.0));
                    if let Some(package) = &self.theme_package {
                        let author = if package.author.trim().is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", package.author)
                        };
                        ui.label(
                            egui::RichText::new(format!("{}{}", package.name, author))
                                .strong()
                                .size(13.0),
                        );
                    } else {
                        ui.label(egui::RichText::new("少数派经典 · 内置").strong().size(13.0));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("导入主题包…").clicked() {
                            self.import_theme(ui.ctx());
                        }
                        if self.theme_package.is_some() && ui.small_button("移除").clicked() {
                            self.remove_theme(ui.ctx());
                        }
                    });
                    ui.separator();
                    ui.label(egui::RichText::new("编辑与正文字号").weak().size(12.0));
                    ui.horizontal(|ui| {
                        if ui.small_button("−").clicked() {
                            self.body_font_size = (self.body_font_size - 0.5).max(12.0);
                        }
                        ui.add(
                            egui::Slider::new(&mut self.body_font_size, 12.0..=22.0)
                                .step_by(0.5)
                                .show_value(false),
                        );
                        if ui.small_button("+").clicked() {
                            self.body_font_size = (self.body_font_size + 0.5).min(22.0);
                        }
                        ui.label(format!("{:.1}", self.body_font_size));
                    });
                    if ui.small_button("恢复默认字号").clicked() {
                        self.body_font_size = 15.5;
                    }
                    ui.separator();
                    let watch_changed = ui
                        .checkbox(&mut self.auto_reload_external, "自动加载外部修改")
                        .on_hover_text("Agent 或其他程序修改当前 Markdown 后自动刷新")
                        .changed();
                    if watch_changed {
                        self.observed_file_stamps.clear();
                        self.pending_external_changes.clear();
                        self.last_external_poll = f64::NEG_INFINITY;
                    }
                    ui.checkbox(&mut self.show_status, "显示状态栏");
                    let theme = if self.dark {
                        "浅色外观"
                    } else {
                        "深色外观"
                    };
                    if ui.button(theme).clicked() {
                        self.toggle_dark_mode(ui.ctx());
                        ui.close();
                    }
                });
                ui.separator();
                let mut switch_to = None;
                let mut close_tab = None;
                let mut create_tab = false;
                let tabs_width = (ui.available_width() - 235.0).max(150.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(tabs_width, CHROME_CONTROL_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("document_tabs")
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .show(ui, |ui| {
                                ui.horizontal_centered(|ui| {
                                    for index in 0..self.visible_tab_count() {
                                        let id = self.tabs[index].id;
                                        let title = self.tab_title(index);
                                        let dirty = self.is_tab_dirty(index);
                                        ui.push_id(id, |ui| {
                                            let (select_clicked, close_clicked) =
                                                document_tab_button(
                                                    ui,
                                                    id,
                                                    &title,
                                                    dirty,
                                                    index == self.active_tab,
                                                );
                                            if select_clicked {
                                                switch_to = Some(index);
                                            }
                                            if close_clicked {
                                                close_tab = Some(index);
                                            }
                                        });
                                    }
                                    if chrome_icon_button(ui, "+")
                                        .on_hover_text(format!("新建标签 · {PRIMARY_SHORTCUT}+N"))
                                        .clicked()
                                    {
                                        create_tab = true;
                                    }
                                });
                            });
                    },
                );
                if let Some(index) = close_tab {
                    self.request_close_tab(index);
                } else if let Some(index) = switch_to {
                    self.switch_tab(index);
                } else if create_tab {
                    self.new_tab();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(has_open_document, |ui| {
                        if chrome_nav_button(ui, "专注", has_open_document && self.focus_mode)
                            .on_hover_text("专注模式 · F8")
                            .clicked()
                        {
                            self.focus_mode = !self.focus_mode;
                        }
                        ui.add_space(4.0);
                        if chrome_nav_button(
                            ui,
                            "分栏",
                            has_open_document && self.view_mode == ViewMode::Split,
                        )
                        .clicked()
                        {
                            self.view_mode = ViewMode::Split;
                        }
                        if chrome_nav_button(
                            ui,
                            "阅读",
                            has_open_document && self.view_mode == ViewMode::Preview,
                        )
                        .clicked()
                        {
                            self.view_mode = ViewMode::Preview;
                        }
                        if chrome_nav_button(
                            ui,
                            "写作",
                            has_open_document && self.view_mode == ViewMode::Write,
                        )
                        .clicked()
                        {
                            self.view_mode = ViewMode::Write;
                        }
                    });
                });
            },
        );
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.workspace_empty {
                ui.label(egui::RichText::new("没有打开的文档").weak());
                ui.separator();
                ui.label(egui::RichText::new("可新建、打开或拖入 Markdown 文件").weak());
                if !self.status_note.is_empty() {
                    ui.separator();
                    ui.label(&self.status_note);
                } else if let DocStatus::SaveFailed(message) = &self.status {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(0xc0, 0x39, 0x2b),
                        format!("出错：{message}"),
                    );
                }
                return;
            }
            let (label, color) = match &self.status {
                DocStatus::Unsaved => ("未保存".to_string(), ui.visuals().weak_text_color()),
                DocStatus::Saved => (
                    "已保存".to_string(),
                    egui::Color32::from_rgb(0x2e, 0x9e, 0x44),
                ),
                DocStatus::Modified => (
                    "已修改".to_string(),
                    egui::Color32::from_rgb(0xe6, 0x7e, 0x22),
                ),
                DocStatus::Conflict => (
                    "外部冲突".to_string(),
                    egui::Color32::from_rgb(0xc0, 0x39, 0x2b),
                ),
                DocStatus::SaveFailed(msg) => (
                    format!("出错：{}", msg),
                    egui::Color32::from_rgb(0xc0, 0x39, 0x2b),
                ),
            };
            ui.colored_label(color, label);
            ui.separator();
            if let Some(p) = &self.path {
                ui.label(p.display().to_string());
            } else {
                ui.label("未命名");
            }
            ui.separator();
            ui.label(format!(
                "{} 字符 / {} 行",
                self.text.chars().count(),
                self.text.lines().count()
            ));
            if (self.text.len() as u64) > markdown::MAX_FILE_SIZE {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(0xc0, 0x39, 0x2b), "超过 10 MB 限制");
            }
            if !self.status_note.is_empty() {
                ui.separator();
                ui.label(&self.status_note);
            }
        });
    }

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        let mut query_changed = false;
        let mut go_previous = false;
        let mut go_next = false;
        let mut close = false;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 32.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                close = chrome_icon_button(ui, "×")
                    .on_hover_text("关闭查找 · Esc")
                    .clicked();
                ui.add_enabled_ui(!self.search_results.ranges().is_empty(), |ui| {
                    go_next = chrome_icon_button(ui, "↓")
                        .on_hover_text("下一项 · Enter")
                        .clicked();
                    go_previous = chrome_icon_button(ui, "↑")
                        .on_hover_text("上一项 · Shift+Enter")
                        .clicked();
                });
                let preview_has_match =
                    !matches!(self.view_mode, ViewMode::Preview | ViewMode::Split)
                        || self.preview_search_has_match();
                let count = if !preview_has_match {
                    "预览无结果".to_string()
                } else {
                    self.search_results.position().map_or_else(
                        || "无结果".to_string(),
                        |(current, total)| format!("{current} / {total}"),
                    )
                };
                ui.label(egui::RichText::new(count).weak().size(12.0));
                let response = ui.add_sized(
                    [260.0, 26.0],
                    egui::TextEdit::singleline(&mut self.search_query)
                        .id_salt("document_search_input")
                        .hint_text("查找当前文档")
                        .font(egui::FontId::new(
                            CHROME_FONT_SIZE,
                            egui::FontFamily::Proportional,
                        )),
                );
                if self.search_focus_requested {
                    response.request_focus();
                    self.search_focus_requested = false;
                }
                query_changed = response.changed();
                if response.has_focus() {
                    go_previous |= ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter)
                    });
                    go_next |= ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                }
            },
        );
        if query_changed {
            self.refresh_search();
        }
        if go_previous {
            self.search_previous();
        } else if go_next {
            self.search_next();
        }
        if close {
            self.close_search();
        }
    }

    fn empty_workspace(&mut self, ui: &mut egui::Ui) {
        let top_space = (ui.available_height() * 0.28).clamp(72.0, 220.0);
        let panel_size = ui.available_size();
        let panel_width = panel_size.x;
        ui.allocate_ui_with_layout(
            panel_size,
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.add_space(top_space);
                ui.label(
                    egui::RichText::new("开始写作")
                        .size(26.0)
                        .strong()
                        .color(ui.visuals().strong_text_color()),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("新建文档，或打开已有的 Markdown 文件")
                        .size(14.0)
                        .weak(),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("也可以将 .md、.markdown 或 .txt 文件拖到这里")
                        .size(14.0)
                        .weak(),
                );
                ui.add_space(18.0);
                let mut create = false;
                let mut open = false;
                // Allocate the full row width explicitly: `vertical_centered` otherwise shrinks
                // nested containers to their content width, leaving the actions at the left edge.
                let row_width = panel_width;
                let action_width = 116.0 * 2.0 + 8.0;
                ui.allocate_ui_with_layout(
                    egui::vec2(row_width, 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add_space(((row_width - action_width) * 0.5).max(0.0));
                        create = ui
                            .add_sized([116.0, 34.0], egui::Button::new("新建文档"))
                            .clicked();
                        ui.add_space(8.0);
                        let accent = ui.visuals().selection.bg_fill;
                        let accent_text = ui.visuals().selection.stroke.color;
                        open = ui
                            .add_sized(
                                [116.0, 34.0],
                                egui::Button::new(
                                    egui::RichText::new("打开文件…").color(accent_text),
                                )
                                .fill(accent),
                            )
                            .clicked();
                    },
                );
                if create {
                    self.new_tab();
                } else if open {
                    self.open_file();
                }
            },
        );
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn sync_scrolls(
        &mut self,
        ctx: &egui::Context,
        editor: &ScrollAreaOutput<EditorWidgetOutput>,
        preview: &ScrollAreaOutput<()>,
    ) {
        let max_e = (editor.content_size.y - editor.inner_rect.height()).max(0.0);
        let max_p = (preview.content_size.y - preview.inner_rect.height()).max(0.0);
        let ratio_e = if max_e > 0.0 {
            editor.state.offset.y / max_e
        } else {
            0.0
        };
        let ratio_p = if max_p > 0.0 {
            preview.state.offset.y / max_p
        } else {
            0.0
        };
        let e_changed = scroll_position_changed(self.prev_editor_ratio, ratio_e, max_e);
        let p_changed = scroll_position_changed(self.prev_preview_ratio, ratio_p, max_p);

        if e_changed && !p_changed {
            let mut st = preview.state;
            st.offset.y = ratio_e * max_p;
            st.store(ctx, preview.id);
        } else if p_changed && !e_changed {
            let mut st = editor.state;
            st.offset.y = ratio_p * max_e;
            st.store(ctx, editor.id);
        }
        self.prev_editor_ratio = ratio_e;
        self.prev_preview_ratio = ratio_p;
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn sync_caret(
        &mut self,
        ctx: &egui::Context,
        editor: &ScrollAreaOutput<EditorWidgetOutput>,
        preview: &ScrollAreaOutput<()>,
    ) {
        if !self.editor_focused {
            return;
        }
        let te_id = editor.inner.id;
        if let Some(state) = egui::TextEdit::load_state(ctx, te_id) {
            if let Some(cursor) = state.cursor.char_range() {
                let char_idx = cursor.primary.index.0;
                let caret_line = self
                    .text
                    .chars()
                    .take(char_idx)
                    .filter(|&c| c == '\n')
                    .count();
                if caret_line != self.last_caret_line {
                    self.last_caret_line = caret_line;
                    let total = self.text.chars().filter(|&c| c == '\n').count().max(1);
                    let ratio = caret_line as f32 / total as f32;
                    let max_p = (preview.content_size.y - preview.inner_rect.height()).max(0.0);
                    let mut st = preview.state;
                    st.offset.y = ratio * max_p;
                    st.store(ctx, preview.id);
                }
            }
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn sync_browser_scrolls(
        &mut self,
        ctx: &egui::Context,
        editor: &ScrollAreaOutput<EditorWidgetOutput>,
        force_preview: bool,
    ) -> Result<(), String> {
        let max_editor = (editor.content_size.y - editor.inner_rect.height()).max(0.0);
        let editor_ratio = if max_editor > 0.0 {
            (editor.state.offset.y / max_editor).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Only a user-originated WebView scroll is allowed to drive the editor.
        // Page reloads and scrollTo calls also emit browser scroll events; treating
        // those as input would make both panes repeatedly pull each other around.
        if let Some(anchor) = self.browser_preview.take_user_scroll_anchor() {
            let source_position = anchor.source_position;
            self.preview_source_position = source_position;
            let mut state = editor.state;
            state.offset.y = editor_offset_for_source_position(editor, &self.text, source_position);
            state.store(ctx, editor.id);
            self.prev_editor_ratio = if max_editor > 0.0 {
                state.offset.y / max_editor
            } else {
                0.0
            };
            self.prev_preview_ratio = self.prev_editor_ratio;
            return Ok(());
        }

        if let Some(source_position) = self.browser_preview.take_user_source_position() {
            self.preview_source_position = source_position;
            let mut state = editor.state;
            state.offset.y = editor_offset_for_source_position(editor, &self.text, source_position);
            state.store(ctx, editor.id);
            self.prev_editor_ratio = if max_editor > 0.0 {
                state.offset.y / max_editor
            } else {
                0.0
            };
            self.prev_preview_ratio = self.prev_editor_ratio;
            return Ok(());
        }

        let source_position = editor_source_position(editor, &self.text);
        let editor_changed =
            scroll_position_changed(self.prev_editor_ratio, editor_ratio, max_editor);
        let preview_out_of_sync = self
            .browser_preview
            .source_position()
            .is_none_or(|preview_position| (preview_position - source_position).abs() > 0.1);
        if force_preview || editor_changed || preview_out_of_sync {
            if let Some(anchor) = self.block_anchor_for_source(source_position) {
                self.browser_preview
                    .scroll_to_block_anchor(&anchor, !force_preview)?;
            } else {
                self.browser_preview
                    .scroll_to_source_position(source_position, !force_preview)?;
            }
        }
        self.preview_source_position = source_position;
        self.prev_editor_ratio = editor_ratio;
        self.prev_preview_ratio = editor_ratio;
        Ok(())
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn block_anchor_for_source(&self, source_position: f32) -> Option<web_preview::ScrollAnchor> {
        let source = &self.text;
        let line_at = |byte: usize| {
            source
                .as_bytes()
                .get(..byte.min(source.len()))
                .unwrap_or_default()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count() as f32
        };
        let index = self
            .document
            .block_index()
            .iter()
            .enumerate()
            .filter(|(_, entry)| line_at(entry.source_range.start) <= source_position)
            .map(|(index, _)| index)
            .next_back()?;
        let entry = &self.document.block_index()[index];
        let start = line_at(entry.source_range.start);
        let end = line_at(entry.source_range.end).max(start + 1.0);
        Some(web_preview::ScrollAnchor {
            block_id: entry.id,
            offset: ((source_position - start) / (end - start)).clamp(0.0, 1.0),
            source_position,
        })
    }

    fn conflict_window(&mut self, ctx: &egui::Context) {
        if self.conflict.is_none() {
            return;
        }
        egui::Window::new("保存冲突")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("磁盘上的文件已被外部程序修改，直接保存会覆盖外部内容。");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("覆盖保存").clicked() {
                        self.resolve_overwrite();
                    }
                    if ui.button("另存为…").clicked() {
                        self.resolve_save_as();
                    }
                    if ui.button("重新载入磁盘内容").clicked() {
                        self.resolve_reload();
                    }
                });
            });
    }

    fn restore_window_session(&mut self, session: window_session::WindowSession) {
        let requested = session.paths.len();
        let active_path = session.active_path;
        let mut restored = 0usize;
        for path in session.paths {
            if path.is_file() && has_supported_text_extension(&path) {
                self.open_path(&path);
                restored += 1;
            }
        }
        if let Some(active_path) = active_path
            && let Some(index) = self
                .tabs
                .iter()
                .position(|tab| tab.path.as_ref() == Some(&active_path))
        {
            self.activate_tab(index);
        }
        let skipped = requested.saturating_sub(restored);
        self.status_note = if skipped == 0 {
            format!("已恢复上次窗口，共 {restored} 个文件")
        } else {
            format!("已恢复 {restored} 个文件，跳过 {skipped} 个缺失或不支持的文件")
        };
    }

    fn restore_draft_session(&mut self, session: io::DraftSession) {
        let active_tab_id = session.active_tab_id;
        let restored = session
            .tabs
            .into_iter()
            .map(restore_draft_tab)
            .collect::<Vec<_>>();
        if restored.is_empty() {
            io::clear_draft_for_window(self.draft_window_id);
            return;
        }
        let active_index = restored
            .iter()
            .position(|tab| tab.id == active_tab_id)
            .unwrap_or(0);
        self.next_tab_id = restored
            .iter()
            .map(|tab| tab.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.tabs = restored;
        let watched_paths = self
            .tabs
            .iter()
            .filter_map(|tab| tab.path.clone())
            .collect::<Vec<_>>();
        for path in watched_paths {
            self.watch_external_path(&path);
        }
        self.activate_tab(active_index);
        self.workspace_empty = false;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            self.browser_document_cache = None;
        }
        if let Err(error) = self.persist_draft_session() {
            self.status_note = format!("草稿已恢复；草稿会话更新失败：{error}");
        }
    }

    fn recovery_window(&mut self, ctx: &egui::Context) {
        if self.recovery.is_none() {
            return;
        }
        let draft_count = self
            .recovery
            .as_ref()
            .map_or(0, |session| session.tabs.len());
        egui::Window::new("发现未保存草稿")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "上次退出时有 {draft_count} 个标签包含未保存内容，是否逐项恢复？"
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("恢复草稿").clicked()
                        && let Some(session) = self.recovery.take()
                    {
                        self.restore_draft_session(session);
                    }
                    if ui.button("放弃草稿").clicked() {
                        self.recovery = None;
                        io::clear_draft_for_window(self.draft_window_id);
                    }
                });
            });
    }

    fn close_tab_window(&mut self, ctx: &egui::Context) {
        let Some(index) = self.pending_close else {
            return;
        };
        if self.conflict.is_some() || index != self.active_tab {
            return;
        }
        let title = self.tab_title(index);
        egui::Window::new("关闭标签")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("“{title}”包含未保存的修改。"));
                ui.label("关闭前要保存这些修改吗？");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("保存并关闭").clicked() {
                        self.save();
                        if matches!(self.status, DocStatus::Saved) {
                            self.close_tab_now(index);
                        }
                    }
                    if ui.button("放弃修改").clicked() {
                        self.close_tab_now(index);
                    }
                    if ui.button("取消").clicked() {
                        self.pending_close = None;
                    }
                });
            });
    }

    fn window_close_window(&mut self, ctx: &egui::Context) {
        if !self.window_close_guard.is_confirmation_open()
            || self.conflict.is_some()
            || self.recovery.is_some()
            || self.pending_close.is_some()
        {
            return;
        }
        let documents = self.window_close_guard.unsaved_documents().to_vec();
        let failed_tab_id = self.window_close_guard.failed_tab_id();
        egui::Modal::new(egui::Id::new("window-close-confirmation")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("关闭窗口");
            ui.add_space(6.0);
            ui.label(format!(
                "有 {} 个标签包含未保存的修改。关闭窗口前要保存吗？",
                documents.len()
            ));
            ui.add_space(10.0);
            egui::Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("window-close-unsaved-documents")
                        .max_height(180.0)
                        .show(ui, |ui| {
                            for document in &documents {
                                ui.horizontal(|ui| {
                                    ui.label("•");
                                    ui.label(&document.title);
                                    if failed_tab_id == Some(document.tab_id) {
                                        ui.colored_label(ui.visuals().error_fg_color, "保存失败");
                                    }
                                });
                            }
                        });
                });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("全部保存并关闭").clicked() {
                    let request_id = self
                        .window_close_guard
                        .confirmation_id()
                        .expect("确认窗口只在待确认状态显示");
                    let result = self.save_all_for_window_close();
                    if let Err(tab_id) = result
                        && let Some(index) =
                            self.tabs.iter().position(|tab| tab.id == u64::from(tab_id))
                    {
                        let title = self.tab_title(index);
                        self.status_note = format!("未能保存“{title}”，窗口保持打开");
                    }
                    if self.window_close_guard.finish_save_all(request_id, result)
                        == window_close::CloseAction::Allow
                    {
                        io::clear_draft_for_window(self.draft_window_id);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                if ui.button("放弃全部修改").clicked() {
                    self.window_close_guard.discard_all();
                    io::clear_draft_for_window(self.draft_window_id);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("取消").clicked() {
                    self.window_close_guard.cancel();
                }
            });
        });
    }
}

impl eframe::App for MdEditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Keep the rendered egui palette in lockstep with the persisted appearance state.
        // This also repairs the first frame after startup/window restoration if another
        // egui component has reinstalled its default visuals.
        let current_theme = self.theme_spec();
        apply_visuals(&ctx, self.dark, &current_theme);
        let now = ctx.input(|i| i.time);
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let mut browser_rect = None;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let mut split_editor_scroll = None;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let mut preview_heading_target = None;
        let mut editor_changed = false;

        let mut dropped_paths = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        dropped_paths.extend(self.browser_preview.take_dropped_paths());
        self.handle_instance_requests(&ctx);
        self.open_dropped_paths(dropped_paths);

        self.poll_external_changes(&ctx, now);

        self.poll_document_parse_results(&ctx);
        if !self.parse_pending && self.text != self.document.source() {
            // Catch programmatic changes that do not pass through the editor
            // widget's changed flag.
            self.queue_document_parse(now);
        }

        self.autosave_draft(now);
        self.handle_window_close_request(&ctx);
        self.handle_shortcuts(&ctx);
        self.refresh_search_if_needed();

        if !self.focus_mode {
            egui::Panel::top("menu_panel")
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(10, 2)),
                )
                .show(ui, |ui| {
                    self.title_bar(ui);
                });
        }

        if self.search_open {
            egui::Panel::top("search_panel")
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(12, 3))
                        .stroke(egui::Stroke::new(
                            1.0,
                            ui.visuals().widgets.noninteractive.bg_stroke.color,
                        )),
                )
                .show(ui, |ui| self.search_bar(ui));
        }

        if self.show_status && !self.focus_mode {
            egui::Panel::bottom("status_panel")
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(12, 5)),
                )
                .show(ui, |ui| self.status_bar(ui));
        }

        if self.workspace_empty {
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            self.release_browser_preview();
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(ui.visuals().window_fill))
                .show(ui, |ui| self.empty_workspace(ui));
            self.recovery_window(&ctx);
            self.persist_window_session_if_changed();
            return;
        }

        let doc_theme = self.theme_spec();
        let editor_fill = doc_theme.editor_canvas;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let modal_open = self.conflict.is_some()
            || self.recovery.is_some()
            || self.pending_close.is_some()
            || self.window_close_guard.is_confirmation_open();
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if modal_open {
            // A native child WebView is always above egui's paint surface on Windows.
            // Drop it before painting a modal instead of relying on an asynchronous
            // visibility change, otherwise the preview can cover the dialog.
            self.browser_preview.close();
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let browser_can_show = {
            #[cfg(target_os = "windows")]
            {
                !modal_open
            }
            #[cfg(target_os = "macos")]
            {
                !modal_open && !ctx.any_popup_open()
            }
        };
        let search_range = self
            .search_open
            .then(|| self.search_results.current_range())
            .flatten();
        let scroll_to_search = std::mem::take(&mut self.search_scroll_requested);
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let mut search_preview_target =
            (matches!(self.view_mode, ViewMode::Preview | ViewMode::Split)
                && scroll_to_search
                && self.preview_search_has_match())
            .then(|| {
                search_range
                    .as_ref()
                    .map(|range| source_position_from_char(&self.text, range.start))
            })
            .flatten();
        let search_preview_clear = matches!(self.view_mode, ViewMode::Preview | ViewMode::Split)
            && scroll_to_search
            && search_range.is_none();

        match self.view_mode {
            ViewMode::Write => {
                let active_index = self.active_tab;
                let active_tab_id = self.tabs[active_index].id;
                let body_font_size = self.body_font_size;
                let (tabs, editor_focused) = (&mut self.tabs, &mut self.editor_focused);
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(editor_fill)
                            .inner_margin(egui::Margin::symmetric(22, 0)),
                    )
                    .show(ui, |ui| {
                        editor_changed = show_centered_editor(
                            ui,
                            active_tab_id,
                            &mut tabs[active_index].text,
                            editor_focused,
                            body_font_size,
                            &doc_theme,
                            EditorSearchTarget {
                                range: search_range.as_ref(),
                                scroll_to_search,
                            },
                        );
                    });
            }
            ViewMode::Preview => {
                let active_tab_id = self.id;
                egui::Panel::left("reading_toc_panel")
                    .resizable(false)
                    .exact_size(228.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ui.visuals().panel_fill)
                            .inner_margin(egui::Margin::symmetric(14, 0)),
                    )
                    .show(ui, |ui| {
                        let target = reading_toc(ui, self.document.blocks());
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        if target.is_some() {
                            preview_heading_target = target;
                        }
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(ui.visuals().window_fill))
                    .show(ui, |ui| {
                        #[cfg(any(target_os = "windows", target_os = "macos"))]
                        {
                            if browser_can_show {
                                let rect = ui.available_rect_before_wrap();
                                ui.allocate_rect(rect, egui::Sense::hover());
                                browser_rect = Some(rect);
                            } else {
                                show_centered_preview(
                                    ui,
                                    active_tab_id,
                                    self.document.blocks(),
                                    self.body_font_size,
                                    &doc_theme,
                                );
                            }
                        }
                        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                        show_centered_preview(
                            ui,
                            active_tab_id,
                            self.document.blocks(),
                            self.body_font_size,
                            &doc_theme,
                        );
                    });
            }
            ViewMode::Split => {
                let active_index = self.active_tab;
                let active_tab_id = self.tabs[active_index].id;
                let body_font_size = self.body_font_size;
                let (tabs, editor_focused) = (&mut self.tabs, &mut self.editor_focused);
                let editor_out = egui::Panel::left("editor_panel")
                    .resizable(true)
                    .default_size(600.0)
                    .min_size(280.0)
                    .frame(
                        egui::Frame::new()
                            .fill(editor_fill)
                            .inner_margin(egui::Margin {
                                left: (doc_theme.preview_padding / 2).max(20),
                                right: (doc_theme.preview_padding / 2).max(20),
                                top: 0,
                                bottom: 0,
                            }),
                    )
                    .show(ui, |ui| {
                        show_editor_scroll(
                            ui,
                            active_tab_id,
                            &mut tabs[active_index].text,
                            editor_focused,
                            body_font_size,
                            search_range.as_ref(),
                            scroll_to_search,
                        )
                    });
                editor_changed = editor_out.inner.inner.changed;
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(ui.visuals().window_fill))
                    .show(ui, |ui| {
                        if browser_can_show {
                            let rect = ui.available_rect_before_wrap();
                            ui.allocate_rect(rect, egui::Sense::hover());
                            browser_rect = Some(rect);
                        } else {
                            let _ = show_preview_scroll(
                                ui,
                                active_tab_id,
                                self.document.blocks(),
                                self.body_font_size,
                                &doc_theme,
                            );
                        }
                    });
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                {
                    let preview_out = egui::CentralPanel::default()
                        .frame(
                            egui::Frame::new()
                                .fill(ui.visuals().window_fill)
                                .inner_margin(egui::Margin {
                                    left: doc_theme.preview_padding,
                                    right: doc_theme.preview_padding,
                                    top: 0,
                                    bottom: 0,
                                }),
                        )
                        .show(ui, |ui| {
                            show_preview_scroll(
                                ui,
                                active_tab_id,
                                self.document.blocks(),
                                self.body_font_size,
                                &doc_theme,
                            )
                        });
                    self.sync_scrolls(&ctx, &editor_out.inner, &preview_out.inner);
                    self.sync_caret(&ctx, &editor_out.inner, &preview_out.inner);
                }
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                {
                    split_editor_scroll = Some(editor_out.inner);
                }
            }
        }

        if editor_changed {
            self.queue_document_parse(now);
        }

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let popup_open = ctx.any_popup_open();
            if popup_open && let Some(browser_rect) = browser_rect {
                self.browser_preview.freeze_for_overlay(
                    frame,
                    &ctx,
                    browser_rect,
                    ctx.pixels_per_point(),
                );
            } else if browser_rect.is_none() {
                self.release_browser_preview();
            } else if let Some(rect) = browser_rect {
                self.poll_browser_render_results(&ctx);
                self.browser_preview.discard_frozen_frame();
                let preview_refresh_remaining = (self.view_mode == ViewMode::Split
                    && self.last_edit_time.is_finite())
                .then(|| PREVIEW_REFRESH_DEBOUNCE_SECONDS - (now - self.last_edit_time))
                .filter(|remaining| *remaining > 0.0);
                if let Some(remaining) = preview_refresh_remaining {
                    ctx.request_repaint_after(Duration::from_secs_f64(remaining));
                }
                let document = self.browser_document(preview_refresh_remaining.is_some());
                match self.browser_preview.show(
                    frame,
                    &ctx,
                    rect,
                    ctx.pixels_per_point(),
                    &document,
                ) {
                    Err(error) => self.status_note = error,
                    Ok(apply_kind) => {
                        if matches!(apply_kind, web_preview::PreviewApplyKind::Navigated) {
                            self.render_metrics.record_webview_navigation();
                        }
                        if let Err(error) = self
                            .browser_preview
                            .set_body_font_size(self.body_font_size, self.browser_base_font_size())
                        {
                            self.status_note = error;
                        }
                        let document_changed = self.browser_preview.take_document_changed();
                        if let Some(editor) = split_editor_scroll.as_ref()
                            && let Err(error) =
                                self.sync_browser_scrolls(&ctx, editor, document_changed)
                        {
                            self.status_note = error;
                        }
                        if document_changed {
                            self.pending_preview_restore =
                                Some((self.id, self.preview_source_position));
                        }
                        if self.view_mode == ViewMode::Preview {
                            if let Some(anchor) = self.browser_preview.take_user_scroll_anchor() {
                                self.preview_source_position = anchor.source_position;
                                self.pending_preview_restore = None;
                            } else if let Some(source_position) =
                                self.browser_preview.take_user_source_position()
                            {
                                self.preview_source_position = source_position;
                                self.pending_preview_restore = None;
                            }
                        }
                        if let Some(index) = preview_heading_target.take() {
                            match self.browser_preview.scroll_to_heading(index) {
                                Ok(()) => self.pending_preview_restore = None,
                                Err(error) => self.status_note = error,
                            }
                        }
                        if let Some(source_position) = search_preview_target.take() {
                            match self.browser_preview.find_text(
                                &self.search_query,
                                source_position,
                                self.search_backwards,
                            ) {
                                Ok(()) => {
                                    self.preview_source_position = source_position;
                                    self.pending_preview_restore = None;
                                }
                                Err(error) => self.status_note = error,
                            }
                        } else if search_preview_clear
                            && let Err(error) = self.browser_preview.find_text("", 0.0, false)
                        {
                            self.status_note = error;
                        }
                        if let Some(ready) = self.browser_preview.take_ready() {
                            if let Some((tab_id, source_position)) = self.pending_preview_restore
                                && tab_id == self.id
                            {
                                match self
                                    .browser_preview
                                    .scroll_to_source_position(source_position, false)
                                {
                                    Ok(()) => self.pending_preview_restore = None,
                                    Err(error) => self.status_note = error,
                                }
                            }
                            self.finish_benchmark_probe(ready);
                        }
                    }
                }
                if self.editor_focused {
                    self.browser_preview.focus_parent();
                }
            }
        }
        self.conflict_window(&ctx);
        self.recovery_window(&ctx);
        self.close_tab_window(&ctx);
        self.window_close_window(&ctx);
        self.persist_window_session_if_changed();
    }
}

struct EditorWidgetOutput {
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    id: egui::Id,
    galley: Arc<egui::Galley>,
    galley_pos: egui::Pos2,
    changed: bool,
}

fn editor_widget(
    ui: &mut egui::Ui,
    text: &mut String,
    focused: &mut bool,
    font_size: f32,
    search_range: Option<&Range<usize>>,
) -> EditorWidgetOutput {
    let id = ui.id().with("md_text");
    let font_id = egui::FontId::new(font_size, egui::FontFamily::Monospace);
    let edit = egui::TextEdit::multiline(text)
        .id(id)
        .font(font_id.clone())
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::same(0))
        .desired_width(f32::INFINITY)
        .desired_rows(40);
    let output = if let Some(range) = search_range {
        let range = range.clone();
        let mut layouter = move |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
            let mut job = egui::text::LayoutJob::simple(
                buffer.as_str().to_owned(),
                font_id.clone(),
                ui.visuals().widgets.inactive.text_color(),
                wrap_width,
            );
            job.keep_trailing_whitespace = true;
            let mut galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
            egui::text_selection::visuals::paint_text_selection(
                &mut galley,
                ui.visuals(),
                &egui::text::CCursorRange::two(
                    egui::text::CCursor::new(range.start),
                    egui::text::CCursor::new(range.end),
                ),
                None,
            );
            galley
        };
        edit.layouter(&mut layouter).show(ui)
    } else {
        edit.show(ui)
    };
    let changed = output.response.changed();
    *focused = output.response.has_focus();
    EditorWidgetOutput {
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        id,
        galley: output.galley,
        galley_pos: output.galley_pos,
        changed,
    }
}

fn document_scroll_id(scope: &'static str, tab_id: u64) -> egui::Id {
    egui::Id::new((scope, tab_id))
}

fn show_editor_scroll(
    ui: &mut egui::Ui,
    tab_id: u64,
    text: &mut String,
    focused: &mut bool,
    font_size: f32,
    search_range: Option<&Range<usize>>,
    scroll_to_search: bool,
) -> ScrollAreaOutput<EditorWidgetOutput> {
    egui::ScrollArea::vertical()
        .id_salt(document_scroll_id("editor_scroll", tab_id))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(28.0);
            let output = editor_widget(ui, text, focused, font_size, search_range);
            if scroll_to_search && let Some(range) = search_range {
                scroll_editor_to_search(ui, &output, range);
            }
            output
        })
}

fn scroll_editor_to_search(ui: &egui::Ui, output: &EditorWidgetOutput, range: &Range<usize>) {
    let local = output
        .galley
        .pos_from_cursor(egui::text::CCursor::new(range.start));
    let screen =
        local.translate(output.galley_pos.to_vec2() - egui::vec2(output.galley.rect.left(), 0.0));
    ui.scroll_to_rect(
        screen.expand2(egui::vec2(24.0, 16.0)),
        Some(egui::Align::Center),
    );
}

fn show_preview_scroll(
    ui: &mut egui::Ui,
    tab_id: u64,
    blocks: &[Block],
    body_font_size: f32,
    theme: &ThemeSpec,
) -> ScrollAreaOutput<()> {
    egui::ScrollArea::vertical()
        .id_salt(document_scroll_id("preview_scroll", tab_id))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(28.0);
            preview::show_preview_with_theme(ui, blocks, body_font_size, theme);
        })
}

struct EditorSearchTarget<'a> {
    range: Option<&'a Range<usize>>,
    scroll_to_search: bool,
}

fn show_centered_editor(
    ui: &mut egui::Ui,
    tab_id: u64,
    text: &mut String,
    focused: &mut bool,
    font_size: f32,
    theme: &ThemeSpec,
    search: EditorSearchTarget<'_>,
) -> bool {
    let mut changed = false;
    egui::ScrollArea::vertical()
        .id_salt(document_scroll_id("editor_scroll_solo", tab_id))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let width = ui.available_width().min(theme.content_width);
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - width) / 2.0).max(20.0));
                ui.allocate_ui_with_layout(
                    egui::vec2(width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.add_space(54.0);
                        let output = editor_widget(ui, text, focused, font_size, search.range);
                        changed = output.changed;
                        if search.scroll_to_search
                            && let Some(range) = search.range
                        {
                            scroll_editor_to_search(ui, &output, range);
                        }
                        ui.add_space(160.0);
                    },
                );
            });
        });
    changed
}

fn show_centered_preview(
    ui: &mut egui::Ui,
    tab_id: u64,
    blocks: &[Block],
    body_font_size: f32,
    theme: &ThemeSpec,
) {
    egui::ScrollArea::vertical()
        .id_salt(document_scroll_id("preview_scroll_solo", tab_id))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let width = ui.available_width().min(theme.content_width);
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - width) / 2.0).max(20.0));
                ui.allocate_ui_with_layout(
                    egui::vec2(width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.add_space(54.0);
                        preview::show_preview_with_theme(ui, blocks, body_font_size, theme);
                        ui.add_space(160.0);
                    },
                );
            });
        });
}

fn setup_fonts(ctx: &egui::Context) {
    export::install_app_fonts(ctx);
    ctx.all_styles_mut(|style| {
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(16.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(12.0, egui::FontFamily::Proportional),
        );
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(9.0, 5.0);
        style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(5);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(5);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(5);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(5);
    });
}

fn apply_visuals(ctx: &egui::Context, dark: bool, spec: &ThemeSpec) {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.window_fill = spec.canvas;
    visuals.panel_fill = spec.panel;
    visuals.extreme_bg_color = spec.code_bg;
    visuals.faint_bg_color = spec.quote_bg;
    visuals.hyperlink_color = spec.accent;
    visuals.override_text_color = Some(spec.text);
    let text_stroke = egui::Stroke::new(1.0, spec.text);
    let border_stroke = egui::Stroke::new(1.0, spec.border);
    visuals.widgets.noninteractive.fg_stroke = text_stroke;
    visuals.widgets.inactive.fg_stroke = text_stroke;
    visuals.widgets.hovered.fg_stroke = text_stroke;
    visuals.widgets.active.fg_stroke = text_stroke;
    visuals.widgets.noninteractive.bg_stroke = border_stroke;
    visuals.widgets.inactive.bg_stroke = border_stroke;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, spec.accent);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, spec.accent);
    visuals.widgets.noninteractive.bg_fill = spec.panel;
    visuals.widgets.inactive.bg_fill = spec.panel;
    visuals.widgets.hovered.bg_fill = spec.code_bg;
    visuals.widgets.active.bg_fill = spec.accent.gamma_multiply(if dark { 0.32 } else { 0.16 });
    visuals.widgets.noninteractive.weak_bg_fill = spec.panel;
    visuals.widgets.inactive.weak_bg_fill = spec.panel;
    visuals.widgets.hovered.weak_bg_fill =
        spec.accent.gamma_multiply(if dark { 0.22 } else { 0.08 });
    visuals.widgets.active.weak_bg_fill =
        spec.accent.gamma_multiply(if dark { 0.32 } else { 0.16 });
    visuals.selection.bg_fill = spec.accent.gamma_multiply(if dark { 0.42 } else { 0.20 });
    if !dark {
        visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.hovered.weak_bg_fill = spec.accent.gamma_multiply(0.08);
        visuals.widgets.active.weak_bg_fill = spec.accent.gamma_multiply(0.16);
    }
    ctx.set_visuals(visuals);
}

fn pick_save_path() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("Markdown", &["md", "markdown", "txt"])
        .set_file_name("未命名.md")
        .save_file()
}

fn has_supported_text_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md")
                || extension.eq_ignore_ascii_case("markdown")
                || extension.eq_ignore_ascii_case("txt")
        })
}

fn describe_read_error(e: &io::ReadError) -> String {
    match e {
        io::ReadError::TooLarge { size, limit } => {
            format!("文件 {} 字节，超过 {} 字节限制", size, limit)
        }
        io::ReadError::InvalidUtf8 => "编码无法识别（不是有效的 UTF-8 文本）".to_string(),
        io::ReadError::Io(msg) => msg.clone(),
    }
}

fn clock_time() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// Compare scroll progress in physical content pixels instead of a fixed ratio.
/// A ratio threshold makes long documents update in large visible chunks.
fn scroll_position_changed(previous_ratio: f32, current_ratio: f32, max_scroll: f32) -> bool {
    (current_ratio - previous_ratio).abs() * max_scroll > 0.5
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn editor_source_position(editor: &ScrollAreaOutput<EditorWidgetOutput>, text: &str) -> f32 {
    let galley_y = (editor.inner_rect.top() - editor.inner.galley_pos.y).max(0.0);
    let cursor = editor
        .inner
        .galley
        .cursor_from_pos(egui::vec2(editor.inner.galley.rect.left(), galley_y));
    source_position_from_char(text, cursor.index.0)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn editor_offset_for_source_position(
    editor: &ScrollAreaOutput<EditorWidgetOutput>,
    text: &str,
    source_position: f32,
) -> f32 {
    let char_index = char_index_from_source_position(text, source_position);
    let cursor_rect = editor
        .inner
        .galley
        .pos_from_cursor(egui::text::CCursor::new(char_index));
    let cursor_screen_y = editor.inner.galley_pos.y + cursor_rect.top();
    let max_scroll = (editor.content_size.y - editor.inner_rect.height()).max(0.0);
    (editor.state.offset.y + cursor_screen_y - editor.inner_rect.top()).clamp(0.0, max_scroll)
}

fn source_position_from_char(text: &str, char_index: usize) -> f32 {
    let mut line = 0usize;
    let mut line_start = 0usize;
    let bounded_index = char_index.min(text.chars().count());
    for (index, ch) in text.chars().take(bounded_index).enumerate() {
        if ch == '\n' {
            line += 1;
            line_start = index + 1;
        }
    }
    let line_length = text
        .chars()
        .skip(line_start)
        .take_while(|ch| *ch != '\n')
        .count();
    let column = bounded_index.saturating_sub(line_start).min(line_length);
    line as f32 + column as f32 / line_length.max(1) as f32
}

fn char_index_from_source_position(text: &str, source_position: f32) -> usize {
    let target_line = source_position.max(0.0).floor() as usize;
    let fraction = source_position.max(0.0).fract();
    let mut char_index = 0usize;
    for (line_index, line) in text.split('\n').enumerate() {
        let line_length = line.chars().count();
        if line_index == target_line {
            return char_index + (fraction * line_length as f32).round() as usize;
        }
        char_index += line_length + 1;
    }
    text.chars().count()
}

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn scroll_state_ids_are_isolated_per_document_tab() {
        assert_ne!(
            document_scroll_id("editor_scroll", 1),
            document_scroll_id("editor_scroll", 2)
        );
        assert_ne!(
            document_scroll_id("preview_scroll", 1),
            document_scroll_id("preview_scroll", 2)
        );
    }

    #[test]
    fn switching_tabs_queues_each_documents_own_preview_position() {
        let mut app = app_with_two_tabs();
        app.tabs[0].preview_source_position = 12.5;
        app.tabs[1].preview_source_position = 84.25;

        app.activate_tab(1);
        assert_eq!(app.pending_preview_restore, Some((2, 84.25)));

        app.activate_tab(0);
        assert_eq!(app.pending_preview_restore, Some((1, 12.5)));
    }

    #[test]
    fn closing_active_tab_restores_the_next_documents_position() {
        let mut app = app_with_two_tabs();
        app.tabs[1].preview_source_position = 42.0;

        app.close_tab_now(0);

        assert_eq!(app.id, 2);
        assert_eq!(app.pending_preview_restore, Some((2, 42.0)));
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn leaving_preview_releases_the_rendered_document_cache() {
        let mut app = app_with_two_tabs();
        let document = app.browser_document(false);
        assert!(app.browser_document_cache.is_some());
        assert!(std::sync::Arc::strong_count(&document) > 1);

        app.release_browser_preview();

        assert!(app.browser_document_cache.is_none());
        assert_eq!(std::sync::Arc::strong_count(&document), 1);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn split_editing_queues_background_preview_after_debounce() {
        let mut app = app_with_two_tabs();
        let initial = app.browser_document(false);
        app.text = "# 已更新\n".to_string();
        app.document = markdown::parse_document(&app.text);
        app.document_revision = app.document_revision.wrapping_add(1);

        let deferred = app.browser_document(true);
        assert!(std::sync::Arc::ptr_eq(&initial, &deferred));

        let refreshed = app.browser_document(false);
        assert!(std::sync::Arc::ptr_eq(&initial, &refreshed));
        assert!(app.render_pending.is_some());
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn debounce_never_reuses_another_tabs_preview() {
        let mut app = app_with_two_tabs();
        let first = app.browser_document(false);

        app.activate_tab(1);
        let second = app.browser_document(true);

        assert!(!std::sync::Arc::ptr_eq(&first, &second));
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn browser_key_tracks_source_while_parse_is_pending() {
        let mut app = app_with_two_tabs();
        let initial = app.current_browser_document_key();
        app.text.push_str("\n追加内容");
        app.document_revision = app.document_revision.wrapping_add(1);
        let pending = app.current_browser_document_key();

        assert_ne!(initial.document_source_hash, pending.document_source_hash);
        assert_ne!(initial, pending);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn render_is_not_queued_until_parsed_document_matches_current_text() {
        let mut app = app_with_two_tabs();
        app.text.push_str("\n尚未完成解析");
        app.document_revision = app.document_revision.wrapping_add(1);
        let key = app.current_browser_document_key();

        app.queue_browser_render(key);

        assert!(app.render_pending.is_none());
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn render_metrics_record_incremental_cost_and_navigation_fallbacks() {
        let mut metrics = RenderMetrics::default();
        metrics.record(RenderTelemetry {
            elapsed_ms: 4.0,
            replaced_blocks: 2,
            replaced_virtual_chunks: 1,
            full_render: false,
        });
        metrics.record(RenderTelemetry {
            elapsed_ms: 9.0,
            replaced_blocks: 5,
            replaced_virtual_chunks: 3,
            full_render: true,
        });

        assert_eq!(metrics.full_render_count, 1);
        assert_eq!(metrics.replaced_blocks, 7);
        assert_eq!(metrics.replaced_virtual_chunks, 4);
        assert_eq!(metrics.edit_p95_ms(), 9.0);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn stale_render_result_is_rejected_when_source_changed() {
        let app = app_with_two_tabs();
        let current_key = app.current_browser_document_key();
        let stale_source = format!("{}\n过期内容", app.text);
        let stale_document = Arc::new(markdown::parse_document(&stale_source));
        let result = RenderResult {
            key: current_key.clone(),
            document: Arc::new(web_preview::preview_document_placeholder("", None, None)),
            parsed_document: stale_document,
            metrics: RenderTelemetry::default(),
        };

        assert!(!render_result_matches_current(
            &result,
            &current_key,
            &app.text
        ));
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn browser_font_size_change_keeps_render_context() {
        let mut app = app_with_two_tabs();
        let initial = app.current_browser_document_key();
        app.body_font_size += 1.0;
        let changed = app.current_browser_document_key();

        assert_ne!(initial.body_font_size_bits, changed.body_font_size_bits);
        assert!(initial.same_render_context(&changed));
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn appearance_change_does_not_reuse_previous_themed_shell() {
        let mut app = app_with_two_tabs();
        let _ = app.browser_document(false);
        let mut changed = app.current_browser_document_key();
        changed.theme_revision = changed.theme_revision.wrapping_add(1);

        let (previous, previous_parsed) = app.previous_browser_render_documents(&changed);

        assert!(previous.is_none());
        assert!(previous_parsed.is_none());
    }

    #[test]
    fn 强制新窗口参数绕过单实例并保留文件路径() {
        let path = std::env::temp_dir().join("markdown-editor-new-window.md");
        let launch = LaunchOptions::from_args([
            "--new-window".to_string(),
            path.to_string_lossy().into_owned(),
        ]);

        assert!(launch.force_new_window);
        assert!(!launch.uses_single_instance());
        assert_eq!(launch.open_paths, vec![path]);
    }

    #[test]
    fn 仅无参数主窗口恢复上次窗口() {
        let plain = LaunchOptions::from_args(Vec::<String>::new());
        let file = LaunchOptions::from_args(["C:/notes/opened.md".to_string()]);
        let secondary = LaunchOptions::from_args(["--new-window".to_string()]);

        assert!(plain.should_restore_window());
        assert!(!file.should_restore_window());
        assert!(!secondary.should_restore_window());
    }

    fn app_with_two_tabs() -> MdEditorApp {
        MdEditorApp {
            tabs: vec![DocumentTab::blank(1), DocumentTab::blank(2)],
            active_tab: 0,
            next_tab_id: 3,
            workspace_empty: false,
            search_open: false,
            search_query: String::new(),
            search_results: search::SearchResults::default(),
            search_tab_id: None,
            search_document_revision: 0,
            search_focus_requested: false,
            search_scroll_requested: false,
            search_backwards: false,
            pending_close: None,
            window_close_guard: window_close::CloseGuard::default(),
            recovery: None,
            dark: false,
            editor_focused: false,
            view_mode: ViewMode::Write,
            focus_mode: false,
            show_status: true,
            body_font_size: 15.0,
            theme_package: None,
            theme_revision: 1,
            auto_reload_external: true,
            last_external_poll: f64::NEG_INFINITY,
            external_watcher: None,
            observed_file_stamps: HashMap::new(),
            pending_external_changes: HashMap::new(),
            parse_worker: ParseWorker::new(),
            instance_requests: None,
            draft_window_id: None,
            persisted_window_session: None,
            window_session_initialized: false,
            pending_preview_restore: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            browser_preview: web_preview::BrowserPreview::default(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            benchmark_probe: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            browser_document_cache: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            render_worker: RenderWorker::new(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            render_pending: None,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            render_metrics: RenderMetrics::default(),
        }
    }

    #[test]
    fn 后台解析完成后应用当前标签文档() {
        let mut app = app_with_two_tabs();
        app.text = "# 后台解析\n\n正文".to_string();
        app.queue_document_parse(1.0);
        let revision = app.document_revision;
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.parse_pending {
            app.poll_document_parse_results(&ctx);
            assert!(Instant::now() < deadline, "后台解析未在测试期限内完成");
            std::thread::yield_now();
        }
        assert_eq!(app.document_revision, revision);
        assert_eq!(app.document.source(), "# 后台解析\n\n正文");
        assert!(!app.parse_pending);
    }

    #[test]
    fn window_close_collects_every_unsaved_tab_not_only_the_active_one() {
        let mut app = app_with_two_tabs();
        app.tabs[0].text = "first draft".to_string();
        app.tabs[1].text = "second draft".to_string();

        assert_eq!(
            app.prepare_window_close(),
            window_close::CloseAction::Confirm
        );
        assert_eq!(
            app.window_close_guard
                .unsaved_documents()
                .iter()
                .map(|document| document.tab_id)
                .map(u64::from)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn save_all_before_window_close_writes_every_named_document() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-window-close-save-all-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let first = directory.join("first.md");
        let second = directory.join("second.md");
        std::fs::write(&first, "old first").unwrap();
        std::fs::write(&second, "old second").unwrap();
        let mut app = app_with_two_tabs();
        app.tabs = vec![
            DocumentTab::from_file(
                1,
                first.clone(),
                "new first".to_string(),
                b"old first".to_vec(),
            ),
            DocumentTab::from_file(
                2,
                second.clone(),
                "new second".to_string(),
                b"old second".to_vec(),
            ),
        ];
        app.prepare_window_close();

        assert_eq!(app.save_all_for_window_close(), Ok(()));
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "new first");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "new second");
        assert!(app.tabs.iter().all(|tab| !document_is_dirty(
            tab.path.as_ref(),
            &tab.text,
            &tab.disk_snapshot,
            &tab.status
        )));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn save_all_before_window_close_stops_when_one_document_cannot_be_saved() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-window-close-save-failure-{}",
            std::process::id()
        ));
        let missing_directory = directory.join("missing");
        let second = directory.join("second.md");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&second, "old second").unwrap();
        let mut app = app_with_two_tabs();
        app.tabs = vec![
            DocumentTab::from_file(
                1,
                missing_directory.join("first.md"),
                "new first".to_string(),
                b"old first".to_vec(),
            ),
            DocumentTab::from_file(
                2,
                second.clone(),
                "new second".to_string(),
                b"old second".to_vec(),
            ),
        ];
        app.prepare_window_close();

        let result = app.save_all_for_window_close();
        let request_id = app.window_close_guard.confirmation_id().unwrap();

        assert_eq!(result, Err(window_close::TabId::from(1)));
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "old second");
        assert_eq!(
            app.window_close_guard.finish_save_all(request_id, result),
            window_close::CloseAction::KeepOpen
        );
        assert_eq!(
            app.window_close_guard.failed_tab_id(),
            Some(window_close::TabId::from(1))
        );
        assert!(app.window_close_guard.is_confirmation_open());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn 内置应用图标尺寸与透明通道有效() {
        let icon = app_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] == 255));
    }

    #[test]
    fn 空白标签不显示修改标记() {
        let tab = DocumentTab::blank(7);
        assert!(!document_is_dirty(
            tab.path.as_ref(),
            &tab.text,
            &tab.disk_snapshot,
            &tab.status,
        ));
        assert_eq!(document_label(tab.id, tab.path.as_ref(), false), "未命名 7");
    }

    #[test]
    fn 关闭最后一个标签后进入空工作区而不创建新标签() {
        let mut app = app_with_two_tabs();
        app.tabs.truncate(1);
        app.next_tab_id = 2;

        app.close_tab_now(0);

        assert!(!app.has_open_document());
        assert_eq!(app.visible_tab_count(), 0);
    }

    #[test]
    fn 每个文件标签独立判断修改状态() {
        let path = PathBuf::from("notes.md");
        let mut tab = DocumentTab::from_file(
            3,
            path.clone(),
            "原文".to_string(),
            "原文".as_bytes().to_vec(),
        );
        assert!(!document_is_dirty(
            tab.path.as_ref(),
            &tab.text,
            &tab.disk_snapshot,
            &tab.status,
        ));
        tab.text.push_str("修改");
        assert!(document_is_dirty(
            tab.path.as_ref(),
            &tab.text,
            &tab.disk_snapshot,
            &tab.status,
        ));
        assert_eq!(document_label(tab.id, Some(&path), true), "notes.md  •");
    }

    #[test]
    fn 界面直接修改活动标签且切换无需写回() {
        let mut app = app_with_two_tabs();
        app.text = "第一个标签".to_string();
        app.activate_tab(1);
        app.text = "第二个标签".to_string();
        assert_eq!(app.tabs[0].text, "第一个标签");
        assert_eq!(app.tabs[1].text, "第二个标签");
        app.activate_tab(0);
        assert_eq!(app.text, "第一个标签");
    }

    #[test]
    fn 草稿会话收集全部未保存标签并保留活动标签() {
        let mut app = app_with_two_tabs();
        app.tabs[0].text = "草稿一".to_string();
        app.tabs[1].text = "草稿二".to_string();
        let mut cleared = DocumentTab::from_file(
            3,
            PathBuf::from("cleared.md"),
            "原文".to_string(),
            "原文".as_bytes().to_vec(),
        );
        cleared.text.clear();
        app.tabs.push(cleared);
        app.activate_tab(1);
        let session = app.draft_session().expect("全部未保存标签都应进入草稿会话");
        assert_eq!(session.active_tab_id, 2);
        assert_eq!(session.tabs.len(), 3);
        assert_eq!(session.tabs[0].text, "草稿一");
        assert_eq!(session.tabs[1].text, "草稿二");
        assert_eq!(session.tabs[2].path, Some(PathBuf::from("cleared.md")));
        assert!(session.tabs[2].text.is_empty());
    }

    #[test]
    fn 窗口会话只记录文件标签和当前活动文件() {
        let mut app = app_with_two_tabs();
        let first = PathBuf::from("C:/notes/first.md");
        let second = PathBuf::from("C:/notes/second.md");
        app.tabs[0] = DocumentTab::from_file(
            1,
            first.clone(),
            "第一份".to_string(),
            "第一份".as_bytes().to_vec(),
        );
        app.tabs.push(DocumentTab::from_file(
            3,
            second.clone(),
            "第二份".to_string(),
            "第二份".as_bytes().to_vec(),
        ));
        app.activate_tab(2);

        let session = app.window_session().unwrap();

        assert_eq!(session.paths, vec![first, second.clone()]);
        assert_eq!(session.active_path, Some(second));
    }

    #[test]
    fn 恢复草稿时磁盘已变化则保留正文并标记冲突() {
        let path = std::env::temp_dir().join(format!(
            "markdown-editor-draft-conflict-test-{}.md",
            std::process::id()
        ));
        std::fs::write(&path, "Agent 新内容").unwrap();
        let draft = io::DraftTab::new(
            7,
            Some(path.clone()),
            "本地未保存内容".to_string(),
            "原磁盘内容".as_bytes(),
        );
        let tab = restore_draft_tab(draft);
        assert_eq!(tab.text, "本地未保存内容");
        assert_eq!(tab.status, DocStatus::Conflict);
        assert_eq!(tab.conflict, Some(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Agent 新内容");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn 后续进程传入路径会在现有窗口创建并激活标签() {
        let path = std::env::temp_dir().join(format!(
            "markdown-editor-single-instance-test-{}.md",
            std::process::id()
        ));
        std::fs::write(&path, "跨进程打开内容").unwrap();
        let mut app = app_with_two_tabs();

        let should_focus =
            app.apply_instance_request(single_instance::OpenRequest::new(vec![path.clone()]));

        assert!(should_focus);
        assert_eq!(app.tabs.len(), 3);
        assert_eq!(app.active_tab, 2);
        assert_eq!(app.path.as_ref(), Some(&path));
        assert_eq!(app.text, "跨进程打开内容");

        app.apply_instance_request(single_instance::OpenRequest::new(vec![path.clone()]));
        assert_eq!(app.tabs.len(), 3, "相同路径应切换标签，不能重复打开");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn 启动文件替换空白占位标签而不是留下未命名标签() {
        let path = std::env::temp_dir().join(format!(
            "markdown-editor-startup-file-test-{}.md",
            std::process::id()
        ));
        std::fs::write(&path, "启动文件内容").unwrap();
        let mut app = app_with_two_tabs();
        app.tabs.truncate(1);
        app.next_tab_id = 2;

        app.open_path(&path);

        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.path.as_ref(), Some(&path));
        assert_eq!(app.text, "启动文件内容");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn 无参数启动恢复上次文件标签和活动标签() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-window-restore-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let first = directory.join("first.md");
        let second = directory.join("second.md");
        std::fs::write(&first, "第一份").unwrap();
        std::fs::write(&second, "第二份").unwrap();
        let session = window_session::WindowSession::new(
            vec![first.clone(), second.clone()],
            Some(second.clone()),
        );
        let mut app = app_with_two_tabs();
        app.tabs = vec![DocumentTab::blank(1)];
        app.active_tab = 0;
        app.next_tab_id = 2;

        app.restore_window_session(session);

        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[0].path.as_ref(), Some(&first));
        assert_eq!(app.tabs[1].path.as_ref(), Some(&second));
        assert_eq!(app.active_tab, 1);
        assert!(app.tabs.iter().all(|tab| tab.path.is_some()));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn 冲突标签始终需要关闭确认() {
        let path = PathBuf::from("conflict.md");
        assert!(document_is_dirty(
            Some(&path),
            "相同内容",
            "相同内容".as_bytes(),
            &DocStatus::Conflict,
        ));
    }

    #[test]
    fn 长标签保留开头结尾并省略中间() {
        assert_eq!(shortened_tab_title("short.md"), "short.md");
        let shortened = shortened_tab_title("这是一个非常非常长的Markdown设计文档.md");
        assert!(shortened.contains('…'));
        assert!(shortened.ends_with("设计文档.md"));
        assert!(shortened.chars().count() <= 21);
    }

    #[test]
    fn 阅读目录保留标题层级并提取富文本标题() {
        let blocks = markdown::parse("# **总览** `v1`\n\n### [细节](details.md)\n\n正文");
        assert_eq!(
            reading_headings(&blocks),
            vec![(1, "总览 v1".to_string()), (3, "细节".to_string())]
        );
    }

    #[test]
    fn 长文档的小幅滚动也会立即触发同步() {
        let max_scroll = 50_000.0;
        let one_pixel = 1.0 / max_scroll;
        assert!(scroll_position_changed(0.4, 0.4 + one_pixel, max_scroll));
        assert!(!scroll_position_changed(
            0.4,
            0.4 + 0.25 / max_scroll,
            max_scroll
        ));
    }

    #[test]
    fn 源码字符位置与行内进度可以双向转换() {
        let text = "第一行\n第二行较长\n第三行";
        let second_line_middle = "第一行\n第二".chars().count();
        let position = source_position_from_char(text, second_line_middle);
        assert!((position - 1.4).abs() < 0.001);
        assert_eq!(
            char_index_from_source_position(text, position),
            second_line_middle
        );
        assert_eq!(
            char_index_from_source_position(text, 99.0),
            text.chars().count()
        );
    }

    #[test]
    fn 无本地修改时自动采用外部内容() {
        let mut tab = DocumentTab::from_file(
            1,
            PathBuf::from("agent.md"),
            "旧内容".to_string(),
            b"\xe6\x97\xa7\xe5\x86\x85\xe5\xae\xb9".to_vec(),
        );
        let result = apply_external_bytes(&mut tab, "Agent 新内容".as_bytes().to_vec()).unwrap();
        assert_eq!(result, ExternalChangeResult::Reloaded);
        assert_eq!(tab.text, "Agent 新内容");
        assert_eq!(tab.disk_snapshot, "Agent 新内容".as_bytes());
        assert_eq!(tab.status, DocStatus::Saved);
    }

    #[test]
    fn 有本地修改时保留内容并标记外部冲突() {
        let mut tab = DocumentTab::from_file(
            1,
            PathBuf::from("agent.md"),
            "原文".to_string(),
            "原文".as_bytes().to_vec(),
        );
        tab.text = "本地尚未保存".to_string();
        let result = apply_external_bytes(&mut tab, "Agent 修改".as_bytes().to_vec()).unwrap();
        assert_eq!(result, ExternalChangeResult::Conflict);
        assert_eq!(tab.text, "本地尚未保存");
        assert_eq!(tab.disk_snapshot, "原文".as_bytes());
        assert_eq!(tab.status, DocStatus::Conflict);
        assert_eq!(tab.conflict, Some(PathBuf::from("agent.md")));
    }

    #[test]
    fn 外部写入恰好等于编辑区内容时直接确认为已保存() {
        let mut tab = DocumentTab::from_file(
            1,
            PathBuf::from("agent.md"),
            "原文".to_string(),
            "原文".as_bytes().to_vec(),
        );
        tab.text = "共同的新内容".to_string();
        let result = apply_external_bytes(&mut tab, "共同的新内容".as_bytes().to_vec()).unwrap();
        assert_eq!(result, ExternalChangeResult::Reconciled);
        assert_eq!(tab.status, DocStatus::Saved);
        assert_eq!(tab.disk_snapshot, "共同的新内容".as_bytes());
    }

    #[test]
    fn 文件通知可在时间戳和大小不变时触发读取() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-external-notify-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("notes.md");
        std::fs::write(&path, "base").unwrap();
        let mut app = app_with_two_tabs();
        app.tabs[0] = DocumentTab::from_file(1, path.clone(), "base".to_string(), b"base".to_vec());
        app.observed_file_stamps
            .insert(path.clone(), io::file_stamp(&path).unwrap());
        std::fs::write(&path, "next").unwrap();

        assert!(matches!(
            app.probe_external_change(&path, b"base", 0.0, true),
            ExternalProbe::Waiting
        ));
        assert!(matches!(
            app.probe_external_change(&path, b"base", EXTERNAL_STABLE_DELAY, false),
            ExternalProbe::Stable(bytes) if bytes == b"next"
        ));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn 恢复带路径的草稿标签会重新注册文件通知() {
        let directory = std::env::temp_dir().join(format!(
            "markdown-editor-restored-watch-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("restored.md");
        std::fs::write(&path, "磁盘内容").unwrap();
        let mut app = app_with_two_tabs();
        app.external_watcher = ExternalFileWatcher::new();
        let session = io::DraftSession::new(
            7,
            vec![io::DraftTab::new(
                7,
                Some(path.clone()),
                "草稿内容".to_string(),
                "磁盘内容".as_bytes(),
            )],
        );

        app.restore_draft_session(session);

        assert!(
            app.external_watcher
                .as_ref()
                .is_some_and(|watcher| watcher.watched.contains(&path))
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn 预览搜索只把渲染后的可见文字视为匹配() {
        let mut app = app_with_two_tabs();
        app.tabs[0].document = markdown::parse_document("**可见文字** [链接](https://example.com)");
        app.search_query = "可见文字".to_string();
        assert!(app.preview_search_has_match());
        app.search_query = "https://example.com".to_string();
        assert!(!app.preview_search_has_match());
    }

    #[test]
    fn 带bom的磁盘快照不会被误判为本地修改() {
        let snapshot = [b"\xef\xbb\xbf".as_slice(), "正文".as_bytes()].concat();
        assert!(snapshot_matches_text(&snapshot, "正文"));
        assert!(!document_is_dirty(
            Some(&PathBuf::from("bom.md")),
            "正文",
            &snapshot,
            &DocStatus::Saved,
        ));
    }

    #[test]
    fn 拖入文件扩展名大小写不敏感且只接受文本类型() {
        assert!(has_supported_text_extension(Path::new("说明.MD")));
        assert!(has_supported_text_extension(Path::new("notes.MarkDown")));
        assert!(has_supported_text_extension(Path::new("草稿.TXT")));
        assert!(!has_supported_text_extension(Path::new("图片.png")));
        assert!(!has_supported_text_extension(Path::new("无扩展名")));
    }
}
