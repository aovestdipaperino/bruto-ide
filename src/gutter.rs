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
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{View, ViewCore};

/// Width of the gutter in characters.
pub const GUTTER_WIDTH: i16 = 1;

const GUTTER_BG: Attr = Attr::new(
    TvColor::DarkGray,
    TvColor::Rgb { r: 0, g: 0, b: 100 }, // Dark navy — noticeably darker than editor blue (0,0,170)
);
const BP_ATTR: Attr = Attr::new(TvColor::LightRed, TvColor::Red);
const EXEC_ATTR: Attr = Attr::new(TvColor::Yellow, TvColor::Blue);

pub struct BreakpointGutter {
    core: ViewCore,
    breakpoints: HashSet<usize>,
    top_line: usize,
    current_exec_line: Option<usize>,
}

impl BreakpointGutter {
    pub fn new(bounds: Rect) -> Self {
        Self {
            core: ViewCore::new(bounds),
            breakpoints: HashSet::new(),
            top_line: 0,
            current_exec_line: None,
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

    pub fn clear_breakpoints(&mut self) {
        self.breakpoints.clear();
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

    /// Snap every breakpoint to the nearest valid line (searching forward).
    /// Breakpoints that can't be mapped (past the last valid line) are removed.
    pub fn snap_breakpoints(&mut self, valid: &std::collections::HashSet<usize>, max_line: usize) {
        let old: Vec<usize> = self.breakpoints.drain().collect();
        for bp in old {
            if valid.contains(&bp) {
                self.breakpoints.insert(bp);
            } else {
                // Search forward for next valid line
                if let Some(snapped) = ((bp + 1)..=max_line).find(|l| valid.contains(l)) {
                    self.breakpoints.insert(snapped);
                }
                // If no valid line found forward, drop the breakpoint
            }
        }
    }
}

impl View for BreakpointGutter {
    fn core(&self) -> &ViewCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut ViewCore {
        &mut self.core
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn draw(&mut self, terminal: &mut Terminal) {
        // Owner-relative coordinates: the gutter draws in its own space,
        // column 0, one row per visible line.
        let height = self.extent().height_clamped() as usize;
        let x = 0i16;

        for row in 0..height {
            let line_num = self.top_line + row + 1;
            let y = row as i16;

            if self.breakpoints.contains(&line_num) {
                terminal.write_cell(x, y, Cell::new('\u{25A0}', BP_ATTR));
            } else if self.current_exec_line == Some(line_num) {
                terminal.write_cell(x, y, Cell::new('\u{25BA}', EXEC_ATTR));
            } else {
                // Error lines paint their own red bar across the editor
                // row in IdeEditorWindow.draw; the gutter stays clear so
                // it remains exclusively a breakpoint surface.
                terminal.write_cell(x, y, Cell::new(' ', GUTTER_BG));
            }
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        if event.what == EventType::MouseDown && (event.mouse.buttons & MB_LEFT_BUTTON != 0) {
            // Mouse positions arrive in the gutter's own space.
            if self.extent().contains(event.mouse.pos) {
                let row = event.mouse.pos.y as usize;
                let line_num = self.top_line + row + 1;
                self.toggle_breakpoint(line_num);
                event.clear();
            }
        }
    }

    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        None
    }
}
