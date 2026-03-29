/// Output panel — a Window with a black background palette.
///
/// Uses the turbo-vision 1.0.4 owner chain palette traversal to override
/// the Window's palette. App palette positions 64-71 are reserved for
/// black-background window colors. The Window's get_palette() maps to
/// those positions, and the Frame + all children resolve through it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Once;

use turbo_vision::core::event::Event;
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{palettes, Palette};
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::terminal_widget::TerminalWidget;
use turbo_vision::views::view::{OwnerType, View};
use turbo_vision::views::window::Window;

/// App palette positions 64-71: black-background window colors.
const BLACK_WINDOW_PALETTE_START: usize = 64;

/// Attr bytes for the black window palette entries.
/// Format: (fg << 0) | (bg << 4) using CGA color indices.
const BLACK_WINDOW_ATTRS: [u8; 8] = [
    0x07, // 64: interior/background — LightGray on Black
    0x0F, // 65: frame active border — White on Black
    0x0A, // 66: frame icon — LightGreen on Black
    0x08, // 67: scrollbar page — DarkGray on Black
    0x08, // 68: scrollbar arrows — DarkGray on Black
    0x07, // 69: editor normal — LightGray on Black
    0x0F, // 70: editor selected — White on Black
    0x00, // 71: reserved
];

/// Window palette that maps indices 1-8 to app palette positions 64-71.
const CP_BLACK_WINDOW: [u8; 8] = [64, 65, 66, 67, 68, 69, 70, 71];

static INIT_PALETTE: Once = Once::new();

/// Extend the app palette with black window entries (idempotent).
fn ensure_black_palette() {
    INIT_PALETTE.call_once(|| {
        let mut pal = palettes::get_app_palette();
        // Extend to at least position 71 (index 71, 0-based 70)
        while pal.len() <= BLACK_WINDOW_PALETTE_START + BLACK_WINDOW_ATTRS.len() {
            pal.push(0);
        }
        for (i, &attr) in BLACK_WINDOW_ATTRS.iter().enumerate() {
            pal[BLACK_WINDOW_PALETTE_START + i] = attr;
        }
        palettes::set_custom_palette(Some(pal));
    });
}

/// Rc wrapper so TerminalWidget can be both a Window child and accessed externally.
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
    fn set_owner(&mut self, o: *const dyn View) { self.0.borrow_mut().set_owner(o); }
    fn get_owner(&self) -> Option<*const dyn View> { self.0.borrow().get_owner() }
    fn get_owner_type(&self) -> OwnerType { self.0.borrow().get_owner_type() }
    fn set_owner_type(&mut self, t: OwnerType) { self.0.borrow_mut().set_owner_type(t); }
}

pub struct OutputPanel {
    window: Window,
    terminal: Rc<RefCell<TerminalWidget>>,
}

impl OutputPanel {
    pub fn new(bounds: Rect, title: &str) -> Self {
        ensure_black_palette();

        let mut window = Window::new(bounds, title);

        // Remove shadow — the output panel sits at the bottom of the screen
        use turbo_vision::core::state::SF_SHADOW;
        let state = window.state();
        window.set_state(state & !SF_SHADOW);

        let interior_w = bounds.width() - 2;
        let interior_h = bounds.height() - 2;
        let terminal = Rc::new(RefCell::new(TerminalWidget::new(
            Rect::new(0, 0, interior_w, interior_h),
        )));
        window.add(Box::new(SharedTerminal(Rc::clone(&terminal))));

        Self { window, terminal }
    }

    pub fn terminal_rc(&self) -> Rc<RefCell<TerminalWidget>> {
        Rc::clone(&self.terminal)
    }
}

impl View for OutputPanel {
    fn bounds(&self) -> Rect { self.window.bounds() }
    fn set_bounds(&mut self, b: Rect) { self.window.set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.window.draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.window.handle_event(e); }
    fn can_focus(&self) -> bool { true }
    fn set_focus(&mut self, f: bool) { self.window.set_focus(f); }
    fn is_focused(&self) -> bool { self.window.is_focused() }
    fn options(&self) -> u16 { self.window.options() }
    fn set_options(&mut self, o: u16) { self.window.set_options(o); }
    fn state(&self) -> StateFlags { self.window.state() }
    fn set_state(&mut self, s: StateFlags) { self.window.set_state(s); }

    fn get_palette(&self) -> Option<Palette> {
        // Return the black window palette — the owner chain traversal in
        // map_color() will use this for the Frame and all children.
        Some(Palette::from_slice(&CP_BLACK_WINDOW))
    }

    fn set_owner(&mut self, owner: *const dyn View) { self.window.set_owner(owner); }
    fn get_owner(&self) -> Option<*const dyn View> { self.window.get_owner() }
    fn get_owner_type(&self) -> OwnerType { self.window.get_owner_type() }
    fn set_owner_type(&mut self, t: OwnerType) { self.window.set_owner_type(t); }
}
