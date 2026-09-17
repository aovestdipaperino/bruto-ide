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

use turbo_vision::core::geometry::Rect;
use turbo_vision::core::state::State;
use turbo_vision::impl_view_for_window;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::group::{Group, GroupLike};
use turbo_vision::views::scrollbar::ScrollBar;
use turbo_vision::views::shared::Shared;
use turbo_vision::views::terminal_widget::TerminalWidget;
use turbo_vision::views::view::View;
use turbo_vision::views::window::{Window, WindowLike};

/// Window-relative palette mapping the 8 frame/interior slots to the black
/// window range (positions 97-104) of CP_APP_COLOR.
const CP_BLACK_WINDOW: [u8; 8] = [97, 98, 99, 100, 101, 102, 103, 104];

pub struct OutputPanel {
    window: Window,
    terminal: Rc<RefCell<TerminalWidget>>,
    v_scrollbar: Rc<RefCell<ScrollBar>>,
    v_scrollbar_idx: usize,
}

impl OutputPanel {
    pub fn new(bounds: Rect, title: &str) -> Self {
        let interior_w = bounds.width() - 2;
        let interior_h = bounds.height() - 2;
        let terminal = Rc::new(RefCell::new(TerminalWidget::new(Rect::new(
            0, 0, interior_w, interior_h,
        ))));
        Self::with_terminal(bounds, title, terminal)
    }

    /// Build a new panel that re-uses an existing `TerminalWidget` buffer —
    /// used to re-open the Output window after the user closed it without
    /// losing the build/run history that's already in the buffer.
    pub fn with_terminal(bounds: Rect, title: &str, terminal: Rc<RefCell<TerminalWidget>>) -> Self {
        let mut window = Window::new(bounds, title);
        window.set_custom_palette(CP_BLACK_WINDOW.to_vec());
        window.set_state_flag(State::SHADOW, false);

        let win_w = bounds.width();
        let win_h = bounds.height();
        let interior_w = win_w - 2;
        let interior_h = win_h - 2;

        // (Re)size the terminal buffer to the window's interior. Children are
        // owner-relative, so the interior origin is always (0, 0).
        terminal
            .borrow_mut()
            .set_bounds(Rect::new(0, 0, interior_w, interior_h));
        window.add(Shared::new(Rc::clone(&terminal)));

        // Vertical scrollbar on the right frame edge (window-relative)
        let v_bounds = Rect::new(win_w - 1, 1, win_w, win_h - 2);
        let v_scrollbar = Rc::new(RefCell::new(ScrollBar::new_vertical(v_bounds)));
        let v_scrollbar_idx =
            window.add_frame_child(Box::new(Shared::new(Rc::clone(&v_scrollbar))));

        Self {
            window,
            terminal,
            v_scrollbar,
            v_scrollbar_idx,
        }
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
        // Frame children are window-relative: lay them out in the extent.
        let extent = self.window.extent();
        let win_w = extent.width();
        let win_h = extent.height();
        if win_w >= 3 && win_h >= 4 {
            let v_bounds = Rect::new(win_w - 1, 1, win_w, win_h - 2);
            self.window
                .update_frame_child(self.v_scrollbar_idx, v_bounds);
        }
    }
}

impl GroupLike for OutputPanel {
    fn group(&self) -> &Group {
        self.window.group()
    }
    fn group_mut(&mut self) -> &mut Group {
        self.window.group_mut()
    }
}

impl WindowLike for OutputPanel {
    fn window(&self) -> &Window {
        &self.window
    }
    fn window_mut(&mut self) -> &mut Window {
        &mut self.window
    }
}

impl_view_for_window!(OutputPanel {
    fn draw(&mut self, t: &mut Terminal) {
        self.sync_scrollbar_positions();
        self.sync_scrollbar();
        self.window_draw(t);
    }
});
