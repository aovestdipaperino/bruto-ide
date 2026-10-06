/// Disassembly window — shows the compiled module's assembly listing
/// (parsed by `bruto_lang::disasm`), with a breakpoint margin mirroring
/// the editor's gutter. Grey dialog palette, like Watches / Call Stack.
///
/// Two independent click zones per row:
/// - the 1-char margin at the left edge toggles a breakpoint on the
///   row's mapped Pascal source line (same mechanism as the editor
///   gutter — see [`take_pending_toggle`]);
/// - the rest of the row navigates the source editor there (see
///   [`take_pending_jump`]).
///
/// Rows with no source mapping (no debug info in a Retail build, or
/// compiler-generated code with no Pascal counterpart) ignore both
/// clicks — there's nothing to jump to or break on.
///
/// A third, independent highlight (cyan) shows which row corresponds to
/// a line the user just double-clicked in the source editor — pushed in
/// via [`set_correlate_line`] by the IDE event loop, which reads it off
/// `IdeEditorWindow::take_pending_correlate_line`.
use std::collections::HashSet;

use bruto_lang::disasm::AsmLine;
use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::{
    Event, EventType, KB_DOWN, KB_END, KB_HOME, KB_PGDN, KB_PGUP, KB_UP, MB_LEFT_BUTTON,
};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::Options;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{View, ViewCore, write_line_to_terminal};

const TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
const BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
const MARGIN_ATTR: Attr = Attr::new(TvColor::DarkGray, TvColor::LightGray);
const BP_ATTR: Attr = Attr::new(TvColor::LightRed, TvColor::Red);
// Mirrors the editor's / call stack's "current statement" highlight.
const HL_TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::Green);
const HL_BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::Green);
const NOTICE_ATTR: Attr = Attr::new(TvColor::Yellow, TvColor::LightGray);
// Double-click-from-editor correlation highlight — distinct from the
// debugger's green "current statement" so the two never look the same.
const CORR_TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::Cyan);

pub struct DisasmPanel {
    core: ViewCore,
    lines: Vec<AsmLine>,
    /// Shown as a highlighted first row when set (e.g. "Retail build —
    /// no source mapping"); doesn't count toward `lines` indices.
    notice: Option<String>,
    /// Index into `lines` of the first visible row.
    top: usize,
    /// Pascal source lines that currently have a breakpoint, pushed in by
    /// the IDE each tick from whichever editor built this listing — purely
    /// for drawing the margin marker.
    breakpoint_lines: HashSet<usize>,
    /// Source line to draw with the green "current statement" highlight
    /// (the debugger's line while paused), pushed in each tick.
    highlighted_line: Option<usize>,
    /// Source line to draw with the cyan "correlated from a double-click
    /// in the editor" highlight, set once by the IDE event loop after
    /// draining `IdeEditorWindow::take_pending_correlate_line`.
    correlate_line: Option<usize>,
    /// Row (index into `lines`) last clicked in the text area. Drained by
    /// the IDE event loop, which navigates the source editor there.
    pending_jump: Option<usize>,
    /// Row last clicked in the margin. Drained by the IDE event loop,
    /// which toggles a breakpoint on that row's source line.
    pending_toggle: Option<usize>,
}

impl DisasmPanel {
    pub fn new(bounds: Rect) -> Self {
        let mut core = ViewCore::new(bounds);
        core.options |= Options::SELECTABLE;
        Self {
            core,
            lines: Vec::new(),
            notice: None,
            top: 0,
            breakpoint_lines: HashSet::new(),
            highlighted_line: None,
            correlate_line: None,
            pending_jump: None,
            pending_toggle: None,
        }
    }

    /// Replace the listing (called after a successful build) and reset
    /// scroll so a rebuilt program doesn't open mid-scroll into content
    /// that no longer matches.
    pub fn set_lines(&mut self, lines: Vec<AsmLine>, notice: Option<String>) {
        self.lines = lines;
        self.notice = notice;
        self.top = 0;
        // A new listing invalidates any correlation to the old one.
        self.correlate_line = None;
    }

    /// Clear the listing (called when a build starts) so stale assembly
    /// isn't shown while a new one compiles or if it fails.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.notice = None;
        self.top = 0;
        self.correlate_line = None;
    }

    pub fn set_breakpoint_lines(&mut self, lines: HashSet<usize>) {
        self.breakpoint_lines = lines;
    }

    pub fn set_highlighted_line(&mut self, line: Option<usize>) {
        self.highlighted_line = line;
        if let Some(src_line) = line {
            self.ensure_line_visible(src_line);
        }
    }

    /// Set the line correlated with a double-click in the editor and
    /// scroll it into view (same "only if not already visible" behaviour
    /// as `set_highlighted_line`, so repeated clicks on the same line
    /// don't fight manual scrolling).
    pub fn set_correlate_line(&mut self, line: Option<usize>) {
        self.correlate_line = line;
        if let Some(src_line) = line {
            self.ensure_line_visible(src_line);
        }
    }

    /// Scroll `src_line`'s row into view if it isn't already. No-op if
    /// nothing in the current listing maps to that line.
    fn ensure_line_visible(&mut self, src_line: usize) {
        if let Some(row) = self
            .lines
            .iter()
            .position(|l| l.source_line == Some(src_line))
        {
            let visible_h = self.extent().height_clamped() as usize;
            if row < self.top || row >= self.top + visible_h {
                self.top = row.saturating_sub(visible_h / 2);
            }
        }
    }

    pub fn take_pending_jump(&mut self) -> Option<usize> {
        self.pending_jump.take()
    }

    pub fn take_pending_toggle(&mut self) -> Option<usize> {
        self.pending_toggle.take()
    }

    /// Pascal source line mapped to `row`, if any.
    pub fn source_line_at(&self, row: usize) -> Option<usize> {
        self.lines.get(row).and_then(|l| l.source_line)
    }

    fn visible_rows(&self) -> usize {
        self.extent().height_clamped() as usize
    }

    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.visible_rows().max(1))
    }

    fn scroll_by(&mut self, delta: isize) {
        let new_top = (self.top as isize + delta).max(0) as usize;
        self.top = new_top.min(self.max_top());
    }
}

impl View for DisasmPanel {
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
        let extent = self.extent();
        let width = extent.width_clamped() as usize;
        let height = extent.height_clamped() as usize;
        let margin_w = 1usize;

        for row in 0..height {
            let mut buf = DrawBuffer::new(width);
            buf.move_char(0, ' ', BG_ATTR, width);

            if row == 0 {
                if let Some(notice) = &self.notice {
                    buf.move_str(0, notice, NOTICE_ATTR);
                    write_line_to_terminal(terminal, 0, 0, &buf);
                    continue;
                }
            }

            let idx = self.top + row;
            if let Some(line) = self.lines.get(idx) {
                let has_bp = line
                    .source_line
                    .is_some_and(|l| self.breakpoint_lines.contains(&l));
                let highlighted =
                    line.source_line.is_some() && line.source_line == self.highlighted_line;
                let correlated =
                    line.source_line.is_some() && line.source_line == self.correlate_line;
                // Margin: debugger line (green) beats a plain breakpoint
                // marker (red) beats nothing. Text: debugger line beats
                // the double-click correlation (cyan) beats nothing —
                // independent of the margin, so a breakpoint on the
                // correlated line still shows its red margin with cyan text.
                let margin_attr = if highlighted {
                    HL_BG_ATTR
                } else if has_bp {
                    BP_ATTR
                } else {
                    MARGIN_ATTR
                };
                let text_attr = if highlighted {
                    HL_TEXT_ATTR
                } else if correlated {
                    CORR_TEXT_ATTR
                } else {
                    TEXT_ATTR
                };
                buf.move_char(
                    0,
                    if has_bp { '\u{25A0}' } else { ' ' },
                    margin_attr,
                    margin_w,
                );
                buf.move_str(margin_w, &line.text, text_attr);
            }

            write_line_to_terminal(terminal, 0, row as i16, &buf);
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        match event.what {
            EventType::MouseDown if event.mouse.buttons & MB_LEFT_BUTTON != 0 => {
                let mx = event.mouse.pos.x;
                let my = event.mouse.pos.y;
                if !self.extent().contains(event.mouse.pos) {
                    return;
                }
                let screen_row = my as usize;
                if screen_row == 0 && self.notice.is_some() {
                    return;
                }
                let idx = self.top + screen_row;
                if idx >= self.lines.len() {
                    return;
                }
                if self.lines[idx].source_line.is_none() {
                    return; // nothing to jump to or break on
                }
                if mx == 0 {
                    self.pending_toggle = Some(idx);
                } else {
                    self.pending_jump = Some(idx);
                }
                event.clear();
            }
            EventType::Keyboard if self.is_focused() => {
                let visible_h = self.visible_rows();
                match event.key_code {
                    KB_UP => self.scroll_by(-1),
                    KB_DOWN => self.scroll_by(1),
                    KB_PGUP => self.scroll_by(-(visible_h as isize)),
                    KB_PGDN => self.scroll_by(visible_h as isize),
                    KB_HOME => self.top = 0,
                    KB_END => self.top = self.max_top(),
                    _ => return,
                }
                event.clear();
            }
            EventType::MouseWheelUp => {
                self.scroll_by(-3);
                event.clear();
            }
            EventType::MouseWheelDown => {
                self.scroll_by(3);
                event.clear();
            }
            _ => {}
        }
    }

    fn can_focus(&self) -> bool {
        true
    }

    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asm(text: &str, source_line: Option<usize>) -> AsmLine {
        AsmLine {
            text: text.to_string(),
            source_line,
        }
    }

    fn panel_with(lines: Vec<AsmLine>, height: i16) -> DisasmPanel {
        let mut p = DisasmPanel::new(Rect::new(0, 0, 40, height));
        p.set_lines(lines, None);
        p
    }

    fn mouse_down(x: i16, y: i16) -> Event {
        Event::mouse(
            EventType::MouseDown,
            turbo_vision::core::geometry::Point { x, y },
            MB_LEFT_BUTTON,
            false,
        )
    }

    #[test]
    fn click_in_text_area_sets_pending_jump() {
        let mut p = panel_with(vec![asm("movl $1, %eax", Some(5)), asm("retq", Some(6))], 5);
        p.handle_event(&mut mouse_down(5, 1));
        assert_eq!(p.take_pending_jump(), Some(1));
        assert_eq!(p.take_pending_toggle(), None);
    }

    #[test]
    fn click_in_margin_sets_pending_toggle_not_jump() {
        let mut p = panel_with(vec![asm("movl $1, %eax", Some(5))], 5);
        p.handle_event(&mut mouse_down(0, 0));
        assert_eq!(p.take_pending_toggle(), Some(0));
        assert_eq!(p.take_pending_jump(), None);
    }

    #[test]
    fn click_on_unmapped_line_does_nothing() {
        let mut p = panel_with(vec![asm(".text", None)], 5);
        p.handle_event(&mut mouse_down(0, 0));
        p.handle_event(&mut mouse_down(5, 0));
        assert_eq!(p.take_pending_toggle(), None);
        assert_eq!(p.take_pending_jump(), None);
    }

    #[test]
    fn click_respects_scroll_offset() {
        let lines = (0..20).map(|i| asm("instr", Some(i + 1))).collect();
        let mut p = panel_with(lines, 5);
        p.scroll_by(10);
        p.handle_event(&mut mouse_down(5, 2));
        // top=10, screen row 2 -> absolute index 12
        assert_eq!(p.take_pending_jump(), Some(12));
    }

    #[test]
    fn scroll_clamps_to_valid_range() {
        let lines = (0..10).map(|i| asm("instr", Some(i + 1))).collect();
        let mut p = panel_with(lines, 5);
        p.scroll_by(-100);
        assert_eq!(p.top, 0);
        p.scroll_by(100);
        assert_eq!(p.top, p.max_top());
        assert!(p.top > 0);
    }

    #[test]
    fn notice_row_absorbs_clicks_without_jump_or_toggle() {
        let mut p = panel_with(vec![asm("movl $1, %eax", Some(5))], 5);
        p.set_lines(
            vec![asm("movl $1, %eax", Some(5))],
            Some("Retail build".into()),
        );
        p.handle_event(&mut mouse_down(2, 0));
        assert_eq!(p.take_pending_jump(), None);
        assert_eq!(p.take_pending_toggle(), None);
    }

    #[test]
    fn source_line_at_reports_mapping() {
        let p = panel_with(vec![asm("a", Some(3)), asm("b", None)], 5);
        assert_eq!(p.source_line_at(0), Some(3));
        assert_eq!(p.source_line_at(1), None);
        assert_eq!(p.source_line_at(99), None);
    }

    #[test]
    fn set_correlate_line_scrolls_matching_row_into_view() {
        let lines = (0..20).map(|i| asm("instr", Some(i + 1))).collect();
        let mut p = panel_with(lines, 5);
        p.set_correlate_line(Some(15));
        // Row for source line 15 is index 14; must now be visible.
        assert!(p.top <= 14 && 14 < p.top + 5);
    }

    #[test]
    fn set_correlate_line_with_no_matching_row_leaves_scroll_unchanged() {
        let lines = vec![asm("a", Some(1)), asm("b", Some(2))];
        let mut p = panel_with(lines, 5);
        p.set_correlate_line(Some(999)); // no row maps to this line
        assert_eq!(p.top, 0);
    }

    #[test]
    fn set_correlate_line_none_clears_without_scrolling() {
        let lines = (0..20).map(|i| asm("instr", Some(i + 1))).collect();
        let mut p = panel_with(lines, 5);
        p.set_correlate_line(Some(15));
        let top_after_set = p.top;
        p.set_correlate_line(None);
        assert_eq!(p.correlate_line, None);
        assert_eq!(p.top, top_after_set); // clearing doesn't reset scroll
    }

    #[test]
    fn rebuilding_clears_any_prior_correlation() {
        let lines = vec![asm("a", Some(1))];
        let mut p = panel_with(lines.clone(), 5);
        p.set_correlate_line(Some(1));
        assert_eq!(p.correlate_line, Some(1));

        p.clear();
        assert_eq!(p.correlate_line, None);

        p.set_correlate_line(Some(1));
        p.set_lines(lines, None);
        assert_eq!(p.correlate_line, None);
    }

    #[test]
    fn highlighted_line_takes_precedence_over_correlate_for_text_color() {
        // Can't inspect drawn colors directly without a terminal, but we
        // can confirm both states coexist correctly: setting one doesn't
        // clobber the other's stored value, which is what draw()'s
        // precedence match relies on.
        let mut p = panel_with(vec![asm("a", Some(5))], 5);
        p.set_highlighted_line(Some(5));
        p.set_correlate_line(Some(5));
        assert_eq!(p.highlighted_line, Some(5));
        assert_eq!(p.correlate_line, Some(5));
    }
}
