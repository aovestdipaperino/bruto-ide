/// IDE editor window — a Window containing a breakpoint gutter and a code editor side-by-side.
///
/// Follows the same pattern as turbo-vision's EditWindow but adds the gutter
/// as an interior child so that the gutter is visually part of the editor frame.

use crate::gutter::{BreakpointGutter, GUTTER_WIDTH};

use std::cell::RefCell;
use std::rc::Rc;

use turbo_vision::core::draw::Cell;
use turbo_vision::core::event::{Event, EventType};
use turbo_vision::core::geometry::{Point, Rect};
use turbo_vision::core::palette::{Attr, Palette, TvColor};
use turbo_vision::core::palette_chain::PaletteChainNode;
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::editor::Editor;
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

struct SharedEditor(Rc<RefCell<Editor>>);

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

// ── IdeEditorWindow ──────────────────────────────────────

/// A Window containing a breakpoint gutter on the left and a code editor on the right,
/// with scrollbars and an indicator on the frame edge (same as EditWindow).
pub struct IdeEditorWindow {
    window: Window,
    editor: Rc<RefCell<Editor>>,
    gutter: Rc<RefCell<BreakpointGutter>>,
    v_scrollbar: Rc<RefCell<ScrollBar>>,
    h_scrollbar: Rc<RefCell<ScrollBar>>,
    indicator: Rc<RefCell<Indicator>>,
    h_scrollbar_idx: usize,
    v_scrollbar_idx: usize,
    indicator_idx: usize,
}

impl IdeEditorWindow {
    pub fn new(bounds: Rect, title: &str) -> Self {
        let mut window = Window::new(bounds, title);

        let win_w = bounds.width();
        let win_h = bounds.height();
        let interior_w = win_w - 2;
        let interior_h = win_h - 2;

        // Gutter sits at the left edge of the interior (relative coords)
        let gutter_bounds = Rect::new(0, 0, GUTTER_WIDTH, interior_h);
        let gutter = Rc::new(RefCell::new(BreakpointGutter::new(gutter_bounds)));

        // Editor fills the rest of the interior, right of the gutter
        let editor_bounds = Rect::new(GUTTER_WIDTH, 0, interior_w, interior_h);

        // Scrollbars on the window frame (relative to frame)
        let h_bounds = Rect::new(18, win_h - 1, win_w - 2, win_h);
        let h_scrollbar = Rc::new(RefCell::new(ScrollBar::new_horizontal(h_bounds)));

        let v_bounds = Rect::new(win_w - 1, 1, win_w, win_h - 2);
        let v_scrollbar = Rc::new(RefCell::new(ScrollBar::new_vertical(v_bounds)));

        let ind_bounds = Rect::new(2, win_h - 1, 16, win_h);
        let indicator = Rc::new(RefCell::new(Indicator::new(ind_bounds)));

        // Create editor with scrollbar references
        let editor = Rc::new(RefCell::new(Editor::with_scrollbars(
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

    pub fn editor_rc(&self) -> Rc<RefCell<Editor>> {
        Rc::clone(&self.editor)
    }

    pub fn gutter_rc(&self) -> Rc<RefCell<BreakpointGutter>> {
        Rc::clone(&self.gutter)
    }

    pub fn set_highlighter(&self, highlighter: Box<dyn SyntaxHighlighter>) {
        self.editor.borrow_mut().set_highlighter(highlighter);
    }

    pub fn set_text(&self, text: &str) {
        self.editor.borrow_mut().set_text(text);
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
