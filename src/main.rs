#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod document_core;
mod export;
#[cfg(target_os = "windows")]
mod file_association;
mod html_image;
mod io;
mod markdown;
mod parse_worker;
mod preview;
mod search;
mod search_worker;
mod single_instance;
mod storage;
mod theme;
mod window_close;
mod window_session;

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, mpsc, mpsc::Receiver};
use std::time::Duration;

use document_core::{DocumentState, DocumentStatus, Revision};
use eframe::egui;
use markdown::Block;
use notify::Watcher;
use parse_worker::{ParseRequest, ParseResult, ParseWorker};
use search_worker::{SearchRequest, SearchResult, SearchWorker};
use theme::{ThemePackage, ThemeSpec};

#[cfg(target_os = "macos")]
const PRIMARY_SHORTCUT: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const PRIMARY_SHORTCUT: &str = "Ctrl";

const EXTERNAL_POLL_INTERVAL: f64 = 0.35;
// File notifications wake the UI immediately. Keep a one-second fallback for
// platforms or editors that do not emit a usable notification, while avoiding
// a 350 ms repaint loop when the workspace is idle.
const EXTERNAL_FALLBACK_INTERVAL: f64 = 1.0;
const EXTERNAL_STABLE_DELAY: f64 = 0.45;
const DRAFT_AUTOSAVE_INTERVAL: f64 = 30.0;
const DRAFT_AUTOSAVE_RETRY_INTERVAL: f64 = 1.0;

fn main() -> eframe::Result {
    let launch = LaunchOptions::from_env();
    io::cleanup_stale_window_drafts();
    let restore_previous_window = launch.should_restore_window();
    let instance_requests = if !launch.uses_single_instance() {
        None
    } else {
        match single_instance::acquire(launch.open_paths.clone()) {
            single_instance::Acquisition::Primary(receiver) => Some(receiver),
            single_instance::Acquisition::Forwarded => return Ok(()),
            single_instance::Acquisition::Unavailable(error) => {
                rfd::MessageDialog::new()
                    .set_title("Markdown 编辑器")
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
            .with_title("Markdown 编辑器"),
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
}

impl LaunchOptions {
    fn from_env() -> Self {
        Self::from_args(std::env::args().skip(1))
    }

    fn from_args(arguments: impl IntoIterator<Item = String>) -> Self {
        let mut open_paths = Vec::new();
        let mut force_new_window = false;
        for argument in arguments {
            if argument == "--new-window" {
                force_new_window = true;
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
        }
    }

    fn uses_single_instance(&self) -> bool {
        !self.force_new_window
    }

    fn should_restore_window(&self) -> bool {
        self.open_paths.is_empty() && !self.force_new_window
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

type DocStatus = DocumentStatus;

#[derive(Clone)]
struct DocumentTab {
    id: u64,
    core: DocumentState,
    path: Option<PathBuf>,
    disk_snapshot: Vec<u8>,
    /// Bumped whenever `disk_snapshot` changes, so the dirty check below can be
    /// cached without comparing the whole document again.
    snapshot_epoch: u64,
    /// `(document_revision, snapshot_epoch, conflict, dirty)`.
    dirty_cache: std::cell::Cell<Option<(Revision, u64, bool, bool)>>,
    status_note: String,
    conflict: Option<PathBuf>,
    parse_requested_revision: Option<Revision>,
    draft_last_write: f64,
    last_edit_time: f64,
    /// Viewport-culling layout cache: rendered height per top-level block
    /// (`0.0` = never measured). Cleared when width/zoom change.
    preview_heights: Vec<f32>,
    preview_height_epoch: Option<(u32, u32)>,
}

// Reads (`tab.text` → `tab.source()`, parse state, status) deref to the core;
// DerefMut is deliberately absent: every in-place source mutation must name
// `core.source_mut()` at the call site so the required `mark_source_changed`
// pairing is greppable instead of silently bypassing the revision invariant.
impl Deref for DocumentTab {
    type Target = DocumentState;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
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
    fn new(ctx: egui::Context) -> Option<Self> {
        let (sender, receiver) = mpsc::channel();
        let repaint_ctx = ctx.clone();
        let watcher = notify::recommended_watcher(move |result| {
            let _ = sender.send(result);
            repaint_ctx.request_repaint();
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
        let mut core = DocumentState::from_source(String::new(), 0);
        core.status = DocStatus::Unsaved;
        Self {
            id,
            core,
            path: None,
            disk_snapshot: Vec::new(),
            snapshot_epoch: 0,
            dirty_cache: std::cell::Cell::new(None),
            status_note: String::new(),
            conflict: None,
            parse_requested_revision: None,
            draft_last_write: 0.0,
            last_edit_time: f64::INFINITY,
            preview_heights: Vec::new(),
            preview_height_epoch: None,
        }
    }

    fn from_file(id: u64, path: PathBuf, text: String, snapshot: Vec<u8>) -> Self {
        let mut core = DocumentState::from_source(text, 1);
        core.status = DocStatus::Saved;
        Self {
            id,
            core,
            path: Some(path),
            disk_snapshot: snapshot,
            snapshot_epoch: 0,
            dirty_cache: std::cell::Cell::new(None),
            status_note: String::new(),
            conflict: None,
            parse_requested_revision: None,
            draft_last_write: 0.0,
            last_edit_time: f64::INFINITY,
            preview_heights: Vec::new(),
            preview_height_epoch: None,
        }
    }

    /// Whether the tab holds content that is not on disk yet.
    ///
    /// The answer is a full byte comparison of the document, and the frame path
    /// asks for it repeatedly (tab bar, window title, autosave), so it is cached
    /// until the source revision, the disk snapshot, or the conflict flag
    /// changes.
    fn is_dirty(&self) -> bool {
        let conflict = matches!(self.status, DocStatus::Conflict);
        let key = (self.document_revision, self.snapshot_epoch, conflict);
        if let Some((revision, epoch, cached_conflict, dirty)) = self.dirty_cache.get()
            && (revision, epoch, cached_conflict) == key
        {
            return dirty;
        }
        let dirty = document_is_dirty(
            self.path.as_ref(),
            self.source(),
            &self.disk_snapshot,
            &self.status,
        );
        self.dirty_cache.set(Some((key.0, key.1, key.2, dirty)));
        dirty
    }

    /// Replace the disk snapshot and invalidate the cached dirty state.
    fn replace_disk_snapshot(&mut self, bytes: Vec<u8>) {
        self.disk_snapshot = bytes;
        self.snapshot_epoch = self.snapshot_epoch.wrapping_add(1);
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
    // Compare as bytes after removing the optional BOM. Decoding a copy of
    // the whole snapshot (the previous behaviour) allocated the document
    // size per tab per frame and showed up while typing in long documents.
    let body = snapshot
        .strip_prefix(b"\xEF\xBB\xBF".as_slice())
        .unwrap_or(snapshot);
    body.len() == text.len() && body == text.as_bytes()
}

fn apply_external_bytes(
    tab: &mut DocumentTab,
    bytes: Vec<u8>,
) -> Result<ExternalChangeResult, io::ReadError> {
    if bytes == tab.disk_snapshot {
        return Ok(ExternalChangeResult::Unchanged);
    }
    let disk_text = io::decode_markdown_bytes(&bytes)?;
    if disk_text == tab.source() {
        tab.replace_disk_snapshot(bytes);
        tab.core.status = DocStatus::Saved;
        tab.conflict = None;
        tab.status_note = "已同步外部保存".to_string();
        return Ok(ExternalChangeResult::Reconciled);
    }

    let has_local_changes = !snapshot_matches_text(&tab.disk_snapshot, tab.source())
        || matches!(tab.status, DocStatus::Conflict);
    if has_local_changes {
        tab.core.status = DocStatus::Conflict;
        tab.conflict = tab.path.clone();
        tab.status_note = "检测到外部修改，本地未保存内容已保留".to_string();
        return Ok(ExternalChangeResult::Conflict);
    }

    tab.core.set_source(disk_text);
    tab.replace_disk_snapshot(bytes);
    tab.core.reparse_current_source();
    tab.parse_requested_revision = None;
    tab.core.status = DocStatus::Saved;
    tab.conflict = None;
    tab.status_note = format!("已自动加载外部修改 {}", clock_time());
    Ok(ExternalChangeResult::Reloaded)
}

fn restore_draft_tab(draft: io::DraftTab) -> DocumentTab {
    // `load_draft_at` 已过滤非法 base64 快照；静默降级成空快照会把磁盘上
    // 存在的文件当成“新文件”，这里必须响亮地失败而不是给出错误答案。
    let stored_snapshot = draft.disk_snapshot().expect("草稿快照编码已在加载时校验");
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
    DocumentTab {
        id: draft.id,
        core: {
            let mut core = DocumentState::from_source(draft.text, 1);
            core.status = status;
            core
        },
        path: draft.path,
        disk_snapshot,
        snapshot_epoch: 0,
        dirty_cache: std::cell::Cell::new(None),
        status_note,
        conflict,
        parse_requested_revision: None,
        draft_last_write: 0.0,
        last_edit_time: f64::NEG_INFINITY,
        preview_heights: Vec::new(),
        preview_height_epoch: None,
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

const CHROME_FONT_SIZE: f32 = 14.0;
const CHROME_CONTROL_HEIGHT: f32 = 30.0;
const CHROME_BAR_HEIGHT: f32 = 36.0;

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
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 8.0, rect.bottom() - 1.0),
                egui::pos2(rect.right() - 8.0, rect.bottom() - 1.0),
            ],
            egui::Stroke::new(1.0, ui.visuals().strong_text_color()),
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

#[cfg(test)]
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

#[cfg(test)]
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

fn reading_toc(ui: &mut egui::Ui, headings: &[markdown::HeadingInfo]) -> Option<usize> {
    ui.add_space(12.0);
    ui.label(
        egui::RichText::new("章节目录")
            .size(15.0)
            .strong()
            .color(ui.visuals().strong_text_color()),
    );
    ui.add_space(8.0);
    // Keep the table of contents airy; the heading and indentation already
    // provide enough grouping without a hard divider.
    ui.add_space(8.0);

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
            for (index, heading) in headings.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_space((heading.level.saturating_sub(1) as f32) * 12.0);
                    let response = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(&heading.text)
                                    .size(13.5)
                                    .color(ui.visuals().text_color()),
                            )
                            .frame(false)
                            .truncate(),
                        )
                        .on_hover_text(&heading.text);
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
        egui::vec2(26.0, CHROME_CONTROL_HEIGHT),
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
    search_generation: u64,
    search_pending: Option<(u64, Revision, u64)>,
    search_focus_requested: bool,
    search_scroll_requested: bool,
    search_backwards: bool,
    /// The search input has keyboard focus this frame. While it does, search
    /// hits must not move the edit block: egui focus is last-wins, so a
    /// request_focus from the block editor would steal the query keystrokes.
    search_input_has_focus: bool,
    pending_close: Option<usize>,
    window_close_guard: window_close::CloseGuard,
    recovery: Option<io::DraftSession>,
    dark: bool,
    focus_mode: bool,
    typewriter_mode: bool,
    active_edit_block: Option<usize>,
    active_edit_range: Option<Range<usize>>,
    pending_edit_cursor: Option<usize>,
    edit_focus_requested: bool,
    show_status: bool,
    body_font_size: f32,
    theme_package: Option<ThemePackage>,
    auto_reload_external: bool,
    last_external_poll: f64,
    external_watcher: Option<ExternalFileWatcher>,
    observed_file_stamps: HashMap<PathBuf, io::FileStamp>,
    pending_external_changes: HashMap<PathBuf, PendingExternalChange>,
    instance_requests: Option<Receiver<single_instance::OpenRequest>>,
    draft_window_id: Option<u32>,
    persisted_window_session: Option<window_session::WindowSession>,
    window_session_initialized: bool,
    image_cache: preview::ImageCache,
    parse_worker: ParseWorker,
    search_worker: SearchWorker,
    /// Status-bar character/line totals, keyed by tab and revision. Recomputed
    /// only after an edit — the status bar repaints at least once per second
    /// through the external-poll timer and must not rescan 10 MB each frame.
    text_stats_cache: Option<(u64, Revision, usize, usize)>,
    /// Signature of the draft session written last, so an idle but dirty tab
    /// does not rewrite the same file every autosave interval.
    last_draft_signature: Option<Vec<(u64, Revision, u64)>>,
    /// Current search hit converted to byte offsets, keyed by tab, revision and
    /// hit. The character→byte walk is linear in the document and must not run
    /// on every frame while the search panel is open.
    search_byte_cache: Option<(u64, Revision, Range<usize>, Range<usize>)>,
    /// Last title sent to the viewport; used to avoid spamming
    /// `ViewportCommand::Title` every frame.
    last_window_title: Option<String>,
}

impl std::ops::Deref for MdEditorApp {
    type Target = DocumentTab;

    fn deref(&self) -> &Self::Target {
        // `tabs` 必须始终非空：帧循环里所有 `self.source()` / `self.path` 这类访问
        // 都经过这里，一旦为空就是索引越界 panic。不变量由 `MdEditorApp::new`
        // 与 `close_tab_now` 的兜底空标签维持，并由测试钉住。
        debug_assert!(!self.tabs.is_empty(), "tabs 必须至少保留一个标签页");
        &self.tabs[self.active_tab]
    }
}

impl std::ops::DerefMut for MdEditorApp {
    fn deref_mut(&mut self) -> &mut Self::Target {
        debug_assert!(!self.tabs.is_empty(), "tabs 必须至少保留一个标签页");
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
        let built_in_theme = ThemePackage::built_in_focused();
        let initial_body_font_size = theme_package
            .as_ref()
            .map(ThemePackage::recommended_body_font_size)
            .unwrap_or_else(|| built_in_theme.recommended_body_font_size());
        let initial_theme = theme_package
            .as_ref()
            .and_then(|t| t.spec(false).ok())
            .unwrap_or_else(|| ThemeSpec::fallback(false));
        apply_visuals(&cc.egui_ctx, false, &initial_theme);
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
            search_generation: 0,
            search_pending: None,
            search_focus_requested: false,
            search_scroll_requested: false,
            search_backwards: false,
            search_input_has_focus: false,
            pending_close: None,
            window_close_guard: window_close::CloseGuard::default(),
            recovery,
            dark: false,
            focus_mode: false,
            typewriter_mode: false,
            active_edit_block: None,
            active_edit_range: None,
            pending_edit_cursor: None,
            edit_focus_requested: false,
            show_status: true,
            body_font_size: initial_body_font_size,
            theme_package,
            auto_reload_external: true,
            last_external_poll: f64::NEG_INFINITY,
            external_watcher: ExternalFileWatcher::new(cc.egui_ctx.clone()),
            observed_file_stamps: HashMap::new(),
            pending_external_changes: HashMap::new(),
            instance_requests: None,
            draft_window_id,
            persisted_window_session: None,
            window_session_initialized: false,
            image_cache: preview::ImageCache::default(),
            parse_worker: ParseWorker::new(),
            search_worker: SearchWorker::new(),
            text_stats_cache: None,
            last_draft_signature: None,
            search_byte_cache: None,
            last_window_title: None,
        }
    }

    /// Queue parsing for one source revision without ever blocking the UI
    /// thread. The worker owns its immutable `Arc<str>` input and returns an
    /// immutable AST tagged with the same revision.
    fn submit_parse_for_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        if tab.is_parsed_current() {
            tab.parse_requested_revision = None;
            return;
        }
        let revision = tab.document_revision;
        if tab.parse_requested_revision == Some(revision) {
            return;
        }
        let request = ParseRequest {
            tab_id: tab.id,
            revision,
            // The source is cloned once at the hand-off boundary. The UI no
            // longer shares a mutable String with the worker.
            source: Arc::from(tab.source()),
        };
        tab.parse_requested_revision = Some(revision);
        let request_id = request.tab_id;
        let request_revision = request.revision;
        if self.parse_worker.submit(request).is_err() {
            // The worker is gone (spawn failed or it panicked). Fall back to
            // a synchronous parse so the tab converges; keeping the revision
            // marked as pending would spin the 16 ms repaint forever.
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == request_id)
                && tab.parse_requested_revision == Some(request_revision)
            {
                tab.parse_requested_revision = None;
                tab.core.reparse_current_source();
            }
            self.status_note = "Markdown 后台解析不可用，已改用同步解析".to_string();
        }
    }

    /// Mark a source mutation exactly once and enqueue its revision. Keeping
    /// this transition in one place prevents a frame-level change detector
    /// from incrementing the revision repeatedly while parsing is pending.
    fn mark_tab_source_changed(&mut self, index: usize, now: f64) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.core.mark_source_changed();
            tab.parse_requested_revision = None;
            tab.last_edit_time = now;
        }
        if index == self.active_tab {
            self.refresh_status();
        }
        self.submit_parse_for_tab(index);
    }

    /// Install only results that still describe the current tab revision.
    /// Results for closed tabs, old revisions, or mismatched source text are
    /// intentionally discarded.
    fn apply_parse_result(&mut self, result: ParseResult) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == result.tab_id) else {
            return false;
        };
        let tab = &mut self.tabs[index];
        if result.revision != tab.document_revision {
            return false;
        }
        if !tab.core.install_parsed(result.revision, result.document) {
            return false;
        }
        tab.parse_requested_revision = None;
        true
    }

    fn poll_parse_results(&mut self, ctx: &egui::Context) {
        let (results, disconnected) = self.parse_worker.drain();
        if !results.is_empty() {
            for result in results {
                self.apply_parse_result(result);
            }
            ctx.request_repaint();
        }
        if disconnected {
            self.recover_parse_worker();
            ctx.request_repaint();
        }
        if self
            .tabs
            .iter()
            .any(|tab| tab.parse_requested_revision.is_some())
        {
            // Keep polling while a worker result is outstanding, but do not
            // run a permanent repaint loop once all tabs are settled.
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    /// The parse worker died (panic or closed mailbox). Resolve every
    /// outstanding request with a synchronous parse so no tab keeps a pending
    /// revision — otherwise the repaint keep-alive above would spin forever.
    fn recover_parse_worker(&mut self) {
        let mut recovered = false;
        for tab in &mut self.tabs {
            if tab.parse_requested_revision.take().is_some() || !tab.is_parsed_current() {
                tab.core.reparse_current_source();
                recovered = true;
            }
        }
        if recovered {
            self.status_note = "Markdown 后台解析线程已退出，本次已同步完成解析".to_string();
        }
    }

    fn activate_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active_tab = index;
        self.active_edit_block = None;
        self.active_edit_range = None;
        self.pending_edit_cursor = None;
        self.edit_focus_requested = false;
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
            self.active_edit_block = None;
            self.active_edit_range = None;
            self.edit_focus_requested = true;
            return;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.push_tab(DocumentTab::blank(id));
        self.edit_focus_requested = true;
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
        self.search_pending = None;
    }

    fn refresh_search(&mut self) {
        if !self.has_open_document() {
            self.search_results = search::SearchResults::default();
            self.search_tab_id = None;
            self.search_pending = None;
            return;
        }
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        let tab_id = self.id;
        let revision = self.document_revision;
        self.search_tab_id = Some(tab_id);
        self.search_document_revision = revision;
        self.search_scroll_requested = false;
        self.search_backwards = false;
        self.search_results = search::SearchResults::default();
        if self.search_query.is_empty() {
            self.search_pending = None;
            return;
        }

        self.search_pending = Some((tab_id, revision, generation));
        let request = SearchRequest {
            tab_id,
            revision,
            generation,
            source: Arc::from(self.source()),
            query: self.search_query.clone(),
        };
        if self.search_worker.submit(request).is_err() {
            // Worker unavailable: search synchronously instead of leaving a
            // pending request that would keep the repaint loop alive.
            let results = search::SearchResults::new(self.source(), &self.search_query);
            self.search_results = results;
            self.search_pending = None;
            self.search_scroll_requested = true;
            self.status_note = "全文搜索后台任务不可用，已改用同步搜索".to_string();
        }
    }

    fn apply_search_result(&mut self, result: SearchResult) -> bool {
        if !self.search_open
            || result.tab_id != self.id
            || result.revision != self.document_revision
            || result.generation != self.search_generation
            || self.search_pending != Some((result.tab_id, result.revision, result.generation))
        {
            return false;
        }
        self.search_results = result.results;
        self.search_pending = None;
        self.search_scroll_requested = true;
        true
    }

    /// Byte offsets of the current search hit in the active document.
    ///
    /// Search results use character offsets while parser ranges use bytes; the
    /// conversion walks the text, so the answer is cached per tab, revision and
    /// hit instead of being recomputed on every frame.
    fn search_byte_range(&mut self) -> Option<Range<usize>> {
        let char_range = self.search_results.current_range()?;
        let tab_id = self.id;
        let revision = self.document_revision;
        if let Some((cached_tab, cached_revision, cached_chars, bytes)) = &self.search_byte_cache
            && *cached_tab == tab_id
            && *cached_revision == revision
            && *cached_chars == char_range
        {
            return Some(bytes.clone());
        }
        let bytes = preview::byte_range_for_chars(self.source(), &char_range);
        self.search_byte_cache = Some((tab_id, revision, char_range, bytes.clone()));
        Some(bytes)
    }

    fn poll_search_results(&mut self, ctx: &egui::Context) {
        let (results, disconnected) = self.search_worker.drain();
        let mut applied = false;
        for result in results {
            applied |= self.apply_search_result(result);
        }
        if disconnected && self.search_pending.take().is_some() && self.search_open {
            // The worker died with a request outstanding: compute the answer
            // synchronously so the 16 ms repaint keep-alive stops.
            let results = search::SearchResults::new(self.source(), &self.search_query);
            self.search_results = results;
            applied = true;
        }
        if applied {
            ctx.request_repaint();
        }
        if self.search_pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn refresh_search_if_needed(&mut self) {
        if self.search_open
            && (self.search_tab_id != Some(self.id)
                || self.search_document_revision != self.document_revision)
        {
            self.refresh_search();
        }
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
        self.tabs[self.active_tab].is_dirty()
    }

    fn is_tab_dirty(&self, index: usize) -> bool {
        self.tabs[index].is_dirty()
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
        let removed_tab_id = self.tabs[index].id;
        self.parse_worker.cancel_tab(removed_tab_id);
        self.search_worker.cancel_tab(removed_tab_id);
        self.image_cache.clear();
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
            self.workspace_empty = true;
            self.focus_mode = false;
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
            .unwrap_or_else(|| ThemeSpec::fallback(self.dark))
    }

    fn apply_current_theme(&self, ctx: &egui::Context) {
        apply_visuals(ctx, self.dark, &self.theme_spec());
    }

    fn refresh_status(&mut self) {
        self.core.status = match &self.path {
            Some(_) => {
                // BOM-aware comparison, consistent with `document_is_dirty`.
                if snapshot_matches_text(&self.disk_snapshot, self.source()) {
                    DocStatus::Saved
                } else {
                    DocStatus::Modified
                }
            }
            None => {
                if self.source().is_empty() {
                    DocStatus::Unsaved
                } else {
                    DocStatus::Modified
                }
            }
        };
    }

    /// Inspect one tab's on-disk file without cloning its snapshot. The tab is
    /// read through an immutable borrow while the bookkeeping maps are mutated
    /// separately, so polling a 10 MB document no longer copies the snapshot
    /// once per tab every frame.
    fn probe_external_change(
        observed_file_stamps: &mut HashMap<PathBuf, io::FileStamp>,
        pending_external_changes: &mut HashMap<PathBuf, PendingExternalChange>,
        tabs: &[DocumentTab],
        tab_index: usize,
        path: &Path,
        now: f64,
        force_read: bool,
    ) -> ExternalProbe {
        let snapshot: &[u8] = &tabs[tab_index].disk_snapshot;
        let stamp = match io::file_stamp(path) {
            Ok(stamp) => stamp,
            Err(error) => {
                observed_file_stamps.remove(path);
                pending_external_changes.remove(path);
                return ExternalProbe::Missing(describe_read_error(&error));
            }
        };

        let stamp_changed = observed_file_stamps.get(path) != Some(&stamp);
        if stamp_changed || force_read {
            observed_file_stamps.insert(path.to_path_buf(), stamp.clone());
            match io::read_snapshot_checked(path) {
                Ok(bytes) if bytes.as_slice() == snapshot => {
                    pending_external_changes.remove(path);
                }
                Ok(bytes) => {
                    let changed = pending_external_changes
                        .get(path)
                        .is_none_or(|pending| pending.stamp != stamp || pending.bytes != bytes);
                    if changed {
                        pending_external_changes.insert(
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

        let is_stable = pending_external_changes.get(path).is_some_and(|pending| {
            pending.stamp == stamp && now - pending.first_seen >= EXTERNAL_STABLE_DELAY
        });
        if is_stable && let Some(pending) = pending_external_changes.remove(path) {
            return ExternalProbe::Stable(pending.bytes);
        }
        ExternalProbe::Waiting
    }

    fn poll_external_changes(&mut self, ctx: &egui::Context, now: f64) {
        if !self.auto_reload_external {
            return;
        }
        let changed_paths = self
            .external_watcher
            .as_ref()
            .map(ExternalFileWatcher::drain_changed_paths)
            .unwrap_or_default();
        // Without a file-backed tab there is nothing to re-read, so no timer is
        // scheduled: an idle untitled window stays asleep instead of waking up
        // several times a second.
        if !self.tabs.iter().any(|tab| tab.path.is_some()) && changed_paths.is_empty() {
            return;
        }
        let has_watched_paths = self
            .external_watcher
            .as_ref()
            .is_some_and(|watcher| !watcher.watched.is_empty());
        let poll_interval = if has_watched_paths
            && changed_paths.is_empty()
            && self.pending_external_changes.is_empty()
        {
            EXTERNAL_FALLBACK_INTERVAL
        } else {
            EXTERNAL_POLL_INTERVAL
        };
        ctx.request_repaint_after(Duration::from_secs_f64(poll_interval));
        if now - self.last_external_poll < poll_interval && changed_paths.is_empty() {
            return;
        }
        self.last_external_poll = now;
        let mut draft_state_changed = false;

        let active_path = self.path.clone();
        if let Some(path) = active_path {
            let active_index = self.active_tab;
            let force_read = changed_paths.contains(&path);
            match Self::probe_external_change(
                &mut self.observed_file_stamps,
                &mut self.pending_external_changes,
                &self.tabs,
                active_index,
                &path,
                now,
                force_read,
            ) {
                ExternalProbe::Stable(bytes) => {
                    match apply_external_bytes(&mut self.tabs[active_index], bytes) {
                        Ok(ExternalChangeResult::Unchanged) => {}
                        Ok(ExternalChangeResult::Reloaded) => {
                            self.active_edit_block = None;
                            self.active_edit_range = None;
                            self.edit_focus_requested = false;
                            draft_state_changed = true;
                        }
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
            let force_read = changed_paths.contains(&path);
            match Self::probe_external_change(
                &mut self.observed_file_stamps,
                &mut self.pending_external_changes,
                &self.tabs,
                index,
                &path,
                now,
                force_read,
            ) {
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

    /// Tabs that hold content worth persisting as a draft, in tab order.
    fn draft_candidates(&self) -> Vec<usize> {
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                (tab.is_dirty() && (!tab.source().is_empty() || tab.path.is_some()))
                    .then_some(index)
            })
            .collect()
    }

    /// Identity of the current draft contents. Re-writing the session when this
    /// is unchanged would serialize the same documents again for nothing.
    fn draft_signature(&self, candidates: &[usize]) -> Vec<(u64, Revision, u64)> {
        candidates
            .iter()
            .map(|&index| {
                let tab = &self.tabs[index];
                (tab.id, tab.document_revision, tab.snapshot_epoch)
            })
            .collect()
    }

    fn draft_session(&self) -> Option<io::DraftSession> {
        let drafts = self
            .tabs
            .iter()
            .filter(|tab| tab.is_dirty() && (!tab.source().is_empty() || tab.path.is_some()))
            .map(|tab| {
                io::DraftTab::new(
                    tab.id,
                    tab.path.clone(),
                    tab.source().to_string(),
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

    fn persist_draft_session(&mut self) -> std::io::Result<()> {
        let signature = self.draft_signature(&self.draft_candidates());
        let result = if let Some(session) = self.draft_session() {
            io::save_draft_for_window(self.draft_window_id, &session)
        } else {
            io::clear_draft_for_window(self.draft_window_id);
            Ok(())
        };
        if result.is_ok() {
            self.last_draft_signature = Some(signature);
        }
        result
    }

    fn autosave_draft(&mut self, ctx: &egui::Context, now: f64) {
        let candidates = self.draft_candidates();
        if candidates.is_empty() {
            self.last_draft_signature = None;
            return;
        }
        let signature = self.draft_signature(&candidates);
        if self.last_draft_signature.as_ref() == Some(&signature) {
            // Nothing changed since the last successful write, so rewriting the
            // session would serialize the same documents again and no timer has
            // to be scheduled to keep doing it.
            return;
        }
        let idle_at = candidates
            .iter()
            .map(|&index| self.tabs[index].last_edit_time + DRAFT_AUTOSAVE_INTERVAL)
            .fold(f64::NEG_INFINITY, f64::max);
        let write_at = candidates
            .iter()
            .map(|&index| self.tabs[index].draft_last_write + DRAFT_AUTOSAVE_INTERVAL)
            .fold(f64::INFINITY, f64::min);
        let mut next_save_at = idle_at.max(write_at);
        if now >= next_save_at
            && let Some(session) = self.draft_session()
        {
            match io::save_draft_for_window(self.draft_window_id, &session) {
                Ok(()) => {
                    self.last_draft_signature = Some(signature);
                    for index in candidates {
                        self.tabs[index].draft_last_write = now;
                    }
                    next_save_at = now + DRAFT_AUTOSAVE_INTERVAL;
                }
                Err(error) => {
                    self.status_note = format!("草稿会话保存失败：{error}");
                    next_save_at = now + DRAFT_AUTOSAVE_RETRY_INTERVAL;
                }
            }
        }
        if next_save_at.is_finite() {
            ctx.request_repaint_after(Duration::from_secs_f64((next_save_at - now).max(0.01)));
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
        } else if self.active_edit_block.is_some()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.active_edit_block = None;
            self.active_edit_range = None;
            self.edit_focus_requested = false;
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
        // `COMMAND` is the cross-platform Ctrl/Cmd alias. Keep an explicit
        // Ctrl fallback as well: some Windows input backends expose only the
        // physical `ctrl` modifier, which otherwise makes the advertised
        // Ctrl+E shortcut appear to do nothing.
        let edit_shortcut = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::E)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::E)
        });
        if has_open_document && edit_shortcut {
            self.active_edit_block = Some(self.active_edit_block.unwrap_or(0));
            self.edit_focus_requested = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F8)) {
            self.focus_mode = !self.focus_mode;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F9)) {
            self.typewriter_mode = !self.typewriter_mode;
        }
        // Font-size zoom mirrors the browser convention: Ctrl/Cmd +"-" grows
        // or shrinks the body text and Ctrl/Cmd+0 restores the theme default.
        // Numpad Plus and the shifted `+`/`_` glyphs are accepted too so the
        // shortcut works without hunting for a specific physical key.
        let zoom_in = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Plus)
                || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Equals)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::Plus)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::Equals)
        });
        let zoom_out = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Minus)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::Minus)
        });
        let zoom_reset = ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num0)
                || i.consume_key(egui::Modifiers::CTRL, egui::Key::Num0)
        });
        if zoom_in {
            self.body_font_size = (self.body_font_size + 0.5).min(22.0);
        } else if zoom_out {
            self.body_font_size = (self.body_font_size - 0.5).max(12.0);
        } else if zoom_reset {
            self.body_font_size = self.default_body_font_size();
        }
        // Browser/Typora convention: Ctrl+滚轮 also zooms the body text. The
        // wheel delta is consumed so the document does not scroll while
        // zooming.
        let wheel_zoom = ctx.input_mut(|input| {
            let delta = input.smooth_scroll_delta.y;
            if input.modifiers.command && delta != 0.0 {
                input.smooth_scroll_delta = egui::Vec2::ZERO;
                Some((delta / 24.0).clamp(-1.0, 1.0))
            } else {
                None
            }
        });
        if let Some(delta) = wheel_zoom {
            self.body_font_size = (self.body_font_size + delta * 1.0).clamp(12.0, 22.0);
        }
    }

    /// The body font size the current theme recommends, used as the zoom
    /// baseline and by “reset default size”.
    fn default_body_font_size(&self) -> f32 {
        self.theme_package.as_ref().map_or_else(
            || ThemePackage::built_in_focused().recommended_body_font_size(),
            |package| package.recommended_body_font_size(),
        )
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
            if path.is_file() && has_supported_text_extension(&path) && self.open_path(&path) {
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

    fn open_path(&mut self, path: &PathBuf) -> bool {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.path.as_ref() == Some(path))
        {
            self.activate_tab(index);
            self.active_edit_block = Some(0);
            self.edit_focus_requested = true;
            self.status_note = format!("已切换到 {}", path.display());
            return true;
        }
        match io::read_markdown_snapshot(path) {
            Ok((text, snapshot)) => {
                let replace_blank = self.tabs.len() == 1
                    && self.tabs[0].path.is_none()
                    && self.tabs[0].source().is_empty()
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
                self.active_edit_block = Some(0);
                self.active_edit_range = None;
                self.pending_edit_cursor = None;
                self.edit_focus_requested = true;
                self.status_note = format!("已打开 {}", path.display());
                true
            }
            Err(e) => {
                // A failed open must not rewrite the active document's save
                // status; it is informational only.
                self.status_note = format!("无法读取文件：{}", describe_read_error(&e));
                false
            }
        }
    }

    fn save(&mut self) {
        if self.path.is_none() {
            self.save_as();
            return;
        }
        let path = self.path.clone().expect("已检查文档路径");
        match io::save_with_conflict_check(&path, self.source(), &self.disk_snapshot) {
            Ok(bytes) => {
                self.replace_disk_snapshot(bytes);
                self.path = Some(path);
                self.core.status = DocStatus::Saved;
                self.status_note = format!("已保存 {}", clock_time());
                if let Err(error) = self.persist_draft_session() {
                    self.status_note = format!("文档已保存；草稿会话更新失败：{error}");
                }
            }
            Err(io::SaveError::ExternalModified) => {
                self.conflict = Some(path);
                self.core.status = DocStatus::Conflict;
            }
            Err(io::SaveError::TooLarge { size, limit }) => {
                self.core.status = DocStatus::SaveFailed(format!(
                    "保存失败：文件大小 {size} 字节，超过 {limit} 字节限制"
                ));
            }
            Err(io::SaveError::Io(e)) => {
                self.core.status = DocStatus::SaveFailed(format!("保存失败：{}", e));
            }
        }
    }

    fn save_as(&mut self) -> bool {
        let Some(path) = pick_save_path() else {
            return false;
        };
        match io::save_overwrite(&path, self.source(), None) {
            Ok(bytes) => {
                // Moving to a new file: stop watching (and stop polling) the
                // previous location so stale stamps cannot fire later.
                let previous = self.path.clone();
                self.replace_disk_snapshot(bytes);
                self.path = Some(path.clone());
                self.watch_external_path(&path);
                if let Some(previous) = previous.filter(|previous| *previous != path) {
                    self.unwatch_external_path(&previous);
                    self.observed_file_stamps.remove(&previous);
                    self.pending_external_changes.remove(&previous);
                }
                self.core.status = DocStatus::Saved;
                self.conflict = None;
                self.status_note = format!("已保存 {}", clock_time());
                if let Err(error) = self.persist_draft_session() {
                    self.status_note = format!("文档已保存；草稿会话更新失败：{error}");
                }
                true
            }
            Err(e) => {
                self.core.status = DocStatus::SaveFailed(format!("保存失败：{}", e));
                false
            }
        }
    }

    fn resolve_overwrite(&mut self) {
        if let Some(path) = self.conflict.take() {
            match io::save_overwrite(&path, self.source(), Some(&self.disk_snapshot)) {
                Ok(bytes) => {
                    self.replace_disk_snapshot(bytes);
                    self.path = Some(path);
                    self.core.status = DocStatus::Saved;
                    self.status_note = "已覆盖保存".to_string();
                    if let Err(error) = self.persist_draft_session() {
                        self.status_note = format!("文档已保存；草稿会话更新失败：{error}");
                    }
                    self.finish_pending_close_if_saved();
                }
                Err(e) => self.core.status = DocStatus::SaveFailed(format!("保存失败：{}", e)),
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
            match io::read_markdown_snapshot(&path) {
                Ok((text, snapshot)) => {
                    self.core.set_source(text);
                    self.path = Some(path.clone());
                    self.replace_disk_snapshot(snapshot);
                    self.core.reparse_current_source();
                    self.parse_requested_revision = None;
                    self.active_edit_block = None;
                    self.active_edit_range = None;
                    self.edit_focus_requested = false;
                    self.core.status = DocStatus::Saved;
                    self.status_note = "已重新载入磁盘内容".to_string();
                    if let Err(error) = self.persist_draft_session() {
                        self.status_note = format!("磁盘内容已载入；草稿会话更新失败：{error}");
                    }
                    self.finish_pending_close_if_saved();
                }
                Err(e) => {
                    self.status_note = format!("无法读取文件：{}", describe_read_error(&e));
                }
            }
        }
    }

    fn export_html(&mut self) {
        let Some(document) = self
            .core
            .snapshot(self.id)
            .map(|snapshot| snapshot.document)
        else {
            self.status_note = "正在解析，完成后才能导出".to_string();
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("HTML", &["html"])
            .set_file_name("导出.html")
            .save_file()
        else {
            return;
        };
        let title = self.export_title();
        let options = self.export_options(&title);
        match export::export_html(&path, &document, options) {
            Ok(()) => self.status_note = format!("已导出 HTML：{}", path.display()),
            Err(e) => self.core.status = DocStatus::SaveFailed(format!("导出失败：{}", e)),
        }
    }

    fn export_pdf(&mut self) {
        let Some(document) = self
            .core
            .snapshot(self.id)
            .map(|snapshot| snapshot.document)
        else {
            self.status_note = "正在解析，完成后才能导出".to_string();
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .set_file_name("导出.pdf")
            .save_file()
        else {
            return;
        };
        let title = self.export_title();
        let options = self.export_options(&title);
        match export::export_pdf(&path, &document, options) {
            Ok(()) => self.status_note = format!("已导出 PDF：{}", path.display()),
            Err(e) => self.core.status = DocStatus::SaveFailed(format!("导出失败：{}", e)),
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
            .unwrap_or(theme::BUILT_IN_FOCUS_CSS);
        let default_size = package
            .map(ThemePackage::recommended_body_font_size)
            .unwrap_or_else(|| ThemePackage::built_in_focused().recommended_body_font_size());
        export::ExportOptions {
            title,
            theme_css,
            base_directory: self.path.as_deref().and_then(Path::parent),
            body_font_size: ((self.body_font_size - default_size).abs() > 0.01)
                .then_some(self.body_font_size),
        }
    }

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let has_open_document = self.has_open_document();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), CHROME_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
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
                    let has_current_snapshot = has_open_document && self.is_parsed_current();
                    if ui
                        .add_enabled(has_current_snapshot, egui::Button::new("复制渲染内容"))
                        .clicked()
                    {
                        ui.close();
                        if let Some(snapshot) = self.core.snapshot(self.id) {
                            ui.ctx()
                                .copy_text(markdown::plain_text(snapshot.document.blocks()));
                        }
                    }
                    if ui
                        .add_enabled(has_current_snapshot, egui::Button::new("复制 HTML"))
                        .clicked()
                    {
                        ui.close();
                        if let Some(snapshot) = self.core.snapshot(self.id) {
                            ui.ctx().copy_text(export::render_html(&snapshot.document));
                        }
                    }
                });
                ui.menu_button(egui::RichText::new("视图").size(CHROME_FONT_SIZE), |ui| {
                    ui.set_min_width(230.0);
                    ui.label(egui::RichText::new("编辑与正文字号").weak().size(12.0))
                        .on_hover_text("快捷键 Ctrl+= 放大 · Ctrl+- 缩小 · Ctrl+0 重置");
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
                        self.body_font_size = self.default_body_font_size();
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
                        self.dark = !self.dark;
                        self.apply_current_theme(ui.ctx());
                        ui.close();
                    }
                });
                ui.add_space(8.0);
                let mut switch_to = None;
                let mut close_tab = None;
                let mut create_tab = false;
                let tabs_width = ui.available_width();
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
            },
        );
    }

    fn text_stats(&mut self) -> (usize, usize) {
        let tab_id = self.id;
        let revision = self.document_revision;
        if let Some((cached_tab, cached_revision, chars, lines)) = self.text_stats_cache
            && cached_tab == tab_id
            && cached_revision == revision
        {
            return (chars, lines);
        }
        let chars = self.source().chars().count();
        let lines = self.source().lines().count();
        self.text_stats_cache = Some((tab_id, revision, chars, lines));
        (chars, lines)
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
            if self.workspace_empty {
                ui.label(egui::RichText::new("没有打开的文档").weak());
                ui.add_space(12.0);
                ui.label(egui::RichText::new("可新建、打开或拖入 Markdown 文件").weak());
                if !self.status_note.is_empty() {
                    ui.add_space(12.0);
                    ui.label(&self.status_note);
                } else if let DocStatus::SaveFailed(message) = &self.status {
                    ui.add_space(12.0);
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
            ui.add_space(12.0);
            let (chars, lines) = self.text_stats();
            ui.label(format!("{chars} 字符 / {lines} 行"));
            if (self.source().len() as u64) > io::MAX_FILE_SIZE {
                ui.add_space(12.0);
                ui.colored_label(egui::Color32::from_rgb(0xc0, 0x39, 0x2b), "超过 10 MB 限制");
            }
            if !self.status_note.is_empty() {
                ui.add_space(12.0);
                ui.label(&self.status_note);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = if !self.document.blocks().is_empty()
                    && self.document.block_ranges().len() != self.document.blocks().len()
                {
                    "全文编辑 · 当前 Markdown 结构暂不支持逐块编辑 · F8 专注 · F9 打字机"
                        .to_string()
                } else {
                    match self.active_edit_block {
                        Some(index) => {
                            format!(
                                "正在编辑第 {} 段 · Esc 收起 · F8 专注 · F9 打字机",
                                index + 1
                            )
                        }
                        None => "点击段落开始编辑 · F8 专注 · F9 打字机".to_string(),
                    }
                };
                ui.label(egui::RichText::new(hint).weak().size(11.0));
            });
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
                let count = self.search_results.position().map_or_else(
                    || "无结果".to_string(),
                    |(current, total)| format!("{current} / {total}"),
                );
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
                self.search_input_has_focus = response.has_focus();
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
        ui.add_space(top_space);
        ui.vertical_centered(|ui| {
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
            ui.add_space(24.0);
            let mut create = false;
            let mut open = false;
            ui.horizontal_centered(|ui| {
                create = ui
                    .add_sized([116.0, 34.0], egui::Button::new("新建文档"))
                    .clicked();
                ui.add_space(8.0);
                let accent = ui.visuals().selection.bg_fill;
                let accent_text = ui.visuals().selection.stroke.color;
                open = ui
                    .add_sized(
                        [116.0, 34.0],
                        egui::Button::new(egui::RichText::new("打开文件…").color(accent_text))
                            .fill(accent),
                    )
                    .clicked();
            });
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new("也可以将 .md、.markdown 或 .txt 文件拖到这里")
                    .size(12.0)
                    .weak(),
            );
            if create {
                self.new_tab();
            } else if open {
                self.open_file();
            }
        });
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
        // `activate_tab` clears the transient editor state. Restore the
        // live-preview editor after selecting the persisted active tab so a
        // restored window is immediately ready for typing as well.
        if !self.workspace_empty {
            self.active_edit_block = Some(0);
            self.active_edit_range = None;
            self.pending_edit_cursor = None;
            self.edit_focus_requested = true;
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
        self.active_edit_block = Some(0);
        self.active_edit_range = None;
        self.pending_edit_cursor = None;
        self.edit_focus_requested = true;
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
            ui.add_space(8.0);
            ui.label(format!(
                "有 {} 个标签包含未保存的修改。关闭窗口前要保存吗？",
                documents.len()
            ));
            ui.add_space(12.0);
            egui::Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .corner_radius(8.0)
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
    /// Reflect the active document in the OS window title (taskbar and
    /// Alt+Tab), with a dot marking unsaved changes.
    fn sync_window_title(&mut self, ctx: &egui::Context) {
        let title = if self.workspace_empty {
            "Markdown 编辑器".to_string()
        } else {
            let dirty = self.is_active_dirty();
            let label = document_label(self.id, self.path.as_ref(), dirty);
            format!("{label} - Markdown 编辑器")
        };
        if self.last_window_title.as_deref() != Some(title.as_str()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_window_title = Some(title);
        }
    }
}

impl eframe::App for MdEditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        let mut heading_target = None;

        let dropped_paths = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        self.handle_instance_requests(&ctx);
        self.open_dropped_paths(dropped_paths);

        self.poll_parse_results(&ctx);
        self.poll_search_results(&ctx);
        self.poll_external_changes(&ctx, now);

        self.autosave_draft(&ctx, now);
        self.handle_window_close_request(&ctx);
        self.handle_shortcuts(&ctx);
        self.refresh_search_if_needed();
        self.sync_window_title(&ctx);

        if !self.focus_mode {
            egui::Panel::top("menu_panel")
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(12, 1))
                        .stroke(egui::Stroke::NONE),
                )
                .show(ui, |ui| {
                    self.title_bar(ui);
                });
        }

        if self.search_open {
            egui::Panel::top("search_panel")
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(12, 3))
                        .stroke(egui::Stroke::NONE),
                )
                .show(ui, |ui| self.search_bar(ui));
        }

        if self.show_status
            && !self.focus_mode
            && (!self.workspace_empty
                || !self.status_note.is_empty()
                || matches!(self.status, DocStatus::SaveFailed(_)))
        {
            egui::Panel::bottom("status_panel")
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(16, 3))
                        .stroke(egui::Stroke::NONE),
                )
                .show(ui, |ui| self.status_bar(ui));
        }

        if self.workspace_empty {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(ui.visuals().window_fill))
                .show(ui, |ui| self.empty_workspace(ui));
            self.recovery_window(&ctx);
            self.persist_window_session_if_changed();
            return;
        }

        let doc_theme = self.theme_spec();
        let editor_fill = doc_theme.editor_canvas;
        let search_byte_range = self.search_open.then(|| self.search_byte_range()).flatten();
        let scroll_to_search = std::mem::take(&mut self.search_scroll_requested);

        {
            let active_index = self.active_tab;
            let active_tab_id = self.tabs[active_index].id;
            let body_font_size = self.body_font_size;
            // A frame takes at most one immutable snapshot per tab: it is `Some`
            // only while the parse still matches the source. Either way the last
            // completed parse is what gets rendered, and only the block owned by
            // the editor can differ from it.
            let current_snapshot = self.tabs[active_index].core.snapshot(active_tab_id);
            let parsed_current = current_snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.tab_id == active_tab_id
                    && snapshot.revision == self.tabs[active_index].document_revision
            });
            let parsed_document: Arc<markdown::ParsedDocument> = match current_snapshot {
                Some(snapshot) => snapshot.document,
                None => self.tabs[active_index].core.document(),
            };
            let headings = parsed_document.headings();
            if !self.focus_mode {
                egui::Panel::left("reading_toc_panel")
                    .resizable(false)
                    .show_separator_line(false)
                    .exact_size(228.0)
                    .frame(
                        egui::Frame::new()
                            .fill(ui.visuals().panel_fill)
                            .inner_margin(egui::Margin::symmetric(16, 0)),
                    )
                    .show(ui, |ui| {
                        heading_target = reading_toc(ui, headings);
                    });
            }
            // Search hits address the document by block, and block indices come
            // from the parse: do not move the editor while one is pending.
            let effective_search_range = parsed_current
                .then_some(search_byte_range.clone())
                .flatten();
            if let Some(search) = effective_search_range.as_ref()
                && let Some(index) =
                    preview::block_index_for_search(parsed_document.block_ranges(), search)
                && self.active_edit_block != Some(index)
                && !self.search_input_has_focus
            {
                // 搜索输入框获得焦点的帧不抢焦点：egui 的 request_focus 是
                // 后写覆盖，正文编辑器抢焦点会把查询按键打进文档。
                self.active_edit_block = Some(index);
                self.active_edit_range = None;
                self.edit_focus_requested = true;
            }
            let active_edit_block = self.active_edit_block;
            let active_edit_range = self.active_edit_range.clone();
            let pending_edit_cursor = self.pending_edit_cursor.take();
            let edit_focus_requested = self.edit_focus_requested;
            let mut editor_output = preview::BlockEditorOutput::default();
            let mut had_blocks = false;
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(editor_fill)
                        .inner_margin(egui::Margin {
                            left: 22,
                            right: 0,
                            top: 0,
                            bottom: 0,
                        }),
                )
                .show(ui, |ui| {
                    let active_tab = &mut self.tabs[active_index];
                    // `text` lives behind `Deref` to the core state, so borrow
                    // the concrete fields at the destructuring level to keep
                    // disjoint-field borrows disjoint for the borrow checker.
                    let DocumentTab {
                        core,
                        path,
                        preview_heights,
                        preview_height_epoch,
                        ..
                    } = active_tab;
                    let text = core.source_mut();
                    let blocks: &[Block] = parsed_document.blocks();
                    had_blocks = !blocks.is_empty();
                    let block_ranges: &[Range<usize>] = parsed_document.block_ranges();
                    let image_base_directory = path
                        .clone()
                        .and_then(|path| path.parent().map(Path::to_path_buf));
                    if !parsed_current {
                        ui.colored_label(ui.visuals().weak_text_color(), "正在解析…");
                    }
                    // Reset the culling layout cache whenever the column
                    // width (rounded) or the body zoom changed; measured
                    // heights from another geometry would misplace spacers.
                    let virtualize = blocks.len() >= preview::VIRTUALIZE_MIN_BLOCKS;
                    if virtualize {
                        let epoch = (
                            (ui.available_width().min(doc_theme.content_width) / 4.0).round()
                                as u32,
                            (body_font_size * 10.0) as u32,
                        );
                        if *preview_height_epoch != Some(epoch) {
                            preview_heights.clear();
                            *preview_height_epoch = Some(epoch);
                        }
                        preview_heights.resize(blocks.len(), 0.0);
                    }
                    editor_output = show_hybrid_editor(
                        ui,
                        active_tab_id,
                        blocks,
                        block_ranges,
                        text,
                        body_font_size,
                        &doc_theme,
                        active_edit_block,
                        active_edit_range,
                        pending_edit_cursor,
                        edit_focus_requested,
                        effective_search_range.clone(),
                        scroll_to_search,
                        self.typewriter_mode,
                        self.focus_mode && active_edit_block.is_some(),
                        !parsed_current,
                        heading_target,
                        &mut preview::PreviewImages {
                            base_directory: image_base_directory.as_deref(),
                            cache: &mut self.image_cache,
                        },
                        virtualize.then_some(preview_heights),
                    );
                });
            if editor_output.changed {
                self.mark_tab_source_changed(active_index, now);
            }
            if editor_output.viewport_unsettled {
                // A skipped block's measured height differed from its estimate;
                // repaint once so the spacers settle and the scrollbar is exact.
                ctx.request_repaint();
            }
            self.edit_focus_requested = false;
            self.active_edit_range = editor_output.edited_range;
            if editor_output.changed && !had_blocks {
                self.active_edit_block = Some(0);
                self.edit_focus_requested = true;
            }
            // Block indices belong to the last completed parse; while a newer
            // one is pending they must not move the editor or its caret.
            if let Some(clicked) =
                clicked_block_accepted(parsed_current, editor_output.clicked_block)
            {
                self.active_edit_block = Some(clicked);
                self.active_edit_range = None;
                self.pending_edit_cursor = editor_output.clicked_cursor;
                self.edit_focus_requested = true;
            }
        }
        self.conflict_window(&ctx);
        self.recovery_window(&ctx);
        self.close_tab_window(&ctx);
        self.window_close_window(&ctx);
        self.persist_window_session_if_changed();
        if self
            .tabs
            .iter()
            .any(|tab| tab.parse_requested_revision.is_some())
            || self.search_pending.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
}

fn document_scroll_id(scope: &'static str, tab_id: u64) -> egui::Id {
    egui::Id::new((scope, tab_id))
}

/// Accept a preview click only while the parse still matches the source.
///
/// While a newer parse is pending, block indices belong to the previous parse
/// and must not move the editor or its caret. `preview.rs` keeps reporting the
/// click so the frame-level harness can assert on it; this gate is where the
/// stale index is dropped.
fn clicked_block_accepted(parsed_current: bool, clicked_block: Option<usize>) -> Option<usize> {
    clicked_block.filter(|_| parsed_current)
}

#[allow(clippy::too_many_arguments)]
fn show_hybrid_editor(
    ui: &mut egui::Ui,
    tab_id: u64,
    blocks: &[Block],
    block_ranges: &[Range<usize>],
    text: &mut String,
    body_font_size: f32,
    theme: &ThemeSpec,
    active_block: Option<usize>,
    active_range: Option<Range<usize>>,
    initial_cursor: Option<usize>,
    request_focus: bool,
    search_range: Option<Range<usize>>,
    scroll_to_search: bool,
    typewriter_mode: bool,
    dim_inactive: bool,
    stale_blocks: bool,
    heading_target: Option<usize>,
    images: &mut preview::PreviewImages<'_>,
    heights: Option<&mut Vec<f32>>,
) -> preview::BlockEditorOutput {
    egui::ScrollArea::vertical()
        .id_salt(document_scroll_id("editor_scroll_hybrid", tab_id))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, visible| {
            // Long documents render only the band around the visible slice.
            // `content_origin` maps egui's content-relative viewport band into
            // the same global coordinates the block walk uses for its cursor.
            let content_origin = ui.cursor().min.y;
            let mut viewport = heights.map(|heights| preview::PreviewViewport {
                band: (content_origin + visible.min.y)..(content_origin + visible.max.y),
                heights,
                margin: ((visible.max.y - visible.min.y) * 1.5).max(600.0),
            });
            // Keep the document column centered and stable. The active block
            // supplies the editing affordance while the rest stays rendered.
            let available_width = ui.available_width();
            let width = available_width.min(theme.content_width);
            ui.horizontal(|ui| {
                ui.add_space(((available_width - width) / 2.0).max(24.0));
                ui.allocate_ui_with_layout(
                    egui::vec2(width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.add_space(56.0);
                        ui.push_id(("hybrid_document", tab_id), |ui| {
                            preview::show_preview_with_block_editor_and_search(
                                ui,
                                blocks,
                                block_ranges,
                                text,
                                body_font_size,
                                theme,
                                active_block,
                                active_range,
                                initial_cursor,
                                request_focus,
                                search_range,
                                scroll_to_search,
                                typewriter_mode,
                                dim_inactive,
                                stale_blocks,
                                heading_target,
                                images,
                                viewport.as_mut(),
                            )
                        })
                        .inner
                    },
                )
            })
            .inner
        })
        .inner
        .inner
}

fn setup_fonts(ctx: &egui::Context) {
    export::install_app_fonts(ctx);
    ctx.all_styles_mut(|style| {
        // 字阶与间距对齐设计系统：正文 16，small 14，间距落在 4px 网格上。
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
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
        );
        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(4);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(4);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(4);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(4);
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
    // Keep the chrome quiet: panels and centered document frames use the
    // window stroke by default, which creates hairlines around otherwise
    // flat surfaces. Typora's canvas relies on whitespace and tone changes
    // instead of boxed regions, so remove those global frame strokes.
    visuals.window_stroke = egui::Stroke::NONE;
    visuals.extreme_bg_color = spec.code_bg;
    visuals.faint_bg_color = spec.quote_bg;
    visuals.hyperlink_color = spec.accent;
    visuals.override_text_color = Some(spec.text);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
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
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut local = SYSTEMTIME::default();
        // SAFETY: GetLocalTime only writes through the provided pointer and
        // fills every field of the SYSTEMTIME structure.
        unsafe { GetLocalTime(&mut local) };
        format!(
            "{:02}:{:02}:{:02}",
            local.wHour, local.wMinute, local.wSecond
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        // macOS 的 libSystem 与 Linux 的 glibc 都导出 localtime_r，直接声明
        // 这两个字段，避免为状态栏时间新增 libc 依赖（与 Windows 分支
        // 手写 GetLocalTime 的风格一致）。
        #[repr(C)]
        struct Tm {
            tm_sec: i32,
            tm_min: i32,
            tm_hour: i32,
            // 余下 6 个 int 字段（mday..isdst）加 glibc/macOS 布局中的
            // tm_gmtoff + tm_zone，两者在这两个平台上都是 isize/指针宽度。
            _rest: [i32; 6],
            _tail: [usize; 2],
        }
        unsafe extern "C" {
            fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
        }
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let mut tm = unsafe { std::mem::zeroed::<Tm>() };
        // SAFETY: 两个指针都指向本函数持有的栈上值；Tm 按上述布局至少
        // 和 struct tm 一样大，localtime_r 不会越界写入。
        let wrote = unsafe { localtime_r(&raw const secs, &raw mut tm) };
        if wrote.is_null() {
            // 系统时钟离谱到超出平台时间范围时退回 UTC 展示：至少与
            // epoch 秒数一致，不会像零值 tm 那样伪装成真实的本地午夜。
            let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
            return format!("{:02}:{:02}:{:02}", h, m, s);
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
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
            search_generation: 0,
            search_pending: None,
            search_focus_requested: false,
            search_scroll_requested: false,
            search_backwards: false,
            search_input_has_focus: false,
            pending_close: None,
            window_close_guard: window_close::CloseGuard::default(),
            recovery: None,
            dark: false,
            focus_mode: false,
            typewriter_mode: false,
            active_edit_block: None,
            active_edit_range: None,
            pending_edit_cursor: None,
            edit_focus_requested: false,
            show_status: true,
            body_font_size: 15.0,
            theme_package: None,
            auto_reload_external: true,
            last_external_poll: f64::NEG_INFINITY,
            external_watcher: None,
            observed_file_stamps: HashMap::new(),
            pending_external_changes: HashMap::new(),
            instance_requests: None,
            draft_window_id: None,
            persisted_window_session: None,
            window_session_initialized: false,
            image_cache: preview::ImageCache::default(),
            parse_worker: ParseWorker::new(),
            search_worker: SearchWorker::new(),
            text_stats_cache: None,
            last_draft_signature: None,
            search_byte_cache: None,
            last_window_title: None,
        }
    }

    #[test]
    fn 脏状态缓存随源码版本与磁盘快照失效() {
        let mut tab = DocumentTab::from_file(
            1,
            PathBuf::from("cached.md"),
            "原文".to_string(),
            "原文".as_bytes().to_vec(),
        );
        assert!(!tab.is_dirty());

        // 长度相同的磁盘快照同样要让缓存失效，否则保存后仍显示未保存。
        tab.replace_disk_snapshot("新文".as_bytes().to_vec());
        assert!(tab.is_dirty());

        tab.core.set_source("新文".to_string());
        assert!(!tab.is_dirty());
    }

    #[test]
    fn 草稿签名随编辑与磁盘快照变化() {
        let mut app = app_with_two_tabs();
        *app.tabs[0].core.source_mut() = "草稿".to_string();
        app.tabs[0].core.mark_source_changed();
        let first = app.draft_signature(&app.draft_candidates());
        assert_eq!(first.len(), 1);

        // 未再编辑时签名不变：自动保存不会再重写同一份草稿。
        assert_eq!(app.draft_signature(&app.draft_candidates()), first);

        app.tabs[0].core.mark_source_changed();
        assert_ne!(app.draft_signature(&app.draft_candidates()), first);
    }

    #[test]
    fn 过期解析结果不能覆盖当前文档快照() {
        let mut app = app_with_two_tabs();
        *app.tabs[0].core.source_mut() = "# newest".to_string();
        let current_revision = app.tabs[0].core.mark_source_changed();
        app.tabs[0].parse_requested_revision = Some(current_revision);

        let stale = ParseResult {
            tab_id: 1,
            revision: current_revision - 1,
            document: Arc::new(markdown::parse_document("# stale")),
        };
        assert!(!app.apply_parse_result(stale));
        assert_eq!(app.tabs[0].document.source(), "");
        assert!(!app.tabs[0].is_parsed_current());

        let current = ParseResult {
            tab_id: 1,
            revision: current_revision,
            document: Arc::new(markdown::parse_document("# newest")),
        };
        assert!(app.apply_parse_result(current));
        assert_eq!(app.tabs[0].document.source(), "# newest");
        assert!(app.tabs[0].is_parsed_current());
        assert_eq!(app.tabs[0].parse_requested_revision, None);
    }

    #[test]
    fn 已关闭标签的迟到解析结果会被忽略() {
        let mut app = app_with_two_tabs();
        let result = ParseResult {
            tab_id: 99,
            revision: 1,
            document: Arc::new(markdown::parse_document("orphan")),
        };

        assert!(!app.apply_parse_result(result));
        assert_eq!(app.tabs.len(), 2);
    }

    #[test]
    fn 编辑后由后台解析结果恢复一致快照() {
        let mut app = app_with_two_tabs();
        *app.tabs[0].core.source_mut() = "# 后台解析".to_string();
        app.mark_tab_source_changed(0, 1.0);

        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !app.tabs[0].is_parsed_current() {
            app.poll_parse_results(&ctx);
            assert!(
                std::time::Instant::now() < deadline,
                "后台解析结果未在期限内返回"
            );
            std::thread::yield_now();
        }
        assert_eq!(app.tabs[0].document.source(), "# 后台解析");
    }

    #[test]
    fn 过期搜索结果不能覆盖当前查询() {
        let mut app = app_with_two_tabs();
        *app.tabs[0].core.source_mut() = "new new".to_string();
        app.tabs[0].core.mark_source_changed();
        app.search_open = true;
        app.search_generation = 2;
        app.search_tab_id = Some(1);
        app.search_document_revision = app.tabs[0].document_revision;
        app.search_pending = Some((1, app.tabs[0].document_revision, 2));

        let stale = SearchResult {
            tab_id: 1,
            revision: app.tabs[0].document_revision,
            generation: 1,
            results: search::SearchResults::new("old old", "old"),
        };
        assert!(!app.apply_search_result(stale));
        assert!(app.search_results.ranges().is_empty());

        let current = SearchResult {
            tab_id: 1,
            revision: app.tabs[0].document_revision,
            generation: 2,
            results: search::SearchResults::new("new new", "new"),
        };
        assert!(app.apply_search_result(current));
        assert_eq!(app.search_results.ranges(), &[0..3, 4..7]);
        assert_eq!(app.search_pending, None);
    }

    #[test]
    fn 搜索请求由后台任务返回并带有当前版本() {
        let mut app = app_with_two_tabs();
        *app.tabs[0].core.source_mut() = "alpha beta alpha".to_string();
        app.tabs[0].core.mark_source_changed();
        app.search_open = true;
        app.search_query = "alpha".to_string();
        app.refresh_search();

        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while app.search_pending.is_some() {
            app.poll_search_results(&ctx);
            assert!(
                std::time::Instant::now() < deadline,
                "后台搜索结果未在期限内返回"
            );
            std::thread::yield_now();
        }
        assert_eq!(app.search_results.ranges(), &[0..5, 11..16]);
    }

    #[test]
    fn window_close_collects_every_unsaved_tab_not_only_the_active_one() {
        let mut app = app_with_two_tabs();
        *app.tabs[0].core.source_mut() = "first draft".to_string();
        *app.tabs[1].core.source_mut() = "second draft".to_string();

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
            tab.source(),
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
        assert!(
            icon.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] == 0)
        );
        assert!(
            icon.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] == 255)
        );
    }

    #[test]
    fn 空白标签不显示修改标记() {
        let tab = DocumentTab::blank(7);
        assert!(!document_is_dirty(
            tab.path.as_ref(),
            tab.source(),
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
    fn 活动标签索引在任何关闭顺序下都有效() {
        // `MdEditorApp` 的 Deref 会索引 `tabs[active_tab]`，一旦 tabs 为空或
        // active_tab 越界，帧循环里任何 `self.text` 访问都会 panic。
        let mut app = app_with_two_tabs();
        while !app.workspace_empty {
            app.close_tab_now(app.active_tab);
            assert!(!app.tabs.is_empty(), "tabs 必须保留兜底标签");
            assert!(app.active_tab < app.tabs.len(), "active_tab 越界");
            // 不变量成立时 Deref 不应 panic。
            let _ = app.source().len();
        }
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, 0);
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
            tab.source(),
            &tab.disk_snapshot,
            &tab.status,
        ));
        tab.core.source_mut().push_str("修改");
        assert!(document_is_dirty(
            tab.path.as_ref(),
            tab.source(),
            &tab.disk_snapshot,
            &tab.status,
        ));
        assert_eq!(document_label(tab.id, Some(&path), true), "notes.md  •");
    }

    #[test]
    fn 界面直接修改活动标签且切换无需写回() {
        let mut app = app_with_two_tabs();
        *app.core.source_mut() = "第一个标签".to_string();
        app.activate_tab(1);
        *app.core.source_mut() = "第二个标签".to_string();
        assert_eq!(app.tabs[0].source(), "第一个标签");
        assert_eq!(app.tabs[1].source(), "第二个标签");
        app.activate_tab(0);
        assert_eq!(app.source(), "第一个标签");
    }

    #[test]
    fn 草稿会话收集全部未保存标签并保留活动标签() {
        let mut app = app_with_two_tabs();
        *app.tabs[0].core.source_mut() = "草稿一".to_string();
        *app.tabs[1].core.source_mut() = "草稿二".to_string();
        let mut cleared = DocumentTab::from_file(
            3,
            PathBuf::from("cleared.md"),
            "原文".to_string(),
            "原文".as_bytes().to_vec(),
        );
        cleared.core.source_mut().clear();
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
        assert_eq!(tab.source(), "本地未保存内容");
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
        assert_eq!(app.source(), "跨进程打开内容");

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
        assert_eq!(app.source(), "启动文件内容");
        assert_eq!(app.active_edit_block, Some(0));
        assert!(app.edit_focus_requested);
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
        assert_eq!(app.active_edit_block, Some(0));
        assert!(app.edit_focus_requested);
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
    fn 无本地修改时自动采用外部内容() {
        let mut tab = DocumentTab::from_file(
            1,
            PathBuf::from("agent.md"),
            "旧内容".to_string(),
            b"\xe6\x97\xa7\xe5\x86\x85\xe5\xae\xb9".to_vec(),
        );
        let result = apply_external_bytes(&mut tab, "Agent 新内容".as_bytes().to_vec()).unwrap();
        assert_eq!(result, ExternalChangeResult::Reloaded);
        assert_eq!(tab.source(), "Agent 新内容");
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
        *tab.core.source_mut() = "本地尚未保存".to_string();
        let result = apply_external_bytes(&mut tab, "Agent 修改".as_bytes().to_vec()).unwrap();
        assert_eq!(result, ExternalChangeResult::Conflict);
        assert_eq!(tab.source(), "本地尚未保存");
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
        *tab.core.source_mut() = "共同的新内容".to_string();
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
            MdEditorApp::probe_external_change(
                &mut app.observed_file_stamps,
                &mut app.pending_external_changes,
                &app.tabs,
                0,
                &path,
                0.0,
                true,
            ),
            ExternalProbe::Waiting
        ));
        assert!(matches!(
            MdEditorApp::probe_external_change(
                &mut app.observed_file_stamps,
                &mut app.pending_external_changes,
                &app.tabs,
                0,
                &path,
                EXTERNAL_STABLE_DELAY,
                false,
            ),
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
        app.external_watcher = ExternalFileWatcher::new(egui::Context::default());
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

    #[test]
    fn 解析落后时丢弃块点击() {
        // Regression for the stale-parse contract (ADR-0004 decision 3): a
        // click captured against the previous parse's block indices must not
        // move the editor or its caret. `preview.rs` has the frame-level
        // counterpart that feeds a real click through the egui harness.
        assert_eq!(clicked_block_accepted(false, Some(2)), None);
        assert_eq!(clicked_block_accepted(false, None), None);
        assert_eq!(clicked_block_accepted(true, Some(2)), Some(2));
    }
}
