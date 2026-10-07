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
///
/// Layout: each function starts after a blank row with its name flush
/// left, branch-target labels are indented two columns, instructions sit
/// in a mnemonic / operands / comment column grid, and whenever the
/// mapped Pascal line changes the source text of that line is
/// interleaved above the instructions it produced (Turbo Debugger's CPU
/// view style). Interleaved source rows and labels are clickable like
/// instructions.
use std::collections::HashSet;

use bruto_lang::disasm::{AsmKind, AsmLine};
use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::{
    Event, EventType, KB_DOWN, KB_END, KB_HOME, KB_PGDN, KB_PGUP, KB_UP, MB_LEFT_BUTTON,
};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::{Grow, Options};
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
// Syntax colours for the listing (all on the grey dialog background).
const FUNC_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::LightGray);
const BLOCK_ATTR: Attr = Attr::new(TvColor::Magenta, TvColor::LightGray);
const DATA_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::LightGray);
const DIRECTIVE_ATTR: Attr = Attr::new(TvColor::Brown, TvColor::LightGray);
const DIM_ATTR: Attr = Attr::new(TvColor::DarkGray, TvColor::LightGray);
const SOURCE_ATTR: Attr = Attr::new(TvColor::Red, TvColor::LightGray);

// Column grid, in cells from the left edge (the margin is column 0).
const LABEL_X: usize = 2;
const BLOCK_X: usize = 4;
const MNEMONIC_X: usize = 8;
const OPERANDS_X: usize = 16;
const COMMENT_X: usize = 44;

/// One display row: an assembly line, an interleaved Pascal source line,
/// or a spacer between functions / sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Asm(usize),
    Source(usize),
    Blank,
}

pub struct DisasmPanel {
    core: ViewCore,
    lines: Vec<AsmLine>,
    /// Pascal source of the build, one entry per line, for the
    /// interleaved source rows.
    source: Vec<String>,
    /// Display rows derived from `lines` by [`build_rows`].
    rows: Vec<Row>,
    /// Shown as a highlighted first row when set (e.g. "Retail build —
    /// no source mapping"); doesn't count toward `rows` indices.
    notice: Option<String>,
    /// Index into `rows` of the first visible row.
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
    /// Row (index into `rows`) last clicked in the text area. Drained by
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
        // Stretch with the window's interior when it is resized or zoomed.
        core.grow_mode = Grow::HI_X | Grow::HI_Y;
        Self {
            core,
            lines: Vec::new(),
            source: Vec::new(),
            rows: Vec::new(),
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
    pub fn set_lines(&mut self, lines: Vec<AsmLine>, source: &str, notice: Option<String>) {
        self.rows = build_rows(&lines);
        self.lines = lines;
        self.source = source.lines().map(str::to_string).collect();
        self.notice = notice;
        self.top = 0;
        // A new listing invalidates any correlation to the old one.
        self.correlate_line = None;
    }

    /// Clear the listing (called when a build starts) so stale assembly
    /// isn't shown while a new one compiles or if it fails.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.source.clear();
        self.rows.clear();
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
        if let Some(row) = (0..self.rows.len()).find(|&r| self.source_line_at(r) == Some(src_line))
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

    /// Pascal source line mapped to display row `row`, if any.
    pub fn source_line_at(&self, row: usize) -> Option<usize> {
        match self.rows.get(row)? {
            Row::Asm(i) => self.lines[*i].source_line,
            Row::Source(line) => Some(*line),
            Row::Blank => None,
        }
    }

    fn visible_rows(&self) -> usize {
        self.extent().height_clamped() as usize
    }

    fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.visible_rows().max(1))
    }

    fn scroll_by(&mut self, delta: isize) {
        let new_top = (self.top as isize + delta).max(0) as usize;
        self.top = new_top.min(self.max_top());
    }
}

impl DisasmPanel {
    /// Lay out display row `idx` into `buf` on the column grid.
    /// `override_attr` (debugger / correlation highlight) replaces every
    /// syntax colour on the row.
    fn draw_row(&self, buf: &mut DrawBuffer, idx: usize, override_attr: Option<Attr>) {
        let pick = |attr: Attr| override_attr.unwrap_or(attr);
        match self.rows[idx] {
            Row::Blank => {}
            Row::Source(line) => {
                let text = self.source.get(line - 1).map_or("", |s| s.trim());
                let number = format!("{line:>4}: ");
                buf.move_str(LABEL_X, &number, pick(DIM_ATTR));
                buf.move_str(LABEL_X + number.len(), text, pick(SOURCE_ATTR));
            }
            Row::Asm(i) => {
                let line = &self.lines[i];
                let end = match line.kind {
                    AsmKind::Section => {
                        let text = format!("section {}", line.operands);
                        buf.move_str(LABEL_X, &text, pick(DIM_ATTR));
                        LABEL_X + text.len()
                    }
                    AsmKind::Function | AsmKind::Block | AsmKind::Data => {
                        let (x, attr) = match line.kind {
                            AsmKind::Function => (LABEL_X, FUNC_ATTR),
                            AsmKind::Block => (BLOCK_X, BLOCK_ATTR),
                            _ => (LABEL_X, DATA_ATTR),
                        };
                        let text = format!("{}:", line.mnemonic);
                        buf.move_str(x, &text, pick(attr));
                        x + text.chars().count()
                    }
                    AsmKind::Instruction | AsmKind::Directive => {
                        let attr = if line.kind == AsmKind::Directive {
                            DIRECTIVE_ATTR
                        } else {
                            TEXT_ATTR
                        };
                        buf.move_str(MNEMONIC_X, &line.mnemonic, pick(attr));
                        let ops_x = OPERANDS_X.max(MNEMONIC_X + line.mnemonic.len() + 1);
                        buf.move_str(ops_x, &line.operands, pick(TEXT_ATTR));
                        ops_x + line.operands.chars().count()
                    }
                };
                if let Some(comment) = &line.comment {
                    let x = COMMENT_X.max(end + 2);
                    buf.move_str(x, &format!("; {comment}"), pick(DIM_ATTR));
                }
            }
        }
    }
}

/// Turn the parsed listing into display rows: a blank spacer before each
/// function and section switch, and a [`Row::Source`] above an
/// instruction whenever its Pascal line differs from the last one shown
/// in the same function.
fn build_rows(lines: &[AsmLine]) -> Vec<Row> {
    let mut rows = Vec::with_capacity(lines.len() * 5 / 4);
    let mut last_source = None;
    for (i, line) in lines.iter().enumerate() {
        if matches!(line.kind, AsmKind::Function | AsmKind::Section) {
            if !rows.is_empty() {
                rows.push(Row::Blank);
            }
            last_source = None;
        }
        if line.kind == AsmKind::Instruction
            && let Some(src) = line.source_line.filter(|&s| Some(s) != last_source)
        {
            rows.push(Row::Source(src));
            last_source = Some(src);
        }
        rows.push(Row::Asm(i));
    }
    rows
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
        // Growing the window can leave `top` past the last full page;
        // pull it back so the extra rows show listing, not blank space.
        self.top = self.top.min(self.max_top());

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
            if let Some(src_line) = self.source_line_at(idx) {
                let has_bp = self.breakpoint_lines.contains(&src_line);
                let highlighted = self.highlighted_line == Some(src_line);
                let correlated = self.correlate_line == Some(src_line);
                // Margin: debugger line (green) beats a plain breakpoint
                // marker (red) beats nothing. Text: debugger line beats
                // the double-click correlation (cyan) beats syntax
                // colouring — independent of the margin, so a breakpoint
                // on the correlated line still shows its red margin with
                // cyan text.
                let margin_attr = if highlighted {
                    HL_BG_ATTR
                } else if has_bp {
                    BP_ATTR
                } else {
                    MARGIN_ATTR
                };
                let row_attr = if highlighted {
                    Some(HL_TEXT_ATTR)
                } else if correlated {
                    Some(CORR_TEXT_ATTR)
                } else {
                    None
                };
                if let Some(attr) = row_attr {
                    buf.move_char(margin_w, ' ', attr, width.saturating_sub(margin_w));
                }
                buf.move_char(
                    0,
                    if has_bp { '\u{25A0}' } else { ' ' },
                    margin_attr,
                    margin_w,
                );
                self.draw_row(&mut buf, idx, row_attr);
            } else if self.rows.get(idx).is_some() {
                buf.move_char(0, ' ', MARGIN_ATTR, margin_w);
                self.draw_row(&mut buf, idx, None);
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
                if self.source_line_at(idx).is_none() {
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

    /// An instruction row, or a directive when `text` starts with `.` —
    /// directives get no interleaved source row, so tests that count
    /// rows can use them for a 1:1 line-to-row listing.
    fn asm(text: &str, source_line: Option<usize>) -> AsmLine {
        let kind = if text.starts_with('.') {
            AsmKind::Directive
        } else {
            AsmKind::Instruction
        };
        AsmLine {
            kind,
            mnemonic: text.to_string(),
            operands: String::new(),
            comment: None,
            source_line,
        }
    }

    fn panel_with(lines: Vec<AsmLine>, height: i16) -> DisasmPanel {
        let mut p = DisasmPanel::new(Rect::new(0, 0, 40, height));
        p.set_lines(lines, "", None);
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
        let mut p = panel_with(vec![asm(".long 1", Some(5)), asm(".long 2", Some(6))], 5);
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
        let lines = (0..20).map(|i| asm(".long", Some(i + 1))).collect();
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
            "",
            Some("Retail build".into()),
        );
        p.handle_event(&mut mouse_down(2, 0));
        assert_eq!(p.take_pending_jump(), None);
        assert_eq!(p.take_pending_toggle(), None);
    }

    #[test]
    fn source_line_at_reports_mapping() {
        let p = panel_with(vec![asm(".a", Some(3)), asm(".b", None)], 5);
        assert_eq!(p.source_line_at(0), Some(3));
        assert_eq!(p.source_line_at(1), None);
        assert_eq!(p.source_line_at(99), None);
    }

    #[test]
    fn set_correlate_line_scrolls_matching_row_into_view() {
        let lines = (0..20).map(|i| asm(".long", Some(i + 1))).collect();
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
        p.set_lines(lines, "", None);
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

    fn label(kind: AsmKind, name: &str, source_line: Option<usize>) -> AsmLine {
        AsmLine {
            kind,
            mnemonic: name.to_string(),
            operands: String::new(),
            comment: None,
            source_line,
        }
    }

    #[test]
    fn source_rows_are_interleaved_when_the_pascal_line_changes() {
        let rows = build_rows(&[
            label(AsmKind::Function, "_main", Some(4)),
            asm("sub", Some(4)),
            asm("str", Some(4)),
            label(AsmKind::Block, "LBB0_1", Some(5)),
            asm("ldr", Some(5)),
            asm("ret", None),
        ]);
        assert_eq!(
            rows,
            vec![
                Row::Asm(0),
                Row::Source(4),
                Row::Asm(1),
                Row::Asm(2),
                Row::Asm(3),
                Row::Source(5),
                Row::Asm(4),
                Row::Asm(5),
            ]
        );
    }

    #[test]
    fn functions_and_sections_are_separated_by_blank_rows() {
        let rows = build_rows(&[
            label(AsmKind::Function, "_a", None),
            asm("ret", None),
            label(AsmKind::Function, "_b", None),
            asm("ret", None),
        ]);
        assert_eq!(
            rows,
            vec![
                Row::Asm(0),
                Row::Asm(1),
                Row::Blank,
                Row::Asm(2),
                Row::Asm(3),
            ]
        );
    }

    #[test]
    fn each_function_restarts_its_source_annotation() {
        // Same Pascal line at the end of one function and the start of the
        // next still gets a fresh source row under the new header.
        let rows = build_rows(&[
            label(AsmKind::Function, "_a", Some(3)),
            asm("ret", Some(3)),
            label(AsmKind::Function, "_b", Some(3)),
            asm("ret", Some(3)),
        ]);
        assert_eq!(rows.iter().filter(|r| **r == Row::Source(3)).count(), 2);
    }

    #[test]
    fn clicking_a_source_row_jumps_to_its_line() {
        let mut p = panel_with(vec![asm("movl", Some(7))], 5);
        // Row 0 is the interleaved `7: ...` source row, row 1 the instruction.
        p.handle_event(&mut mouse_down(5, 0));
        assert_eq!(p.take_pending_jump(), Some(0));
        assert_eq!(p.source_line_at(0), Some(7));
        assert_eq!(p.source_line_at(1), Some(7));
    }

    #[test]
    fn blank_rows_ignore_clicks() {
        let mut p = panel_with(
            vec![
                label(AsmKind::Function, "_a", Some(1)),
                asm(".long", Some(1)),
                label(AsmKind::Function, "_b", Some(2)),
            ],
            5,
        );
        assert_eq!(p.rows[2], Row::Blank);
        p.handle_event(&mut mouse_down(5, 2));
        assert_eq!(p.take_pending_jump(), None);
    }

    #[test]
    fn panel_follows_window_resize() {
        use std::cell::RefCell;
        use std::rc::Rc;
        use turbo_vision::views::group::GroupLike;
        use turbo_vision::views::shared::Shared;
        use turbo_vision::views::window::{Window, WindowPaletteType};

        let panel = Rc::new(RefCell::new(DisasmPanel::new(Rect::new(0, 0, 38, 8))));
        let mut win = Window::new_with_type(
            Rect::new(0, 0, 40, 10),
            "Disassembly",
            WindowPaletteType::Gray,
        );
        win.add(Shared::new(Rc::clone(&panel)));
        win.set_bounds(Rect::new(0, 0, 80, 30));
        let extent = panel.borrow().extent();
        assert_eq!((extent.width(), extent.height()), (78, 28));
    }
}
