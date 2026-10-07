/// IDE editor window — a Window containing a breakpoint gutter and a code editor side-by-side.
///
/// Follows the same pattern as turbo-vision's EditWindow but adds the gutter
/// as an interior child so that the gutter is visually part of the editor frame.
use crate::commands::CM_CLOSE_EDITOR;
use crate::gutter::{BreakpointGutter, GUTTER_WIDTH};
use crate::heat::{Heat, LineProfile, PROFILE_COL_WIDTH, format_share, heat_bucket, text_hash};
use crate::ide_file_editor::IdeFileEditor;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use turbo_vision::app::Application;
use turbo_vision::core::command::{CM_CLOSE, CommandId};
use turbo_vision::core::draw::{Cell, DrawBuffer};
use turbo_vision::core::event::{Event, EventType};
use turbo_vision::core::geometry::{Point, Rect};
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::State;
use turbo_vision::impl_view_for_window;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::editor::EditorWindow;
use turbo_vision::views::editor_traits::{
    Editor, ExternalState, FileEditor, confirm_save_on_close,
};
use turbo_vision::views::file_dialog::FileDialogBuilder;
use turbo_vision::views::group::{Group, GroupLike};
use turbo_vision::views::indicator::Indicator;
use turbo_vision::views::scrollbar::ScrollBar;
use turbo_vision::views::shared::Shared;
use turbo_vision::views::syntax::SyntaxHighlighter;
use turbo_vision::views::view::{View, dispatch_to_child, write_line_to_terminal};
use turbo_vision::views::window::{Window, WindowLike};

/// Desktop-installable handle to an [`IdeEditorWindow`]. The owning `Rc`
/// lives in the IDE main loop, so closing the wrapper drops only one ref and
/// the underlying window (with its loaded buffer) stays alive; File→Open can
/// re-install it later. Reach the window through [`Shared::inner`].
pub type SharedIdeEditorWindow = Shared<IdeEditorWindow>;

// ── IdeEditorWindow ──────────────────────────────────────

/// A Window containing a breakpoint gutter on the left and a code editor on the right,
/// with scrollbars and an indicator on the frame edge (same as EditWindow).
///
/// Tracks the on-disk path of the buffer's source file. Dirty state lives on
/// the inner [`EditorWindow`] (see [`EditorWindow::is_modified`]).
pub struct IdeEditorWindow {
    window: Window,
    editor: Rc<RefCell<EditorWindow>>,
    gutter: Rc<RefCell<BreakpointGutter>>,
    v_scrollbar: Rc<RefCell<ScrollBar>>,
    h_scrollbar: Rc<RefCell<ScrollBar>>,
    indicator: Rc<RefCell<Indicator>>,
    file_path: Rc<RefCell<Option<PathBuf>>>,
    /// mtime captured at the last successful load/save. Used by
    /// [`FileEditor::poll_external_changes`] to detect outside edits.
    last_mtime: RefCell<Option<SystemTime>>,
    /// Wildcard used by [`FileEditor::prompt_save_as`] (e.g. `"*.pas"`).
    save_wildcard: String,
    /// Title shown when no file is bound (e.g. `"Untitled.pas"`).
    default_title: String,
    /// Last value reflected in the window title — lets `draw()` cheaply detect
    /// when `file_path` changed (e.g. via the main loop) and refresh the title.
    last_titled_path: RefCell<Option<PathBuf>>,
    h_scrollbar_idx: usize,
    v_scrollbar_idx: usize,
    indicator_idx: usize,
    /// Last build error tied to this buffer: 1-based line + message
    /// extracted from the compiler diagnostic. The line drives the red
    /// row overlay; the message is surfaced in the status bar when the
    /// caret sits on that line. Cleared at the start of every build.
    build_error: RefCell<Option<(usize, String)>>,
    /// Per-line timings from the last profile run. `None` until a profile
    /// run completes; cleared on the next build or when the text changes.
    line_profile: Option<LineProfile>,
    /// Pascal source line correlated with the Disassembly window via a
    /// double-click — distinct from `build_error` (red) and the debugger's
    /// exec line (green). Drives a cyan row overlay in `draw()`; cleared
    /// at the start of the next build (see `set_asm_correlate_line`).
    asm_correlate_line: RefCell<Option<usize>>,
    /// Set alongside `asm_correlate_line` on a double-click; drained once
    /// by the IDE event loop (`take_pending_correlate_line`) to push the
    /// line into the Disassembly window's own highlight.
    pending_correlate_line: RefCell<Option<usize>>,
}

impl IdeEditorWindow {
    pub fn new(bounds: Rect, title: &str, save_wildcard: &str) -> Self {
        let mut window = Window::new(bounds, title);
        // Opt out of auto-close so CM_CLOSE bubbles up to the IDE main loop,
        // which translates it into CM_CLOSE_EDITOR and shows a save prompt
        // before destroying the editor buffer.
        window.set_auto_close(false);

        let win_w = bounds.width();
        let win_h = bounds.height();
        let interior_w = win_w - 2;
        let interior_h = win_h - 2;

        // Gutter sits at the left edge of the interior (relative coords)
        let gutter_bounds = Rect::new(0, 0, GUTTER_WIDTH, interior_h);
        let gutter = Rc::new(RefCell::new(BreakpointGutter::new(gutter_bounds)));

        // EditorWindow fills the rest of the interior, right of the gutter
        let editor_bounds = Rect::new(GUTTER_WIDTH, 0, interior_w, interior_h);

        // Scrollbars on the window frame (relative to frame)
        let h_bounds = Rect::new(18, win_h - 1, win_w - 2, win_h);
        let h_scrollbar = Rc::new(RefCell::new(ScrollBar::new_horizontal(h_bounds)));

        let v_bounds = Rect::new(win_w - 1, 1, win_w, win_h - 2);
        let v_scrollbar = Rc::new(RefCell::new(ScrollBar::new_vertical(v_bounds)));

        let ind_bounds = Rect::new(2, win_h - 1, 16, win_h);
        let indicator = Rc::new(RefCell::new(Indicator::new(ind_bounds)));

        // Create editor with scrollbar references
        let editor = Rc::new(RefCell::new(EditorWindow::with_scrollbars(
            editor_bounds,
            Some(Rc::clone(&h_scrollbar)),
            Some(Rc::clone(&v_scrollbar)),
            Some(Rc::clone(&indicator)),
        )));

        // Add gutter and editor as interior children (interior-relative coords)
        window.add(Shared::new(Rc::clone(&gutter)));
        window.add(Shared::new(Rc::clone(&editor)));

        // Add scrollbars + indicator as frame children (window-relative coords)
        let h_scrollbar_idx =
            window.add_frame_child(Box::new(Shared::new(Rc::clone(&h_scrollbar))));
        let v_scrollbar_idx =
            window.add_frame_child(Box::new(Shared::new(Rc::clone(&v_scrollbar))));
        let indicator_idx = window.add_frame_child(Box::new(Shared::new(Rc::clone(&indicator))));

        indicator.borrow_mut().set_value(Point::new(1, 1), false);

        let mut ide_win = Self {
            window,
            editor,
            gutter,
            v_scrollbar,
            h_scrollbar,
            indicator,
            file_path: Rc::new(RefCell::new(None)),
            last_mtime: RefCell::new(None),
            save_wildcard: save_wildcard.to_string(),
            default_title: title.to_string(),
            last_titled_path: RefCell::new(None),
            h_scrollbar_idx,
            v_scrollbar_idx,
            indicator_idx,
            build_error: RefCell::new(None),
            line_profile: None,
            asm_correlate_line: RefCell::new(None),
            pending_correlate_line: RefCell::new(None),
        };

        ide_win.window.set_focus(true);
        // Disable shadow — IDE windows are tiled, shadows waste space
        ide_win.window.set_state_flag(State::SHADOW, false);
        ide_win
    }

    pub fn editor_rc(&self) -> Rc<RefCell<EditorWindow>> {
        Rc::clone(&self.editor)
    }

    pub fn gutter_rc(&self) -> Rc<RefCell<BreakpointGutter>> {
        Rc::clone(&self.gutter)
    }

    pub fn file_path_rc(&self) -> Rc<RefCell<Option<PathBuf>>> {
        Rc::clone(&self.file_path)
    }

    pub fn set_text(&self, text: &str) {
        self.editor.borrow_mut().set_text(text);
    }

    /// Sync the window title with the current file path. Cheap to call every
    /// frame: writes only when the path has changed since the last sync.
    fn sync_title_from_file_path(&mut self) {
        let current = self.file_path.borrow().clone();
        if *self.last_titled_path.borrow() == current {
            return;
        }
        let title = match current
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
        {
            Some(name) => name.to_string(),
            None => self.default_title.clone(),
        };
        self.window.set_title(&title);
        *self.last_titled_path.borrow_mut() = current;
    }

    /// Sync the gutter scroll position with the editor's viewport offset.
    pub fn sync_gutter_scroll(&self) {
        let delta_y = self.editor.borrow().get_delta().y;
        self.gutter
            .borrow_mut()
            .set_top_line(delta_y.max(0) as usize);
    }

    /// Sync frame child positions after resize. Frame children are
    /// window-relative, so everything is laid out in the window's extent.
    fn sync_frame_children_positions(&mut self) {
        let extent = self.window.extent();
        let win_w = extent.width();
        let win_h = extent.height();

        if win_h >= 3 {
            let h_bounds = Rect::new(
                18i16.min(win_w.saturating_sub(2)),
                win_h - 1,
                win_w - 2,
                win_h,
            );
            self.window
                .update_frame_child(self.h_scrollbar_idx, h_bounds);
        }

        if win_w >= 3 && win_h >= 4 {
            let v_bounds = Rect::new(win_w - 1, 1, win_w, win_h - 2);
            self.window
                .update_frame_child(self.v_scrollbar_idx, v_bounds);
        }

        if win_h >= 3 {
            let ind_bounds = Rect::new(2, win_h - 1, 16i16.min(win_w - 2), win_h);
            self.window
                .update_frame_child(self.indicator_idx, ind_bounds);
        }
    }

    /// Lay the gutter and editor out side by side across the interior.
    /// `Group::set_bounds` only moves children by their grow bits, and the
    /// gutter must stay fixed-width, so both are positioned explicitly here.
    /// Cheap to call every frame: writes only when the bounds differ.
    fn sync_interior_layout(&mut self) {
        let extent = self.window.extent();
        let interior_w = extent.width().saturating_sub(2);
        let interior_h = extent.height().saturating_sub(2);
        if interior_w == 0 || interior_h == 0 {
            return;
        }
        let gutter_bounds = Rect::new(0, 0, GUTTER_WIDTH, interior_h);
        if self.gutter.borrow().bounds() != gutter_bounds {
            self.gutter.borrow_mut().set_bounds(gutter_bounds);
        }
        let col = self.profile_col_width();
        let editor_bounds = Rect::new(GUTTER_WIDTH + col, 0, interior_w, interior_h);
        if self.editor.borrow().bounds() != editor_bounds {
            self.editor.borrow_mut().set_bounds(editor_bounds);
        }
    }

    /// Paint `bg` behind every cell of interior row `visible_row` (gutter and
    /// editor columns), keeping the glyphs and foreground already drawn.
    fn highlight_interior_row(&self, terminal: &mut Terminal, visible_row: i16, bg: TvColor) {
        let extent = self.window.extent();
        let row_y = 1 + visible_row; // inside the top frame line
        let x_start = 1i16; // inside the left frame line
        let x_end = extent.b.x - 1; // stop before the right frame line
        for x in x_start..x_end {
            if let Some(existing) = terminal.read_cell(x, row_y) {
                terminal.write_cell(
                    x,
                    row_y,
                    Cell::new(existing.ch, Attr::new(existing.attr.fg, bg)),
                );
            }
        }
    }

    pub fn set_line_profile(&mut self, p: Option<LineProfile>) {
        self.line_profile = p;
    }

    pub fn has_line_profile(&self) -> bool {
        self.line_profile.is_some()
    }

    pub fn profile_column_visible(&self) -> bool {
        self.line_profile.as_ref().is_some_and(|p| p.visible)
    }

    pub fn set_profile_column_visible(&mut self, on: bool) {
        if let Some(p) = self.line_profile.as_mut() {
            p.visible = on;
        }
    }

    /// Width of the profile column currently occupying interior space.
    fn profile_col_width(&self) -> i16 {
        if self.profile_column_visible() {
            PROFILE_COL_WIDTH
        } else {
            0
        }
    }

    /// Drop the profile if the buffer changed since it was taken.
    fn drop_profile_if_edited(&mut self) {
        let Some(p) = self.line_profile.as_ref() else {
            return;
        };
        let now = text_hash(&self.editor.borrow().get_text());
        if now != p.text_hash {
            self.line_profile = None;
        }
    }

    /// Paint the profile column and the heat tint for every visible row.
    fn draw_profile_overlay(&self, terminal: &mut Terminal) {
        let Some(p) = self.line_profile.as_ref() else {
            return;
        };
        if !p.visible {
            return;
        }
        let extent = self.window.extent();
        let interior_h = extent.height().saturating_sub(2);
        let scroll_y = self.editor.borrow().get_delta().y.max(0) as usize;
        let col_x = 1 + GUTTER_WIDTH; // after the frame line and the gutter
        let text_attr = Attr::new(TvColor::LightGray, TvColor::Rgb { r: 0, g: 0, b: 100 });
        for row in 0..interior_h {
            let line = scroll_y + row as usize + 1;
            let (self_ns, _hits) = p.lines.get(&line).copied().unwrap_or((0, 0));
            let share = if p.total_ns == 0 {
                0.0
            } else {
                self_ns as f64 / p.total_ns as f64
            };
            let heat = heat_bucket(share);
            let bg = match heat {
                Heat::None => None,
                Heat::Cold => Some(TvColor::Rgb { r: 110, g: 0, b: 0 }),
                Heat::Warm => Some(TvColor::Red),
                Heat::Hot => Some(TvColor::LightRed),
            };
            if let Some(bg) = bg {
                self.highlight_interior_row(terminal, row, bg);
                if heat == Heat::Hot {
                    // Bright rows get white text so the tint stays legible.
                    let y = 1 + row;
                    for x in (col_x + PROFILE_COL_WIDTH)..(extent.b.x - 1) {
                        if let Some(c) = terminal.read_cell(x, y) {
                            terminal.write_cell(
                                x,
                                y,
                                Cell::new(c.ch, Attr::new(TvColor::White, bg)),
                            );
                        }
                    }
                }
            }
            let mut buf = DrawBuffer::new(PROFILE_COL_WIDTH as usize);
            let attr = match bg {
                Some(bg) if heat == Heat::Hot => Attr::new(TvColor::White, bg),
                Some(bg) => Attr::new(TvColor::LightGray, bg),
                None => text_attr,
            };
            buf.move_str(0, &format_share(self_ns, p.total_ns), attr);
            write_line_to_terminal(terminal, col_x, 1 + row, &buf);
        }
    }
}

impl GroupLike for IdeEditorWindow {
    fn group(&self) -> &Group {
        self.window.group()
    }
    fn group_mut(&mut self) -> &mut Group {
        self.window.group_mut()
    }
}

impl WindowLike for IdeEditorWindow {
    fn window(&self) -> &Window {
        &self.window
    }
    fn window_mut(&mut self) -> &mut Window {
        &mut self.window
    }
}

impl_view_for_window!(IdeEditorWindow {
    fn set_bounds(&mut self, bounds: Rect) {
        self.window_set_bounds(bounds);
        self.sync_interior_layout();
    }

    fn draw(&mut self, terminal: &mut Terminal) {
        self.sync_title_from_file_path();
        self.sync_frame_children_positions();
        self.sync_interior_layout();
        self.sync_gutter_scroll();
        self.window_draw(terminal);

        // Overlays are drawn in the window's own space (owner-relative
        // coordinates): row 0 is the top frame line, so interior row `r`
        // lands on `1 + r`.
        let interior_h = self.window.extent().height() - 2;
        let scroll_y = self.editor.borrow().get_delta().y.max(0) as usize;

        // Profile column and heat tint first; the error and exec overlays
        // below paint over it because they are more urgent.
        self.draw_profile_overlay(terminal);

        // Overlay error-line highlight FIRST, so a debugger exec line on
        // the same row paints over it (the program counter is more
        // immediately relevant than a stale build error).
        let error_line = self.build_error.borrow().as_ref().map(|(l, _)| *l);
        if let Some(error_line) = error_line
            && error_line > scroll_y
        {
            let visible_row = (error_line - scroll_y - 1) as i16;
            if visible_row >= 0 && visible_row < interior_h {
                self.highlight_interior_row(terminal, visible_row, TvColor::Red);
            }
        }

        // Overlay the Disassembly double-click correlation line (cyan),
        // between the build-error (red) and exec-line (green) overlays —
        // a live debug session's current statement is more relevant than
        // a stale double-click, so exec_line still paints on top of this.
        let correlate_line = *self.asm_correlate_line.borrow();
        if let Some(correlate_line) = correlate_line
            && correlate_line > scroll_y
        {
            let visible_row = (correlate_line - scroll_y - 1) as i16;
            if visible_row >= 0 && visible_row < interior_h {
                self.highlight_interior_row(terminal, visible_row, TvColor::Cyan);
            }
        }

        // Overlay execution line highlight on top of the editor area.
        // The gutter already shows ► but we also paint the entire line's
        // background green so the current statement is clearly visible.
        let exec_line = self.gutter.borrow().current_exec_line();
        // exec_line is 1-based, scroll_y is 0-based top line
        if let Some(exec_line) = exec_line
            && exec_line > scroll_y
        {
            let visible_row = (exec_line - scroll_y - 1) as i16;
            if visible_row >= 0 && visible_row < interior_h {
                self.highlight_interior_row(terminal, visible_row, TvColor::Green);
            }
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        // Forward mouse events to scrollbars (matching EditWindow pattern).
        // Window::handle_event does NOT dispatch to frame_children, so without
        // this the scrollbars would be purely decorative. `dispatch_to_child`
        // translates the mouse position into the scrollbar's own space.
        if event.what == EventType::MouseDown
            || event.what == EventType::MouseMove
            || event.what == EventType::MouseUp
        {
            let mut scrollbar_handled = false;

            if let Some(child) = self.window.get_frame_child_mut(self.h_scrollbar_idx) {
                dispatch_to_child(&mut **child, event);
                if event.what == EventType::Nothing {
                    scrollbar_handled = true;
                }
            }

            if !scrollbar_handled
                && let Some(child) = self.window.get_frame_child_mut(self.v_scrollbar_idx)
            {
                dispatch_to_child(&mut **child, event);
                if event.what == EventType::Nothing {
                    scrollbar_handled = true;
                }
            }

            if scrollbar_handled {
                self.editor.borrow_mut().sync_from_scrollbars();
                return;
            }
        }

        // A double-click on a source line correlates it with the
        // Disassembly window — peek at the event without consuming it, so
        // the editor's own double-click-selects-word behaviour (handled
        // below by `self.window.handle_event`) still runs normally.
        if event.what == EventType::MouseDown && event.mouse.double_click {
            // Event position is window-relative (frame at 0); the editor
            // sits at interior-relative `editor_bounds`, one cell inside.
            let editor_bounds = self.editor.borrow().bounds();
            let pos = event.mouse.pos;
            let (ix, iy) = (pos.x - 1, pos.y - 1);
            if ix >= editor_bounds.a.x
                && ix < editor_bounds.b.x
                && iy >= 0
                && iy < editor_bounds.height()
            {
                let scroll_y = self.editor.borrow().get_delta().y.max(0) as usize;
                let line = scroll_y + iy as usize + 1;
                *self.asm_correlate_line.borrow_mut() = Some(line);
                *self.pending_correlate_line.borrow_mut() = Some(line);
            }
        }
        let was_keyboard = event.what == EventType::Keyboard;
        self.window_handle_event(event);

        // An edit invalidates the profile. Only keyboard events can change
        // the text, so the hash is checked there and never per frame.
        if event.what == EventType::Nothing && was_keyboard && self.line_profile.is_some() {
            self.drop_profile_if_edited();
        }

        // The inner Window's frame turns close-button clicks into CM_CLOSE.
        // Translate to CM_CLOSE_EDITOR so the IDE can show a save prompt before
        // removing this window (other windows leave plain CM_CLOSE alone).
        if event.what == EventType::Command && event.command == CM_CLOSE {
            *event = Event::command(CM_CLOSE_EDITOR);
            return;
        }

        // After a resize the gutter must keep its fixed width and the editor
        // take the rest; the plain grow-bit cascade would not do that.
        self.sync_interior_layout();
    }

    fn set_focus(&mut self, focused: bool) {
        self.window_set_focus(focused);
        // The window base only propagates focus to its interior children and
        // marks itself ACTIVE, never FOCUSED. The IDE event loop relies on
        // is_focused() to find the window to close, so set the flag here.
        self.set_state_flag(State::FOCUSED, focused);
    }
});

// ── Trait impls ──────────────────────────────────────────

impl Editor for IdeEditorWindow {
    fn valid_close(&mut self, app: &mut Application, command: CommandId) -> bool {
        confirm_save_on_close(self, app, command)
    }

    fn undo(&mut self) {
        self.editor.borrow_mut().undo();
    }

    fn redo(&mut self) {
        self.editor.borrow_mut().redo();
    }

    fn can_undo(&self) -> bool {
        self.editor.borrow().can_undo()
    }

    fn can_redo(&self) -> bool {
        self.editor.borrow().can_redo()
    }

    fn cut(&mut self) -> bool {
        self.editor.borrow_mut().clip_cut()
    }

    fn copy(&mut self) -> bool {
        self.editor.borrow_mut().clip_copy()
    }

    fn paste(&mut self) -> bool {
        self.editor.borrow_mut().clip_paste()
    }

    fn select_all(&mut self) {
        self.editor.borrow_mut().select_all();
    }

    fn clear_selection(&mut self) {
        self.editor.borrow_mut().delete_selection();
    }

    fn has_selection(&self) -> bool {
        self.editor.borrow().has_selection()
    }
}

impl FileEditor for IdeEditorWindow {
    fn file_path(&self) -> Option<PathBuf> {
        self.file_path.borrow().clone()
    }

    fn set_file_path(&mut self, path: Option<PathBuf>) {
        *self.file_path.borrow_mut() = path;
    }

    fn is_dirty(&self) -> bool {
        self.editor.borrow().is_modified()
    }

    fn save(&mut self) -> std::io::Result<()> {
        let path = match self.file_path.borrow().clone() {
            Some(p) => p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no filename set; use save_as",
                ));
            }
        };
        self.editor.borrow_mut().save_file()?;
        *self.last_mtime.borrow_mut() = read_mtime(&path);
        Ok(())
    }

    fn save_as(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.editor.borrow_mut().save_as(&path)?;
        *self.last_mtime.borrow_mut() = read_mtime(&path);
        *self.file_path.borrow_mut() = Some(path);
        Ok(())
    }

    fn load(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.editor.borrow_mut().load_file(&path)?;
        *self.last_mtime.borrow_mut() = read_mtime(&path);
        *self.file_path.borrow_mut() = Some(path);
        self.gutter.borrow_mut().clear_breakpoints();
        self.line_profile = None;
        Ok(())
    }

    fn new_buffer(&mut self) {
        self.editor.borrow_mut().set_text("");
        self.editor.borrow_mut().clear_modified();
        *self.file_path.borrow_mut() = None;
        *self.last_mtime.borrow_mut() = None;
        self.gutter.borrow_mut().clear_breakpoints();
        self.line_profile = None;
    }

    fn last_known_mtime(&self) -> Option<SystemTime> {
        *self.last_mtime.borrow()
    }

    fn poll_external_changes(&self) -> ExternalState {
        let path = match self.file_path.borrow().clone() {
            Some(p) => p,
            None => return ExternalState::NoFile,
        };
        match read_mtime(&path) {
            Some(disk) => match *self.last_mtime.borrow() {
                Some(known) if disk == known => ExternalState::Unchanged,
                _ => ExternalState::Modified,
            },
            None => ExternalState::Deleted,
        }
    }

    fn reload(&mut self) -> std::io::Result<()> {
        let path = match self.file_path.borrow().clone() {
            Some(p) => p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no filename to reload",
                ));
            }
        };
        self.editor.borrow_mut().load_file(&path)?;
        *self.last_mtime.borrow_mut() = read_mtime(&path);
        self.line_profile = None;
        Ok(())
    }

    fn prompt_save_as(&mut self, app: &mut Application) -> bool {
        let (tw, th) = app.terminal.size();
        let dw = 64i16.min(tw - 4);
        let dh = 18i16.min(th - 4);
        let x = (tw - dw) / 2;
        let y = (th - dh) / 2;
        let bounds = Rect::new(x, y, x + dw, y + dh);
        let mut dialog = FileDialogBuilder::new()
            .bounds(bounds)
            .title("Save As")
            .wildcard(self.save_wildcard.clone())
            .button_label("~S~ave")
            .hidden_toggle(true)
            .build();
        match dialog.execute(app) {
            Some(path) => self.save_as(path).is_ok(),
            None => false,
        }
    }
}

impl IdeFileEditor for IdeEditorWindow {
    fn set_highlighter(&mut self, highlighter: Box<dyn SyntaxHighlighter>) {
        self.editor.borrow_mut().set_highlighter(highlighter);
    }

    fn toggle_breakpoint(&mut self, line: usize) {
        self.gutter.borrow_mut().toggle_breakpoint(line);
    }

    fn clear_breakpoints(&mut self) {
        self.gutter.borrow_mut().clear_breakpoints();
    }

    fn breakpoint_lines(&self) -> Vec<usize> {
        self.gutter.borrow().breakpoint_lines()
    }

    fn snap_breakpoints(&mut self, valid_lines: &[usize], total_lines: usize) {
        let valid: HashSet<usize> = valid_lines.iter().copied().collect();
        self.gutter
            .borrow_mut()
            .snap_breakpoints(&valid, total_lines);
    }

    fn current_exec_line(&self) -> Option<usize> {
        self.gutter.borrow().current_exec_line()
    }

    fn set_current_exec_line(&mut self, line: Option<usize>) {
        self.gutter.borrow_mut().set_current_exec_line(line);
    }

    fn build_error(&self) -> Option<(usize, String)> {
        self.build_error.borrow().clone()
    }

    fn set_build_error(&mut self, err: Option<(usize, String)>) {
        *self.build_error.borrow_mut() = err;
    }
}

impl IdeEditorWindow {
    /// Line currently correlated with the Disassembly window (see the
    /// double-click handling in `handle_event`). `&self` + `RefCell`
    /// because callers typically only hold a shared borrow of the editor
    /// (e.g. through `Rc<RefCell<IdeEditorWindow>>::borrow()`).
    pub fn asm_correlate_line(&self) -> Option<usize> {
        *self.asm_correlate_line.borrow()
    }

    /// Set (or clear) the correlated line directly — used to clear a
    /// stale highlight at the start of a new build.
    pub fn set_asm_correlate_line(&self, line: Option<usize>) {
        *self.asm_correlate_line.borrow_mut() = line;
    }

    /// Drain the latest double-click target. Called by the IDE event
    /// loop, which pushes it into the Disassembly window's own highlight.
    pub fn take_pending_correlate_line(&self) -> Option<usize> {
        self.pending_correlate_line.borrow_mut().take()
    }
}

fn read_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}
