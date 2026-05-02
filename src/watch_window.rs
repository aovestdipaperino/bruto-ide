/// Watch window — displays variable names and values during debugging.
/// Renders like StaticText views inside a Dialog (gray palette).

use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::{Event, EventType, MB_LEFT_BUTTON};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{write_line_to_terminal, View};

use crate::debugger::VarType;

// Dialog-compatible colors (gray background to match dialog chrome)
const TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
const VAL_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::LightGray);
const BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);

pub struct WatchPanel {
    bounds: Rect,
    state: StateFlags,
    variables: Vec<(String, String, VarType)>,
    /// Set when a watch row is double-clicked. The IDE event loop drains
    /// this each tick (via [`take_pending_edit`]) and opens the value
    /// editor; we don't open the dialog here because we don't own the
    /// `Application`.
    pending_edit: Option<usize>,
}

impl WatchPanel {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            state: 0,
            variables: Vec::new(),
            pending_edit: None,
        }
    }

    pub fn set_variables(&mut self, vars: Vec<(String, String, VarType)>) {
        self.variables = vars;
    }

    pub fn clear(&mut self) {
        self.variables.clear();
    }

    /// Look up the variable shown on `row`, returning `(name, value, ty)` if
    /// the row currently holds one. Used by the IDE to populate the value
    /// editor when a row is double-clicked.
    pub fn variable_at(&self, row: usize) -> Option<&(String, String, VarType)> {
        self.variables.get(row)
    }

    /// Drain the latest double-click target. Called by the IDE event loop.
    pub fn take_pending_edit(&mut self) -> Option<usize> {
        self.pending_edit.take()
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
                let (name, value, _ty) = &self.variables[row];
                buf.move_str(0, name, TEXT_ATTR);
                buf.move_str(name.len(), " = ", TEXT_ATTR);
                buf.move_str(name.len() + 3, value, VAL_ATTR);
            }

            write_line_to_terminal(terminal, self.bounds.a.x, self.bounds.a.y + row as i16, &buf);
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        if event.what == EventType::MouseDown
            && event.mouse.double_click
            && (event.mouse.buttons & MB_LEFT_BUTTON != 0)
        {
            let mx = event.mouse.pos.x;
            let my = event.mouse.pos.y;
            if mx >= self.bounds.a.x
                && mx < self.bounds.b.x
                && my >= self.bounds.a.y
                && my < self.bounds.b.y
            {
                let row = (my - self.bounds.a.y) as usize;
                if row < self.variables.len() {
                    self.pending_edit = Some(row);
                    event.clear();
                }
            }
        }
    }
    fn state(&self) -> StateFlags { self.state }
    fn set_state(&mut self, state: StateFlags) { self.state = state; }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> { None }
}
