/// Watch window — displays variable names and values during debugging.
/// Renders like StaticText views inside a Dialog (gray palette).

use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::Event;
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{write_line_to_terminal, OwnerType, View};

// Dialog-compatible colors (gray background to match dialog chrome)
const TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
const VAL_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::LightGray);
const BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);

pub struct WatchPanel {
    bounds: Rect,
    state: StateFlags,
    variables: Vec<(String, String)>,
    owner_type: OwnerType,
}

impl WatchPanel {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            state: 0,
            variables: Vec::new(),
            owner_type: OwnerType::Dialog,
        }
    }

    pub fn set_variables(&mut self, vars: Vec<(String, String)>) {
        self.variables = vars;
    }

    pub fn clear(&mut self) {
        self.variables.clear();
    }
}

impl View for WatchPanel {
    fn bounds(&self) -> Rect { self.bounds }
    fn set_bounds(&mut self, bounds: Rect) { self.bounds = bounds; }

    fn draw(&mut self, terminal: &mut Terminal) {
        let width = self.bounds.width_clamped() as usize;
        let height = self.bounds.height_clamped() as usize;

        for row in 0..height {
            let mut buf = DrawBuffer::new(width);
            buf.move_char(0, ' ', BG_ATTR, width);

            if row < self.variables.len() {
                let (name, value) = &self.variables[row];
                buf.move_str(0, name, TEXT_ATTR);
                buf.move_str(name.len(), " = ", TEXT_ATTR);
                buf.move_str(name.len() + 3, value, VAL_ATTR);
            }

            write_line_to_terminal(terminal, self.bounds.a.x, self.bounds.a.y + row as i16, &buf);
        }
    }

    fn handle_event(&mut self, _event: &mut Event) {}
    fn state(&self) -> StateFlags { self.state }
    fn set_state(&mut self, state: StateFlags) { self.state = state; }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> { None }
    fn get_owner_type(&self) -> OwnerType { self.owner_type }
    fn set_owner_type(&mut self, owner_type: OwnerType) { self.owner_type = owner_type; }
}
