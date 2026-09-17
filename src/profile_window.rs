//! Profile window — a collapsible call tree of routines and lines from the
//! last profile run. Same view pattern as `CallStackPanel`: owner-relative
//! drawing, gray dialog palette, jumps delivered through `pending_jump`.

use std::collections::HashSet;

use bruto_lang::profile::{Profile, ProfileKind};
use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::{
    Event, EventType, KB_DOWN, KB_ENTER, KB_LEFT, KB_RIGHT, KB_UP, MB_LEFT_BUTTON,
};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::Options;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{View, ViewCore, write_line_to_terminal};

const TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
const NUM_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::LightGray);
const BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
const HL_TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::Green);
const HL_NUM_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::Green);
const HL_BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::Green);

/// One visible row of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub node: usize,
    pub depth: usize,
    pub expandable: bool,
    pub expanded: bool,
}

/// Depth-first flattening of `profile` into display rows. Children are
/// sorted by total time, descending; nodes in `collapsed` hide their
/// subtree.
pub fn flatten_tree(profile: &Profile, collapsed: &HashSet<usize>) -> Vec<Row> {
    let n = profile.nodes.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for (i, node) in profile.nodes.iter().enumerate() {
        match node.parent {
            Some(p) if p < n => children[p].push(i),
            _ => roots.push(i),
        }
    }
    let by_total =
        |a: &usize, b: &usize| profile.nodes[*b].total_ns.cmp(&profile.nodes[*a].total_ns);
    roots.sort_by(by_total);
    for c in &mut children {
        c.sort_by(by_total);
    }

    let mut rows = Vec::new();
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&r| (r, 0)).collect();
    while let Some((idx, depth)) = stack.pop() {
        let expandable = !children[idx].is_empty();
        let expanded = expandable && !collapsed.contains(&idx);
        rows.push(Row {
            node: idx,
            depth,
            expandable,
            expanded,
        });
        if expanded {
            for &c in children[idx].iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }
    rows
}

fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.2}s", ns as f64 / 1e9)
    } else if ns >= 1_000_000 {
        format!("{:.1}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.1}µs", ns as f64 / 1e3)
    } else {
        format!("{ns}ns")
    }
}

/// Clip `left` so it fits within `label_max` characters, replacing the tail
/// with `…` when truncation is needed. Returns an empty string when
/// `label_max` is 0.
fn clip_label(left: &str, label_max: usize) -> String {
    if left.chars().count() <= label_max {
        return left.to_string();
    }
    if label_max == 0 {
        return String::new();
    }
    let keep = label_max.saturating_sub(1);
    let mut clipped: String = left.chars().take(keep).collect();
    clipped.push('…');
    clipped
}

pub struct ProfilePanel {
    core: ViewCore,
    profile: Option<Profile>,
    collapsed: HashSet<usize>,
    rows: Vec<Row>,
    selected: usize,
    top: usize,
    pending_jump: Option<usize>,
}

impl ProfilePanel {
    pub fn new(bounds: Rect) -> Self {
        let mut core = ViewCore::new(bounds);
        core.options |= Options::SELECTABLE;
        Self {
            core,
            profile: None,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            top: 0,
            pending_jump: None,
        }
    }

    pub fn set_profile(&mut self, p: Option<Profile>) {
        self.profile = p;
        self.collapsed.clear();
        self.selected = 0;
        self.top = 0;
        self.rebuild();
    }

    pub fn clear(&mut self) {
        self.set_profile(None);
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Source line of the selected row, if any.
    pub fn selected_line(&self) -> Option<usize> {
        let p = self.profile.as_ref()?;
        let row = self.rows.get(self.selected)?;
        Some(p.nodes[row.node].line)
    }

    /// Drain the latest jump request (a 1-based source line).
    pub fn take_pending_jump(&mut self) -> Option<usize> {
        self.pending_jump.take()
    }

    fn rebuild(&mut self) {
        self.rows = match &self.profile {
            Some(p) => flatten_tree(p, &self.collapsed),
            None => Vec::new(),
        };
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn toggle_selected(&mut self, expand: bool) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        if !row.expandable {
            return;
        }
        if expand {
            self.collapsed.remove(&row.node);
        } else {
            self.collapsed.insert(row.node);
        }
        self.rebuild();
    }

    fn ensure_visible(&mut self) {
        let h = self.extent().height_clamped() as usize;
        if h == 0 {
            return;
        }
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + h {
            self.top = self.selected + 1 - h;
        }
    }

    fn request_jump(&mut self) {
        if let Some(line) = self.selected_line() {
            self.pending_jump = Some(line);
        }
    }
}

impl View for ProfilePanel {
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
        self.ensure_visible();
        let extent = self.extent();
        let width = extent.width_clamped() as usize;
        let height = extent.height_clamped() as usize;
        let total = self
            .profile
            .as_ref()
            .map(|p| p.elapsed_ns.max(1))
            .unwrap_or(1);

        for r in 0..height {
            let mut buf = DrawBuffer::new(width);
            let idx = self.top + r;
            let Some(row) = self.rows.get(idx) else {
                buf.move_char(0, ' ', BG_ATTR, width);
                if r == 0 && self.rows.is_empty() {
                    buf.move_str(0, "No profile yet. Use Build > Profile.", TEXT_ATTR);
                }
                write_line_to_terminal(terminal, 0, r as i16, &buf);
                continue;
            };
            let p = self.profile.as_ref().unwrap();
            let node = &p.nodes[row.node];
            let selected = idx == self.selected;
            let (bg, text, num) = if selected {
                (HL_BG_ATTR, HL_TEXT_ATTR, HL_NUM_ATTR)
            } else {
                (BG_ATTR, TEXT_ATTR, NUM_ATTR)
            };
            buf.move_char(0, ' ', bg, width);

            let marker = if row.expandable {
                if row.expanded { "▾ " } else { "▸ " }
            } else {
                "  "
            };
            let label = match node.kind {
                ProfileKind::Routine => node.name.clone(),
                ProfileKind::Line => format!("line {}", node.line),
            };
            let mut left = format!("{}{}{}", " ".repeat(row.depth * 2), marker, label);

            let pct = node.total_ns as f64 * 100.0 / total as f64;
            let right = format!(
                "{pct:5.1}%  {:>8}  {:>8}  {:>6}",
                fmt_ns(node.total_ns),
                fmt_ns(node.self_ns),
                node.calls
            );
            let rlen = right.chars().count();
            let label_max = if rlen + 1 < width {
                width - rlen - 1
            } else {
                width
            };
            if left.chars().count() > label_max {
                left = clip_label(&left, label_max);
            }
            buf.move_str(0, &left, text);
            if rlen + 1 < width {
                buf.move_str(width - rlen, &right, num);
            }
            write_line_to_terminal(terminal, 0, r as i16, &buf);
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        match event.what {
            EventType::Keyboard => {
                match event.key_code {
                    KB_UP => {
                        self.selected = self.selected.saturating_sub(1);
                    }
                    KB_DOWN => {
                        if self.selected + 1 < self.rows.len() {
                            self.selected += 1;
                        }
                    }
                    KB_RIGHT => self.toggle_selected(true),
                    KB_LEFT => self.toggle_selected(false),
                    KB_ENTER => self.request_jump(),
                    _ => return,
                }
                event.clear();
            }
            EventType::MouseDown if event.mouse.buttons & MB_LEFT_BUTTON != 0 => {
                if self.extent().contains(event.mouse.pos) {
                    let idx = self.top + event.mouse.pos.y as usize;
                    if idx < self.rows.len() {
                        self.selected = idx;
                        if event.mouse.double_click {
                            self.request_jump();
                        }
                        event.clear();
                    }
                }
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
    use bruto_lang::profile::{Profile, ProfileKind, ProfileNode};

    fn node(
        kind: ProfileKind,
        name: &str,
        line: usize,
        parent: Option<usize>,
        total: u64,
    ) -> ProfileNode {
        ProfileNode {
            kind,
            name: name.into(),
            line,
            parent,
            calls: 1,
            self_ns: total / 2,
            total_ns: total,
        }
    }

    fn sample() -> Profile {
        Profile {
            elapsed_ns: 1000,
            truncated: false,
            nodes: vec![
                node(ProfileKind::Routine, "program", 1, None, 1000), // 0
                node(ProfileKind::Line, "", 10, Some(0), 100),        // 1
                node(ProfileKind::Routine, "Work", 3, Some(0), 800),  // 2
                node(ProfileKind::Line, "", 5, Some(2), 800),         // 3
                node(ProfileKind::Line, "", 11, Some(0), 50),         // 4
            ],
        }
    }

    #[test]
    fn flatten_orders_children_by_total_descending() {
        let rows = flatten_tree(&sample(), &HashSet::new());
        let order: Vec<usize> = rows.iter().map(|r| r.node).collect();
        assert_eq!(order, vec![0, 2, 3, 1, 4]);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].depth, 2);
        assert!(rows[0].expandable && rows[0].expanded);
        assert!(!rows[2].expandable, "leaf lines are not expandable");
    }

    #[test]
    fn collapsed_nodes_hide_their_subtree() {
        let mut collapsed = HashSet::new();
        collapsed.insert(2);
        let rows = flatten_tree(&sample(), &collapsed);
        let order: Vec<usize> = rows.iter().map(|r| r.node).collect();
        assert_eq!(order, vec![0, 2, 1, 4]);
        assert!(rows[1].expandable && !rows[1].expanded);
    }

    #[test]
    fn panel_navigation_and_jump() {
        use turbo_vision::core::event::{Event, KB_DOWN, KB_ENTER, KB_LEFT};
        use turbo_vision::core::geometry::Rect;
        let mut panel = ProfilePanel::new(Rect::new(0, 0, 40, 10));
        panel.set_profile(Some(sample()));
        assert_eq!(panel.selected_line(), Some(1), "program row selected first");
        panel.handle_event(&mut Event::keyboard(KB_DOWN));
        assert_eq!(panel.selected_line(), Some(3), "Work row");
        panel.handle_event(&mut Event::keyboard(KB_LEFT));
        assert_eq!(panel.row_count(), 4, "Work collapsed");
        panel.handle_event(&mut Event::keyboard(KB_ENTER));
        assert_eq!(panel.take_pending_jump(), Some(3));
        assert_eq!(panel.take_pending_jump(), None);
    }

    #[test]
    fn long_labels_are_clipped_before_the_numbers() {
        assert_eq!(clip_label("AAAAAAAA", 5), "AAAA…");
        assert_eq!(clip_label("AB", 5), "AB");
        assert_eq!(clip_label("ABC", 0), "");

        let long_name = "A".repeat(60);
        let mut panel = ProfilePanel::new(Rect::new(0, 0, 40, 3));
        panel.set_profile(Some(Profile {
            elapsed_ns: 1000,
            truncated: false,
            nodes: vec![node(ProfileKind::Routine, &long_name, 1, None, 1000)],
        }));
        // The panel should build without panicking and the label logic
        // should clip a name this long given the panel's width.
        let right = format!(
            "{pct:5.1}%  {:>8}  {:>8}  {:>6}",
            fmt_ns(1000),
            fmt_ns(500),
            1,
            pct = 100.0
        );
        let rlen = right.chars().count();
        let width = 40usize;
        let label_max = if rlen + 1 < width {
            width - rlen - 1
        } else {
            width
        };
        let left = format!("  {long_name}");
        assert!(left.chars().count() > label_max);
        let clipped = clip_label(&left, label_max);
        assert!(clipped.ends_with('…'));
        assert!(clipped.chars().count() <= label_max);
        assert_eq!(panel.row_count(), 1);
    }
}
