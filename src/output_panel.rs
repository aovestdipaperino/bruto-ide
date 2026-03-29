/// Output panel — a modeless Dialog containing a TerminalWidget.
///
/// Uses Dialog (gray palette) instead of Window (blue palette) so the
/// frame chrome looks neutral. The TerminalWidget inside uses black background.

use std::cell::RefCell;
use std::rc::Rc;

use turbo_vision::core::event::Event;
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::Palette;
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::dialog::Dialog;
use turbo_vision::views::terminal_widget::TerminalWidget;
use turbo_vision::views::view::{OwnerType, View};

/// Rc wrapper so TerminalWidget can be both a Dialog child and accessed externally.
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
    fn get_owner_type(&self) -> OwnerType { self.0.borrow().get_owner_type() }
    fn set_owner_type(&mut self, t: OwnerType) { self.0.borrow_mut().set_owner_type(t); }
}

pub struct OutputPanel {
    dialog: Dialog,
    terminal: Rc<RefCell<TerminalWidget>>,
}

impl OutputPanel {
    pub fn new(bounds: Rect, title: &str) -> Self {
        let mut dialog = Dialog::new(bounds, title);
        // Remove shadow — the output panel sits at the bottom of the screen
        // and the shadow causes constrain_to_limits to shift it up.
        use turbo_vision::core::state::SF_SHADOW;
        let state = dialog.state();
        dialog.set_state(state & !SF_SHADOW);

        let interior_w = bounds.width() - 2;
        let interior_h = bounds.height() - 2;
        let terminal = Rc::new(RefCell::new(TerminalWidget::new(
            Rect::new(0, 0, interior_w, interior_h),
        )));
        dialog.add(Box::new(SharedTerminal(Rc::clone(&terminal))));

        Self { dialog, terminal }
    }

    pub fn terminal_rc(&self) -> Rc<RefCell<TerminalWidget>> {
        Rc::clone(&self.terminal)
    }
}

impl View for OutputPanel {
    fn bounds(&self) -> Rect { self.dialog.bounds() }
    fn set_bounds(&mut self, b: Rect) { self.dialog.set_bounds(b); }
    fn draw(&mut self, t: &mut Terminal) { self.dialog.draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.dialog.handle_event(e); }
    fn can_focus(&self) -> bool { true }
    fn set_focus(&mut self, f: bool) { self.dialog.set_focus(f); }
    fn is_focused(&self) -> bool { self.dialog.is_focused() }
    fn options(&self) -> u16 { self.dialog.options() }
    fn set_options(&mut self, o: u16) { self.dialog.set_options(o); }
    fn state(&self) -> StateFlags { self.dialog.state() }
    fn set_state(&mut self, s: StateFlags) { self.dialog.set_state(s); }
    fn get_palette(&self) -> Option<Palette> { self.dialog.get_palette() }
    fn set_owner(&mut self, owner: *const dyn View) { self.dialog.set_owner(owner); }
    fn get_owner_type(&self) -> OwnerType { self.dialog.get_owner_type() }
    fn set_owner_type(&mut self, t: OwnerType) { self.dialog.set_owner_type(t); }
}
