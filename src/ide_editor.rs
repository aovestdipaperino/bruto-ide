/// IDE editor window — a Window containing a breakpoint gutter and a code editor side-by-side.
///
/// Follows the same pattern as turbo-vision's EditWindow but adds the gutter
/// as an interior child so that the gutter is visually part of the editor frame.

use crate::commands::CM_CLOSE_EDITOR;
use crate::gutter::{BreakpointGutter, GUTTER_WIDTH};
use crate::ide_file_editor::IdeFileEditor;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use turbo_vision::app::Application;
use turbo_vision::core::command::{CommandId, CM_CLOSE};
use turbo_vision::core::draw::Cell;
use turbo_vision::core::event::{Event, EventType};
use turbo_vision::core::geometry::{Point, Rect};
use turbo_vision::core::palette::{Attr, Palette, TvColor};
use turbo_vision::core::palette_chain::PaletteChainNode;
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::editor::EditorWindow;
use turbo_vision::views::editor_traits::{confirm_save_on_close, Editor, ExternalState, FileEditor};
use turbo_vision::views::file_dialog::FileDialogBuilder;
use turbo_vision::views::indicator::Indicator;
use turbo_vision::views::scrollbar::ScrollBar;
use turbo_vision::views::syntax::SyntaxHighlighter;
use turbo_vision::views::view::View;
use turbo_vision::views::window::Window;

// ── Rc<RefCell<...>> View wrappers (same pattern as EditWindow internals) ──

struct SharedGutter(Rc<RefCell<BreakpointGutter>>);

impl View for SharedGutter {
    fn bounds(&self) -> Rect { self.0.borrow().bounds() }
    fn set_bounds(&mut self, b: Rect) { self.0.borrow_mut().set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.0.borrow_mut().draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.0.borrow_mut().handle_event(e); }
    fn state(&self) -> StateFlags { self.0.borrow().state() }
    fn set_state(&mut self, s: StateFlags) { self.0.borrow_mut().set_state(s); }
    fn get_palette(&self) -> Option<Palette> { None }
    fn set_palette_chain(&mut self, n: Option<PaletteChainNode>) { self.0.borrow_mut().set_palette_chain(n); }
    fn get_palette_chain(&self) -> Option<&PaletteChainNode> { None }
}

struct SharedEditor(Rc<RefCell<EditorWindow>>);

impl View for SharedEditor {
    fn bounds(&self) -> Rect { self.0.borrow().bounds() }
    fn set_bounds(&mut self, b: Rect) { self.0.borrow_mut().set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.0.borrow_mut().draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.0.borrow_mut().handle_event(e); }
    fn can_focus(&self) -> bool { self.0.borrow().can_focus() }
    fn set_focus(&mut self, f: bool) { self.0.borrow_mut().set_focus(f); }
    fn is_focused(&self) -> bool { self.0.borrow().is_focused() }
    fn options(&self) -> u16 { self.0.borrow().options() }
    fn set_options(&mut self, o: u16) { self.0.borrow_mut().set_options(o); }
    fn state(&self) -> StateFlags { self.0.borrow().state() }
    fn set_state(&mut self, s: StateFlags) { self.0.borrow_mut().set_state(s); }
    fn update_cursor(&self, t: &mut Terminal) { self.0.borrow().update_cursor(t); }
    fn get_palette(&self) -> Option<Palette> { self.0.borrow().get_palette() }
    fn set_palette_chain(&mut self, n: Option<PaletteChainNode>) { self.0.borrow_mut().set_palette_chain(n); }
    fn get_palette_chain(&self) -> Option<&PaletteChainNode> { None }
}

struct SharedScrollBar(Rc<RefCell<ScrollBar>>);

impl View for SharedScrollBar {
    fn bounds(&self) -> Rect { self.0.borrow().bounds() }
    fn set_bounds(&mut self, b: Rect) { self.0.borrow_mut().set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.0.borrow_mut().draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.0.borrow_mut().handle_event(e); }
    fn get_palette(&self) -> Option<Palette> { self.0.borrow().get_palette() }
    fn set_palette_chain(&mut self, n: Option<PaletteChainNode>) { self.0.borrow_mut().set_palette_chain(n); }
    fn get_palette_chain(&self) -> Option<&PaletteChainNode> { None }
}

struct SharedIndicator(Rc<RefCell<Indicator>>);

impl View for SharedIndicator {
    fn bounds(&self) -> Rect { self.0.borrow().bounds() }
    fn set_bounds(&mut self, b: Rect) { self.0.borrow_mut().set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.0.borrow_mut().draw(t); }
    fn handle_event(&mut self, _e: &mut Event) {}
    fn get_palette(&self) -> Option<Palette> { self.0.borrow().get_palette() }
    fn set_palette_chain(&mut self, n: Option<PaletteChainNode>) { self.0.borrow_mut().set_palette_chain(n); }
    fn get_palette_chain(&self) -> Option<&PaletteChainNode> { None }
}

/// View wrapper that lets the same `IdeEditorWindow` be installed on the
/// desktop, removed (when the user clicks the close button), and re-installed
/// later (when File→Open creates a window again). The owning `Rc` lives in
/// the IDE main loop, so closing the wrapper drops only one ref and the
/// underlying `IdeEditorWindow` (with its loaded buffer) stays alive.
pub struct SharedIdeEditorWindow(pub Rc<RefCell<IdeEditorWindow>>);

impl View for SharedIdeEditorWindow {
    fn bounds(&self) -> Rect { self.0.borrow().bounds() }
    fn set_bounds(&mut self, b: Rect) { self.0.borrow_mut().set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.0.borrow_mut().draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.0.borrow_mut().handle_event(e); }
    fn can_focus(&self) -> bool { self.0.borrow().can_focus() }
    fn set_focus(&mut self, f: bool) { self.0.borrow_mut().set_focus(f); }
    fn is_focused(&self) -> bool { self.0.borrow().is_focused() }
    fn options(&self) -> u16 { self.0.borrow().options() }
    fn set_options(&mut self, o: u16) { self.0.borrow_mut().set_options(o); }
    fn state(&self) -> StateFlags { self.0.borrow().state() }
    fn set_state(&mut self, s: StateFlags) { self.0.borrow_mut().set_state(s); }
    fn get_palette(&self) -> Option<Palette> { self.0.borrow().get_palette() }
    fn set_palette_chain(&mut self, n: Option<PaletteChainNode>) { self.0.borrow_mut().set_palette_chain(n); }
    fn get_palette_chain(&self) -> Option<&PaletteChainNode> { None }
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
}

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

        // Add gutter and editor as interior children (relative coords → auto-converted)
        window.add(Box::new(SharedGutter(Rc::clone(&gutter))));
        window.add(Box::new(SharedEditor(Rc::clone(&editor))));

        // Add scrollbars + indicator as frame children
        let h_scrollbar_idx = window.add_frame_child(Box::new(SharedScrollBar(Rc::clone(&h_scrollbar))));
        let v_scrollbar_idx = window.add_frame_child(Box::new(SharedScrollBar(Rc::clone(&v_scrollbar))));
        let indicator_idx = window.add_frame_child(Box::new(SharedIndicator(Rc::clone(&indicator))));

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
        };

        ide_win.window.set_focus(true);
        // Disable shadow — IDE windows are tiled, shadows waste space
        let state = ide_win.window.state();
        ide_win.window.set_state(state & !turbo_vision::core::state::SF_SHADOW);
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
        self.gutter.borrow_mut().set_top_line(delta_y.max(0) as usize);
    }

    /// Sync frame child positions after resize.
    fn sync_frame_children_positions(&mut self) {
        let bounds = self.window.bounds();
        let win_w = bounds.width();
        let win_h = bounds.height();

        if win_h >= 3 {
            let h_bounds = Rect::new(
                bounds.a.x + 18i16.min(win_w.saturating_sub(2)),
                bounds.a.y + win_h - 1,
                bounds.a.x + win_w - 2,
                bounds.a.y + win_h,
            );
            self.window.update_frame_child(self.h_scrollbar_idx, h_bounds);
        }

        if win_w >= 3 && win_h >= 4 {
            let v_bounds = Rect::new(
                bounds.a.x + win_w - 1,
                bounds.a.y + 1,
                bounds.a.x + win_w,
                bounds.a.y + win_h - 2,
            );
            self.window.update_frame_child(self.v_scrollbar_idx, v_bounds);
        }

        if win_h >= 3 {
            let ind_bounds = Rect::new(
                bounds.a.x + 2,
                bounds.a.y + win_h - 1,
                bounds.a.x + 16i16.min(win_w - 2),
                bounds.a.y + win_h,
            );
            self.window.update_frame_child(self.indicator_idx, ind_bounds);
        }
    }
}

impl View for IdeEditorWindow {
    fn bounds(&self) -> Rect { self.window.bounds() }
    fn set_bounds(&mut self, bounds: Rect) { self.window.set_bounds(bounds); }

    fn draw(&mut self, terminal: &mut Terminal) {
        self.sync_title_from_file_path();
        self.sync_frame_children_positions();
        self.sync_gutter_scroll();
        self.window.draw(terminal);

        // Overlay execution line highlight on top of the editor area.
        // The gutter already shows ► but we also paint the entire line's
        // background green so the current statement is clearly visible.
        let exec_line = self.gutter.borrow().current_exec_line();
        if let Some(exec_line) = exec_line {
            let scroll_y = self.editor.borrow().get_delta().y.max(0) as usize;
            // exec_line is 1-based, scroll_y is 0-based top line
            if exec_line > scroll_y {
                let visible_row = (exec_line - scroll_y - 1) as i16;
                let bounds = self.window.bounds();
                let interior_h = bounds.height() - 2;

                if visible_row >= 0 && visible_row < interior_h {
                    let highlight_bg = TvColor::Green;

                    // Highlight the gutter columns for this row
                    let gutter_x = bounds.a.x + 1;
                    let row_y = bounds.a.y + 1 + visible_row;
                    for col in 0..GUTTER_WIDTH {
                        let x = gutter_x + col;
                        if let Some(existing) = terminal.read_cell(x, row_y) {
                            terminal.write_cell(
                                x as u16,
                                row_y as u16,
                                Cell::new(existing.ch, Attr::new(existing.attr.fg, highlight_bg)),
                            );
                        }
                    }

                    // Highlight the editor columns for this row
                    let editor_x = gutter_x + GUTTER_WIDTH;
                    let editor_end = bounds.b.x - 1; // stop before right frame
                    for x in editor_x..editor_end {
                        if let Some(existing) = terminal.read_cell(x, row_y) {
                            terminal.write_cell(
                                x as u16,
                                row_y as u16,
                                Cell::new(existing.ch, Attr::new(existing.attr.fg, highlight_bg)),
                            );
                        }
                    }
                }
            }
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        // Forward mouse events to scrollbars (matching EditWindow pattern).
        // Window::handle_event does NOT dispatch to frame_children, so without
        // this the scrollbars would be purely decorative.
        if event.what == EventType::MouseDown
            || event.what == EventType::MouseMove
            || event.what == EventType::MouseUp
        {
            let mut scrollbar_handled = false;

            if let Some(child) = self.window.get_frame_child_mut(self.h_scrollbar_idx) {
                child.handle_event(event);
                if event.what == EventType::Nothing {
                    scrollbar_handled = true;
                }
            }

            if !scrollbar_handled {
                if let Some(child) = self.window.get_frame_child_mut(self.v_scrollbar_idx) {
                    child.handle_event(event);
                    if event.what == EventType::Nothing {
                        scrollbar_handled = true;
                    }
                }
            }

            if scrollbar_handled {
                self.editor.borrow_mut().sync_from_scrollbars();
                return;
            }
        }

        let old_bounds = self.window.bounds();

        self.window.handle_event(event);

        // The inner Window's frame turns close-button clicks into CM_CLOSE.
        // Translate to CM_CLOSE_EDITOR so the IDE can show a save prompt before
        // removing this window (other windows leave plain CM_CLOSE alone).
        if event.what == EventType::Command && event.command == CM_CLOSE {
            *event = Event::command(CM_CLOSE_EDITOR);
            return;
        }

        // After resize/move, recalculate gutter and editor bounds.
        // Group::set_bounds applies the same width delta to ALL children, but the
        // gutter must stay fixed-width — so we override both here.
        let new_bounds = self.window.bounds();
        if old_bounds != new_bounds {
            let win_w = new_bounds.width();
            let win_h = new_bounds.height();
            let interior_w = win_w.saturating_sub(2);
            let interior_h = win_h.saturating_sub(2);

            if interior_w > 0 && interior_h > 0 {
                let interior_a = Point::new(new_bounds.a.x + 1, new_bounds.a.y + 1);

                self.gutter.borrow_mut().set_bounds(Rect::new(
                    interior_a.x,
                    interior_a.y,
                    interior_a.x + GUTTER_WIDTH,
                    interior_a.y + interior_h,
                ));

                self.editor.borrow_mut().set_bounds(Rect::new(
                    interior_a.x + GUTTER_WIDTH,
                    interior_a.y,
                    interior_a.x + interior_w,
                    interior_a.y + interior_h,
                ));
            }
        }
    }

    fn can_focus(&self) -> bool { true }

    fn set_focus(&mut self, focused: bool) {
        self.window.set_focus(focused);
        // Window::set_focus only propagates focus to its interior children and
        // never toggles SF_FOCUSED on itself. The IDE event loop relies on
        // is_focused() to find the window to close, so we set the flag here
        // (state forwards to the inner window).
        let s = self.window.state();
        self.window.set_state(
            if focused { s | turbo_vision::core::state::SF_FOCUSED }
            else { s & !turbo_vision::core::state::SF_FOCUSED },
        );
    }

    fn is_focused(&self) -> bool {
        self.window.is_focused()
    }

    fn options(&self) -> u16 { self.window.options() }
    fn set_options(&mut self, o: u16) { self.window.set_options(o); }
    fn state(&self) -> StateFlags { self.window.state() }
    fn set_state(&mut self, s: StateFlags) { self.window.set_state(s); }

    fn get_palette(&self) -> Option<Palette> { self.window.get_palette() }

    fn set_palette_chain(&mut self, n: Option<PaletteChainNode>) { self.window.set_palette_chain(n); }
    fn get_palette_chain(&self) -> Option<&PaletteChainNode> { self.window.get_palette_chain() }
}

// ── Trait impls ──────────────────────────────────────────

impl Editor for IdeEditorWindow {
    fn valid_close(&mut self, app: &mut Application, command: CommandId) -> bool {
        confirm_save_on_close(self, app, command)
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
        Ok(())
    }

    fn new_buffer(&mut self) {
        self.editor.borrow_mut().set_text("");
        self.editor.borrow_mut().clear_modified();
        *self.file_path.borrow_mut() = None;
        *self.last_mtime.borrow_mut() = None;
        self.gutter.borrow_mut().clear_breakpoints();
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
        Ok(())
    }

    fn prompt_save_as(&mut self, app: &mut Application) -> bool {
        let (tw, th) = app.terminal.size();
        let dw = 64i16.min(tw as i16 - 4);
        let dh = 18i16.min(th as i16 - 4);
        let x = ((tw as i16) - dw) / 2;
        let y = ((th as i16) - dh) / 2;
        let bounds = Rect::new(x, y, x + dw, y + dh);
        let mut dialog = FileDialogBuilder::new()
            .bounds(bounds)
            .title("Save As")
            .wildcard(self.save_wildcard.clone())
            .button_label("~S~ave")
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
        self.gutter.borrow_mut().snap_breakpoints(&valid, total_lines);
    }

    fn current_exec_line(&self) -> Option<usize> {
        self.gutter.borrow().current_exec_line()
    }

    fn set_current_exec_line(&mut self, line: Option<usize>) {
        self.gutter.borrow_mut().set_current_exec_line(line);
    }
}

fn read_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}
