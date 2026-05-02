/// Output panel — a Window with a black background via set_custom_palette().
///
/// Points the Window's custom palette at app-palette positions 97-104, the
/// black-window range that turbo-vision already reserves. The owner-chain
/// traversal in `map_color()` then resolves Frame and children through these
/// black attrs without clobbering anything else.
///
/// Earlier the panel used positions 64-71, but a turbo-vision update moved
/// syntax-highlighting attrs into 64-74; pointing the black window at 64-71
/// erased Normal/Keyword/.../Identifier and wrote 0 over Type, which
/// rendered as `ERROR_ATTR` in the editor. The 97-104 range is the black
/// window's intended home and leaves syntax intact.

use std::cell::RefCell;
use std::rc::Rc;

use turbo_vision::core::event::Event;
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::Palette;
use turbo_vision::core::palette_chain::PaletteChainNode;
use turbo_vision::core::state::{StateFlags, SF_SHADOW};
use turbo_vision::terminal::Terminal;
use turbo_vision::views::scrollbar::ScrollBar;
use turbo_vision::views::terminal_widget::TerminalWidget;
use turbo_vision::views::view::View;
use turbo_vision::views::window::Window;

/// Window-relative palette mapping the 8 frame/interior slots to the black
/// window range (positions 97-104) of CP_APP_COLOR.
const CP_BLACK_WINDOW: [u8; 8] = [97, 98, 99, 100, 101, 102, 103, 104];

/// Rc wrapper so TerminalWidget can be a Window child and accessed externally.
struct SharedTerminal(Rc<RefCell<TerminalWidget>>);

impl View for SharedTerminal {
    fn bounds(&self) -> Rect { self.0.borrow().bounds() }
    fn set_bounds(&mut self, b: Rect) { self.0.borrow_mut().set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.0.borrow_mut().draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.0.borrow_mut().handle_event(e); }
    fn can_focus(&self) -> bool { self.0.borrow().can_focus() }
    fn state(&self) -> StateFlags { self.0.borrow().state() }
    fn set_state(&mut self, s: StateFlags) { self.0.borrow_mut().set_state(s); }
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

pub struct OutputPanel {
    window: Window,
    terminal: Rc<RefCell<TerminalWidget>>,
    v_scrollbar: Rc<RefCell<ScrollBar>>,
    v_scrollbar_idx: usize,
}

impl OutputPanel {
    pub fn new(bounds: Rect, title: &str) -> Self {
        let mut window = Window::new(bounds, title);
        window.set_custom_palette(CP_BLACK_WINDOW.to_vec());

        let state = window.state();
        window.set_state(state & !SF_SHADOW);

        let win_w = bounds.width();
        let win_h = bounds.height();
        let interior_w = win_w - 2;
        let interior_h = win_h - 2;

        let terminal = Rc::new(RefCell::new(TerminalWidget::new(
            Rect::new(0, 0, interior_w, interior_h),
        )));
        window.add(Box::new(SharedTerminal(Rc::clone(&terminal))));

        // Vertical scrollbar on the right frame edge
        let v_bounds = Rect::new(win_w - 1, 1, win_w, win_h - 2);
        let v_scrollbar = Rc::new(RefCell::new(ScrollBar::new_vertical(v_bounds)));
        let v_scrollbar_idx = window.add_frame_child(Box::new(SharedScrollBar(Rc::clone(&v_scrollbar))));

        Self { window, terminal, v_scrollbar, v_scrollbar_idx }
    }

    /// Build a new panel that re-uses an existing `TerminalWidget` buffer —
    /// used to re-open the Output window after the user closed it without
    /// losing the build/run history that's already in the buffer.
    pub fn with_terminal(
        bounds: Rect,
        title: &str,
        terminal: Rc<RefCell<TerminalWidget>>,
    ) -> Self {
        let mut window = Window::new(bounds, title);
        window.set_custom_palette(CP_BLACK_WINDOW.to_vec());

        let state = window.state();
        window.set_state(state & !SF_SHADOW);

        let win_w = bounds.width();
        let win_h = bounds.height();
        let interior_w = win_w - 2;
        let interior_h = win_h - 2;

        // Resize the existing terminal buffer to the new window's interior
        terminal.borrow_mut().set_bounds(Rect::new(0, 0, interior_w, interior_h));
        window.add(Box::new(SharedTerminal(Rc::clone(&terminal))));

        let v_bounds = Rect::new(win_w - 1, 1, win_w, win_h - 2);
        let v_scrollbar = Rc::new(RefCell::new(ScrollBar::new_vertical(v_bounds)));
        let v_scrollbar_idx = window.add_frame_child(Box::new(SharedScrollBar(Rc::clone(&v_scrollbar))));

        Self { window, terminal, v_scrollbar, v_scrollbar_idx }
    }

    pub fn terminal_rc(&self) -> Rc<RefCell<TerminalWidget>> {
        Rc::clone(&self.terminal)
    }

    fn sync_scrollbar(&self) {
        let term = self.terminal.borrow();
        let total = term.line_count() as i32;
        let visible = term.bounds().height_clamped() as i32;
        let mut sb = self.v_scrollbar.borrow_mut();
        sb.set_params(0, 0, total.saturating_sub(visible).max(0), visible, 1);
        sb.set_total(total);
    }

    fn sync_scrollbar_positions(&mut self) {
        let bounds = self.window.bounds();
        let win_w = bounds.width();
        let win_h = bounds.height();
        if win_w >= 3 && win_h >= 4 {
            let v_bounds = Rect::new(
                bounds.a.x + win_w - 1,
                bounds.a.y + 1,
                bounds.a.x + win_w,
                bounds.a.y + win_h - 2,
            );
            self.window.update_frame_child(self.v_scrollbar_idx, v_bounds);
        }
    }
}

impl View for OutputPanel {
    fn bounds(&self) -> Rect { self.window.bounds() }
    fn set_bounds(&mut self, b: Rect) { self.window.set_bounds(b); }

    fn draw(&mut self, t: &mut Terminal) {
        self.sync_scrollbar_positions();
        self.sync_scrollbar();
        self.window.draw(t);
    }

    fn handle_event(&mut self, e: &mut Event) { self.window.handle_event(e); }
    fn can_focus(&self) -> bool { true }
    fn set_focus(&mut self, f: bool) { self.window.set_focus(f); }
    fn is_focused(&self) -> bool { self.window.is_focused() }
    fn options(&self) -> u16 { self.window.options() }
    fn set_options(&mut self, o: u16) { self.window.set_options(o); }
    fn state(&self) -> StateFlags { self.window.state() }
    fn set_state(&mut self, s: StateFlags) { self.window.set_state(s); }
    fn get_palette(&self) -> Option<Palette> { self.window.get_palette() }
    fn set_palette_chain(&mut self, n: Option<PaletteChainNode>) { self.window.set_palette_chain(n); }
    fn get_palette_chain(&self) -> Option<&PaletteChainNode> { self.window.get_palette_chain() }
    fn init_after_add(&mut self) { self.window.init_after_add(); }
    fn constrain_to_parent_bounds(&mut self) { self.window.constrain_to_parent_bounds(); }
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
}
