#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod export;
mod io;
mod markdown;
mod preview;
mod theme;
#[cfg(target_os = "windows")]
mod web_preview;

use std::path::PathBuf;

use eframe::egui;
use egui::containers::scroll_area::ScrollAreaOutput;
use markdown::Block;
use theme::{ThemePackage, ThemeSpec};

fn main() -> eframe::Result {
    let open_path = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Markdown 编辑器与预览器"),
        ..Default::default()
    };
    eframe::run_native(
        "markdown-editor",
        options,
        Box::new(move |cc| {
            let mut app = MdEditorApp::new(cc);
            if let Some(path) = open_path {
                app.open_path(&path);
            }
            Ok(Box::new(app))
        }),
    )
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
    blocks: Vec<Block>,
    last_parsed: String,
    status_note: String,
    conflict: Option<PathBuf>,
    draft_last_write: f64,
    last_edit_time: f64,
    prev_editor_ratio: f32,
    prev_preview_ratio: f32,
    last_caret_line: usize,
}

impl DocumentTab {
    fn blank(id: u64) -> Self {
        Self {
            id,
            text: String::new(),
            path: None,
            disk_snapshot: Vec::new(),
            status: DocStatus::Unsaved,
            blocks: Vec::new(),
            last_parsed: String::new(),
            status_note: String::new(),
            conflict: None,
            draft_last_write: 0.0,
            last_edit_time: f64::INFINITY,
            prev_editor_ratio: 0.0,
            prev_preview_ratio: 0.0,
            last_caret_line: 0,
        }
    }

    fn from_file(id: u64, path: PathBuf, text: String, snapshot: Vec<u8>) -> Self {
        let blocks = markdown::parse(&text);
        Self {
            id,
            last_parsed: text.clone(),
            text,
            path: Some(path),
            disk_snapshot: snapshot,
            status: DocStatus::Saved,
            blocks,
            status_note: String::new(),
            conflict: None,
            draft_last_write: 0.0,
            last_edit_time: f64::INFINITY,
            prev_editor_ratio: 0.0,
            prev_preview_ratio: 0.0,
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
            Some(_) => snapshot != text.as_bytes(),
            None => !text.is_empty(),
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

const CHROME_FONT_SIZE: f32 = 13.0;
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
    let galley = ui.painter().layout_no_wrap(title, font.clone(), text_color);
    let width = (galley.size().x + 58.0).clamp(96.0, 220.0);
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

    let text_pos = egui::pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(text_pos, galley, text_color);

    if dirty {
        ui.painter().circle_filled(
            egui::pos2(close_rect.left() - 5.0, rect.center().y),
            2.5,
            ui.visuals().warn_fg_color,
        );
    }
    if selected || hovered {
        let close_color = if close_response.hovered() {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().widgets.inactive.fg_stroke.color
        };
        ui.painter().text(
            close_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::new(CHROME_FONT_SIZE, egui::FontFamily::Proportional),
            close_color,
        );
    }

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
    pending_close: Option<usize>,
    text: String,
    path: Option<PathBuf>,
    disk_snapshot: Vec<u8>,
    status: DocStatus,
    blocks: Vec<Block>,
    last_parsed: String,
    status_note: String,
    conflict: Option<PathBuf>,
    recovery: Option<String>,
    dark: bool,
    draft_last_write: f64,
    last_edit_time: f64,
    prev_editor_ratio: f32,
    prev_preview_ratio: f32,
    last_caret_line: usize,
    editor_focused: bool,
    view_mode: ViewMode,
    focus_mode: bool,
    show_status: bool,
    body_font_size: f32,
    theme_package: Option<ThemePackage>,
    #[cfg(target_os = "windows")]
    browser_preview: web_preview::BrowserPreview,
}

impl MdEditorApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);
        let theme_package = theme::load_saved();
        let built_in_theme = ThemePackage::built_in_sspai();
        let initial_body_font_size = theme_package
            .as_ref()
            .map(ThemePackage::recommended_body_font_size)
            .unwrap_or_else(|| built_in_theme.recommended_body_font_size());
        let initial_theme = theme_package
            .as_ref()
            .and_then(|t| t.spec(false).ok())
            .or_else(|| built_in_theme.spec(false).ok())
            .unwrap_or_else(|| ThemeSpec::fallback(false));
        apply_visuals(&cc.egui_ctx, false, &initial_theme);
        let recovery = io::load_draft();
        let sample = "# Markdown 编辑器与预览器\n\n左栏编辑，右栏实时预览。\n\n- 支持标题、列表、表格、代码块\n- Ctrl+S 保存，Ctrl+O 打开\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n## 功能\n\n| 功能 | 状态 |\n| --- | --- |\n| 编辑 | 可用 |\n| 预览 | 可用 |\n";
        let mut app = Self {
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 2,
            pending_close: None,
            text: sample.to_string(),
            path: None,
            disk_snapshot: Vec::new(),
            status: DocStatus::Modified,
            blocks: Vec::new(),
            last_parsed: String::new(),
            status_note: String::new(),
            conflict: None,
            recovery,
            dark: false,
            draft_last_write: 0.0,
            last_edit_time: f64::INFINITY,
            prev_editor_ratio: 0.0,
            prev_preview_ratio: 0.0,
            last_caret_line: 0,
            editor_focused: false,
            view_mode: ViewMode::Write,
            focus_mode: false,
            show_status: true,
            body_font_size: initial_body_font_size,
            theme_package,
            #[cfg(target_os = "windows")]
            browser_preview: web_preview::BrowserPreview::default(),
        };
        app.blocks = markdown::parse(&app.text);
        app.last_parsed = app.text.clone();
        app.tabs.push(app.capture_active_tab(1));
        app
    }

    fn capture_active_tab(&self, id: u64) -> DocumentTab {
        DocumentTab {
            id,
            text: self.text.clone(),
            path: self.path.clone(),
            disk_snapshot: self.disk_snapshot.clone(),
            status: self.status.clone(),
            blocks: self.blocks.clone(),
            last_parsed: self.last_parsed.clone(),
            status_note: self.status_note.clone(),
            conflict: self.conflict.clone(),
            draft_last_write: self.draft_last_write,
            last_edit_time: self.last_edit_time,
            prev_editor_ratio: self.prev_editor_ratio,
            prev_preview_ratio: self.prev_preview_ratio,
            last_caret_line: self.last_caret_line,
        }
    }

    fn persist_active_tab(&mut self) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let state = self.capture_active_tab(tab.id);
        self.tabs[self.active_tab] = state;
    }

    fn load_tab_state(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index).cloned() else {
            return;
        };
        self.active_tab = index;
        self.text = tab.text;
        self.path = tab.path;
        self.disk_snapshot = tab.disk_snapshot;
        self.status = tab.status;
        self.blocks = tab.blocks;
        self.last_parsed = tab.last_parsed;
        self.status_note = tab.status_note;
        self.conflict = tab.conflict;
        self.draft_last_write = tab.draft_last_write;
        self.last_edit_time = tab.last_edit_time;
        self.prev_editor_ratio = tab.prev_editor_ratio;
        self.prev_preview_ratio = tab.prev_preview_ratio;
        self.last_caret_line = tab.last_caret_line;
        self.editor_focused = false;
    }

    fn switch_tab(&mut self, index: usize) {
        if self.pending_close.is_some() || index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        self.persist_active_tab();
        self.load_tab_state(index);
    }

    fn push_tab(&mut self, tab: DocumentTab) {
        self.persist_active_tab();
        self.tabs.push(tab);
        self.load_tab_state(self.tabs.len() - 1);
    }

    fn new_tab(&mut self) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.push_tab(DocumentTab::blank(id));
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
        if index == self.active_tab {
            return self.is_active_dirty();
        }
        let tab = &self.tabs[index];
        document_is_dirty(
            tab.path.as_ref(),
            &tab.text,
            &tab.disk_snapshot,
            &tab.status,
        )
    }

    fn tab_title(&self, index: usize) -> String {
        let (id, path) = if index == self.active_tab {
            (self.tabs[index].id, &self.path)
        } else {
            let tab = &self.tabs[index];
            (tab.id, &tab.path)
        };
        document_label(id, path.as_ref(), false)
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

    fn close_tab_now(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.persist_active_tab();
        let old_active = self.active_tab;
        self.tabs.remove(index);
        self.pending_close = None;
        if self.tabs.is_empty() {
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            self.tabs.push(DocumentTab::blank(id));
            self.load_tab_state(0);
        } else {
            let new_active = if index < old_active {
                old_active - 1
            } else if index == old_active {
                index.min(self.tabs.len() - 1)
            } else {
                old_active
            };
            self.load_tab_state(new_active);
        }
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

    #[cfg(target_os = "windows")]
    fn browser_document(&self) -> String {
        let built_in = ThemePackage::built_in_sspai();
        let package = self.theme_package.as_ref().unwrap_or(&built_in);
        let css = package.browser_css().unwrap_or(theme::BUILT_IN_SSPAI_CSS);
        let default_size = package.recommended_body_font_size();
        let font_override =
            ((self.body_font_size - default_size).abs() > 0.01).then_some(self.body_font_size);
        let base_directory = self.path.as_deref().and_then(std::path::Path::parent);
        web_preview::document(&self.text, css, base_directory, font_override)
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
                self.apply_current_theme(ctx);
                self.status_note = format!("已加载主题：{name}");
            }
            Err(e) => self.status = DocStatus::SaveFailed(e),
        }
    }

    fn remove_theme(&mut self, ctx: &egui::Context) {
        self.theme_package = None;
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

    fn autosave_draft(&mut self, now: f64) {
        let dirty = !matches!(self.status, DocStatus::Saved) && !self.text.is_empty();
        if dirty && now - self.last_edit_time > 30.0 && now - self.draft_last_write > 30.0 {
            if io::save_draft(&self.text).is_ok() {
                self.draft_last_write = now;
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let new_tab = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N);
        let open = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
        let save = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S);
        let save_as = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::S,
        );
        let close_tab = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);
        if ctx.input_mut(|i| i.consume_shortcut(&new_tab)) {
            self.new_tab();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&open)) {
            self.open_file();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&save)) {
            self.save();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&save_as)) {
            self.save_as();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&close_tab)) {
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

    fn open_path(&mut self, path: &PathBuf) {
        self.persist_active_tab();
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.path.as_ref() == Some(path))
        {
            self.load_tab_state(index);
            self.status_note = format!("已切换到 {}", path.display());
            return;
        }
        match io::read_markdown(path) {
            Ok(text) => {
                let snapshot = io::read_snapshot(path).unwrap_or_default();
                let id = self.next_tab_id;
                self.next_tab_id += 1;
                self.push_tab(DocumentTab::from_file(id, path.clone(), text, snapshot));
                self.status_note = format!("已打开 {}", path.display());
                io::clear_draft();
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
                io::clear_draft();
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
                self.status = DocStatus::Saved;
                self.status_note = format!("已保存 {}", clock_time());
                io::clear_draft();
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
                    io::clear_draft();
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
                    self.last_parsed.clear();
                    self.status = DocStatus::Saved;
                    self.status_note = "已重新载入磁盘内容".to_string();
                    io::clear_draft();
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
        match export::export_html(&path, &self.text) {
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
        match export::export_pdf(&path, &self.text) {
            Ok(()) => self.status_note = format!("已导出 PDF：{}", path.display()),
            Err(e) => self.status = DocStatus::SaveFailed(format!("导出失败：{}", e)),
        }
    }

    #[allow(dead_code)]
    fn menu_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("文件", |ui| {
                if ui.button("打开…    Ctrl+O").clicked() {
                    ui.close();
                    self.open_file();
                }
                if ui.button("保存    Ctrl+S").clicked() {
                    ui.close();
                    self.save();
                }
                if ui.button("另存为…    Ctrl+Shift+S").clicked() {
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
                    ctx.copy_text(markdown::plain_text(&self.blocks));
                    self.status_note = "已复制渲染内容".to_string();
                    ui.close();
                }
                if ui.button("复制 HTML").clicked() {
                    ctx.copy_text(export::render_html(&self.text));
                    self.status_note = "已复制 HTML".to_string();
                    ui.close();
                }
            });
            ui.menu_button("视图", |ui| {
                ui.selectable_value(&mut self.view_mode, ViewMode::Write, "写作模式     Ctrl+1");
                ui.selectable_value(
                    &mut self.view_mode,
                    ViewMode::Preview,
                    "阅读模式     Ctrl+2",
                );
                ui.selectable_value(&mut self.view_mode, ViewMode::Split, "分栏模式     Ctrl+3");
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
                    self.dark = !self.dark;
                    self.apply_current_theme(ui.ctx());
                    ui.close();
                }
            });
            ui.menu_button("视图", |ui| {
                ui.selectable_value(&mut self.view_mode, ViewMode::Write, "写作模式    Ctrl+1");
                ui.selectable_value(&mut self.view_mode, ViewMode::Preview, "阅读模式    Ctrl+2");
                ui.selectable_value(&mut self.view_mode, ViewMode::Split, "分栏模式    Ctrl+3");
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
                    self.dark = !self.dark;
                    self.apply_current_theme(ctx);
                    ui.close();
                }
            });
        });
    }

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), CHROME_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().button_padding = egui::vec2(7.0, 4.0);
                ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
                ui.visuals_mut().widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                ui.menu_button(egui::RichText::new("文件").size(CHROME_FONT_SIZE), |ui| {
                    if ui.button("新建标签   Ctrl+N").clicked() {
                        ui.close();
                        self.new_tab();
                    }
                    if ui.button("打开…     Ctrl+O").clicked() {
                        ui.close();
                        self.open_file();
                    }
                    if ui.button("保存       Ctrl+S").clicked() {
                        ui.close();
                        self.save();
                    }
                    if ui.button("另存为…   Ctrl+Shift+S").clicked() {
                        ui.close();
                        self.save_as();
                    }
                    if ui.button("关闭标签   Ctrl+W").clicked() {
                        ui.close();
                        self.request_close_tab(self.active_tab);
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
                });
                ui.menu_button(egui::RichText::new("编辑").size(CHROME_FONT_SIZE), |ui| {
                    if ui.button("复制渲染内容").clicked() {
                        ui.close();
                        ui.ctx().copy_text(markdown::plain_text(&self.blocks));
                    }
                    if ui.button("复制 HTML").clicked() {
                        ui.close();
                        ui.ctx().copy_text(export::render_html(&self.text));
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
                                    for index in 0..self.tabs.len() {
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
                                        .on_hover_text("新建标签 · Ctrl+N")
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
                    if chrome_nav_button(ui, "专注", self.focus_mode)
                        .on_hover_text("专注模式 · F8")
                        .clicked()
                    {
                        self.focus_mode = !self.focus_mode;
                    }
                    ui.add_space(4.0);
                    if chrome_nav_button(ui, "分栏", self.view_mode == ViewMode::Split).clicked()
                    {
                        self.view_mode = ViewMode::Split;
                    }
                    if chrome_nav_button(ui, "阅读", self.view_mode == ViewMode::Preview).clicked()
                    {
                        self.view_mode = ViewMode::Preview;
                    }
                    if chrome_nav_button(ui, "写作", self.view_mode == ViewMode::Write).clicked()
                    {
                        self.view_mode = ViewMode::Write;
                    }
                });
            },
        );
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
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

    fn sync_scrolls(
        &mut self,
        ctx: &egui::Context,
        editor: &ScrollAreaOutput<egui::Id>,
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
        let e_changed = (ratio_e - self.prev_editor_ratio).abs() > 0.002;
        let p_changed = (ratio_p - self.prev_preview_ratio).abs() > 0.002;

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

    fn sync_caret(
        &mut self,
        ctx: &egui::Context,
        editor: &ScrollAreaOutput<egui::Id>,
        preview: &ScrollAreaOutput<()>,
    ) {
        if !self.editor_focused {
            return;
        }
        let te_id = editor.inner;
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

    fn recovery_window(&mut self, ctx: &egui::Context) {
        if self.recovery.is_none() {
            return;
        }
        egui::Window::new("发现未保存草稿")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("上次退出时存在未保存的内容，是否恢复？");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("恢复草稿").clicked() {
                        if let Some(draft) = self.recovery.take() {
                            self.text = draft;
                            self.last_parsed.clear();
                            self.refresh_status();
                            self.status_note = "已恢复草稿".to_string();
                            io::clear_draft();
                        }
                    }
                    if ui.button("放弃草稿").clicked() {
                        self.recovery = None;
                        io::clear_draft();
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
}

impl eframe::App for MdEditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        #[cfg(target_os = "windows")]
        let mut browser_rect = None;

        if self.text != self.last_parsed {
            self.blocks = markdown::parse(&self.text);
            self.last_parsed = self.text.clone();
            self.last_edit_time = now;
            self.refresh_status();
        }

        self.autosave_draft(now);
        self.handle_shortcuts(&ctx);

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

        if self.show_status && !self.focus_mode {
            egui::Panel::bottom("status_panel")
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(12, 5)),
                )
                .show(ui, |ui| self.status_bar(ui));
        }

        let doc_theme = self.theme_spec();
        let editor_fill = doc_theme.editor_canvas;

        match self.view_mode {
            ViewMode::Write => {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(editor_fill)
                            .inner_margin(egui::Margin::symmetric(22, 0)),
                    )
                    .show(ui, |ui| {
                        show_centered_editor(
                            ui,
                            &mut self.text,
                            &mut self.editor_focused,
                            self.body_font_size,
                            &doc_theme,
                        );
                    });
            }
            ViewMode::Preview => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(ui.visuals().window_fill))
                    .show(ui, |ui| {
                        #[cfg(target_os = "windows")]
                        {
                            let rect = ui.available_rect_before_wrap();
                            ui.allocate_rect(rect, egui::Sense::hover());
                            browser_rect = Some(rect);
                        }
                        #[cfg(not(target_os = "windows"))]
                        show_centered_preview(ui, &self.blocks, self.body_font_size, &doc_theme);
                    });
            }
            ViewMode::Split => {
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
                            &mut self.text,
                            &mut self.editor_focused,
                            self.body_font_size,
                        )
                    });
                #[cfg(target_os = "windows")]
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(ui.visuals().window_fill))
                    .show(ui, |ui| {
                        let rect = ui.available_rect_before_wrap();
                        ui.allocate_rect(rect, egui::Sense::hover());
                        browser_rect = Some(rect);
                    });
                #[cfg(not(target_os = "windows"))]
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
                            show_preview_scroll(ui, &self.blocks, self.body_font_size, &doc_theme)
                        });
                    self.sync_scrolls(&ctx, &editor_out.inner, &preview_out.inner);
                    self.sync_caret(&ctx, &editor_out.inner, &preview_out.inner);
                }
                #[cfg(target_os = "windows")]
                let _ = editor_out;
            }
        }

        #[cfg(target_os = "windows")]
        {
            let modal_open =
                self.conflict.is_some() || self.recovery.is_some() || self.pending_close.is_some();
            let popup_open = ctx.any_popup_open();
            if modal_open || popup_open || browser_rect.is_none() {
                self.browser_preview.hide();
            } else if let Some(rect) = browser_rect {
                let document = self.browser_document();
                if let Err(error) =
                    self.browser_preview
                        .show(frame, rect, ctx.pixels_per_point(), &document)
                {
                    self.status_note = error;
                }
                if self.editor_focused {
                    self.browser_preview.focus_parent();
                }
            }
        }
        self.conflict_window(&ctx);
        self.recovery_window(&ctx);
        self.close_tab_window(&ctx);
    }
}

fn editor_widget(
    ui: &mut egui::Ui,
    text: &mut String,
    focused: &mut bool,
    font_size: f32,
) -> egui::Id {
    let id = ui.id().with("md_text");
    let response = ui.add(
        egui::TextEdit::multiline(text)
            .id(id)
            .font(egui::FontId::new(font_size, egui::FontFamily::Monospace))
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::same(0))
            .desired_width(f32::INFINITY)
            .desired_rows(40),
    );
    *focused = response.has_focus();
    id
}

fn show_editor_scroll(
    ui: &mut egui::Ui,
    text: &mut String,
    focused: &mut bool,
    font_size: f32,
) -> ScrollAreaOutput<egui::Id> {
    egui::ScrollArea::vertical()
        .id_salt("editor_scroll")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(28.0);
            editor_widget(ui, text, focused, font_size)
        })
}

fn show_preview_scroll(
    ui: &mut egui::Ui,
    blocks: &[Block],
    body_font_size: f32,
    theme: &ThemeSpec,
) -> ScrollAreaOutput<()> {
    egui::ScrollArea::vertical()
        .id_salt("preview_scroll")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(28.0);
            preview::show_preview_with_theme(ui, blocks, body_font_size, theme);
        })
}

fn show_centered_editor(
    ui: &mut egui::Ui,
    text: &mut String,
    focused: &mut bool,
    font_size: f32,
    theme: &ThemeSpec,
) {
    egui::ScrollArea::vertical()
        .id_salt("editor_scroll_solo")
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
                        editor_widget(ui, text, focused, font_size);
                        ui.add_space(160.0);
                    },
                );
            });
        });
}

fn show_centered_preview(
    ui: &mut egui::Ui,
    blocks: &[Block],
    body_font_size: f32,
    theme: &ThemeSpec,
) {
    egui::ScrollArea::vertical()
        .id_salt("preview_scroll_solo")
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
    visuals.widgets.noninteractive.bg_stroke.color = spec.border;
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

#[cfg(test)]
mod app_tests {
    use super::*;

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
}
