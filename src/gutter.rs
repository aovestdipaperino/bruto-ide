/// Breakpoint gutter — a single-column View to the left of the editor.
///
/// Displays breakpoint markers (red ■) and the current execution line (►).
/// Mouse clicks toggle breakpoints. Does not paint its own background —
/// the editor's window frame fills that area.

use std::collections::HashSet;
use turbo_vision::core::draw::Cell;
use turbo_vision::core::event::{Event, EventType, MB_LEFT_BUTTON};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{OwnerType, View};

/// Width of the gutter in characters.
pub const GUTTER_WIDTH: i16 = 1;

const BP_ATTR: Attr = Attr::new(TvColor::LightRed, TvColor::Red);
const EXEC_ATTR: Attr = Attr::new(TvColor::Yellow, TvColor::Blue);

pub struct BreakpointGutter {
    bounds: Rect,
    state: StateFlags,
    breakpoints: HashSet<usize>,
    top_line: usize,
    current_exec_line: Option<usize>,
    owner_type: OwnerType,
}

impl BreakpointGutter {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            state: 0,
            breakpoints: HashSet::new(),
            top_line: 0,
            current_exec_line: None,
            owner_type: OwnerType::Window,
        }
    }

    pub fn toggle_breakpoint(&mut self, line: usize) {
        if !self.breakpoints.remove(&line) {
            self.breakpoints.insert(line);
        }
    }

    pub fn has_breakpoint(&self, line: usize) -> bool {
        self.breakpoints.contains(&line)
    }

    pub fn breakpoints(&self) -> &HashSet<usize> {
        &self.breakpoints
    }

    pub fn breakpoint_lines(&self) -> Vec<usize> {
        self.breakpoints.iter().copied().collect()
    }

    pub fn set_top_line(&mut self, line: usize) {
        self.top_line = line;
    }

    pub fn current_exec_line(&self) -> Option<usize> {
        self.current_exec_line
    }

    pub fn set_current_exec_line(&mut self, line: Option<usize>) {
        self.current_exec_line = line;
    }
}

impl View for BreakpointGutter {
    fn bounds(&self) -> Rect { self.bounds }
    fn set_bounds(&mut self, bounds: Rect) { self.bounds = bounds; }

    fn draw(&mut self, terminal: &mut Terminal) {
        let height = self.bounds.height_clamped() as usize;
        let x = self.bounds.a.x as u16;

        for row in 0..height {
            let line_num = self.top_line + row + 1;
            let y = (self.bounds.a.y + row as i16) as u16;

            if self.breakpoints.contains(&line_num) {
                terminal.write_cell(x, y, Cell::new('\u{25A0}', BP_ATTR));
            } else if self.current_exec_line == Some(line_num) {
                terminal.write_cell(x, y, Cell::new('\u{25BA}', EXEC_ATTR));
            }
            // No else — don't paint empty cells, let the background show through
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        if event.what == EventType::MouseDown && (event.mouse.buttons & MB_LEFT_BUTTON != 0) {
            let mouse_x = event.mouse.pos.x;
            let mouse_y = event.mouse.pos.y;

            if mouse_x >= self.bounds.a.x
                && mouse_x < self.bounds.b.x
                && mouse_y >= self.bounds.a.y
                && mouse_y < self.bounds.b.y
            {
                let row = (mouse_y - self.bounds.a.y) as usize;
                let line_num = self.top_line + row + 1;
                self.toggle_breakpoint(line_num);
                event.clear();
            }
        }
    }

    fn state(&self) -> StateFlags { self.state }
    fn set_state(&mut self, state: StateFlags) { self.state = state; }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> { None }
    fn get_owner_type(&self) -> OwnerType { self.owner_type }
    fn set_owner_type(&mut self, owner_type: OwnerType) { self.owner_type = owner_type; }
}
