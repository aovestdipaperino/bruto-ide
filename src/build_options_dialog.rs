/// Build Options dialog — Build → Options… lets the user pick Debug vs
/// Retail and, for Retail builds, whether to optimize for size, speed, or
/// a balance of both.
///
/// turbo-vision's `RadioButton` is a single item that neither deselects its
/// siblings nor can be read back out of a `Dialog`, so this module carries a
/// small Borland-style `TRadioButtons` equivalent: one view, several rows,
/// selection held in a shared `Rc<Cell<usize>>` the caller reads after OK.
use std::cell::Cell;
use std::rc::Rc;

use bruto_lang::language::{BuildOptions, BuildProfile, OptimizeFor};
use turbo_vision::app::Application;
use turbo_vision::core::command::{CM_CANCEL, CM_OK};
use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::{Event, EventType, KB_DOWN, KB_UP};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{CLUSTER_FOCUSED, CLUSTER_NORMAL, CLUSTER_SHORTCUT};
use turbo_vision::core::state::Options;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::View;
use turbo_vision::views::button::Button;
use turbo_vision::views::checkbox::CheckBox;
use turbo_vision::views::dialog::Dialog;
use turbo_vision::views::group::GroupLike;
use turbo_vision::views::label::Label;
use turbo_vision::views::static_text::StaticText;
use turbo_vision::views::view::{ViewCore, write_line_to_terminal};

const PROFILES: [BuildProfile; 2] = [BuildProfile::Debug, BuildProfile::Retail];
const GOALS: [OptimizeFor; 3] = [OptimizeFor::Size, OptimizeFor::Both, OptimizeFor::Speed];

/// Show the dialog seeded with `current`. Returns the new options on OK,
/// `None` on Cancel / Esc.
pub fn prompt_build_options(app: &mut Application, current: BuildOptions) -> Option<BuildOptions> {
    let (tw, th) = app.terminal.size();
    let dw = 48i16.min(tw - 4);
    let dh = 14i16;
    let x = ((tw) - dw) / 2;
    let y = ((th) - dh) / 2;
    let mut dialog = Dialog::new(Rect::new(x, y, x + dw, y + dh), "Build Options");

    let profile = Rc::new(Cell::new(index_of(&PROFILES, current.profile)));
    let goal = Rc::new(Cell::new(index_of(&GOALS, current.optimize)));

    let col2 = dw / 2;
    let profile_id = dialog.add(RadioGroup::new(
        Rect::new(3, 2, col2 - 1, 4),
        &["~D~ebug", "~R~etail"],
        Rc::clone(&profile),
    ));
    let goal_id = dialog.add(RadioGroup::new(
        Rect::new(col2 + 1, 2, dw - 3, 5),
        &["~S~ize", "~B~oth", "S~p~eed"],
        Rc::clone(&goal),
    ));

    let mut profile_label = Label::new(Rect::new(2, 1, col2 - 1, 2), "Build ~m~ode");
    profile_label.set_link(profile_id);
    dialog.add(profile_label);
    let mut goal_label = Label::new(Rect::new(col2, 1, dw - 2, 2), "~O~ptimize for");
    goal_label.set_link(goal_id);
    dialog.add(goal_label);

    let obfuscate = dialog.add_typed(CheckBox::new(
        Rect::new(2, 5, dw - 2, 6),
        "Obfuscate ~c~ode",
    ));
    if let Some(cb) = dialog.group_mut().get_mut::<CheckBox>(obfuscate) {
        cb.set_checked(current.obfuscate);
    }

    dialog.add(StaticText::new(
        Rect::new(2, 6, dw - 2, 7),
        "Optimization applies to Retail builds only; obfuscation to any.",
    ));

    dialog.add(Button::new(
        Rect::new(dw - 28, dh - 4, dw - 18, dh - 2),
        "O~K~",
        CM_OK,
        true,
    ));
    dialog.add(Button::new(
        Rect::new(dw - 16, dh - 4, dw - 4, dh - 2),
        "Cancel",
        CM_CANCEL,
        false,
    ));

    dialog.set_initial_focus();
    if dialog.execute(app) != CM_OK {
        return None;
    }
    let obfuscate = dialog
        .group_mut()
        .get_mut::<CheckBox>(obfuscate)
        .map(|cb| cb.is_checked())
        .unwrap_or(false);
    Some(BuildOptions {
        profile: PROFILES[profile.get()],
        optimize: GOALS[goal.get()],
        obfuscate,
    })
}

fn index_of<T: PartialEq>(items: &[T], value: T) -> usize {
    items.iter().position(|i| *i == value).unwrap_or(0)
}

/// A vertical group of mutually exclusive choices, one per row. Up/Down
/// (or clicking a row, or the row's Alt-less shortcut letter while
/// focused) moves the selection; the selected index lives in `selected`.
struct RadioGroup {
    core: ViewCore,
    items: Vec<String>,
    selected: Rc<Cell<usize>>,
}

impl RadioGroup {
    fn new(bounds: Rect, items: &[&str], selected: Rc<Cell<usize>>) -> Self {
        let mut core = ViewCore::new(bounds);
        core.options |= Options::SELECTABLE;
        Self {
            core,
            items: items.iter().map(|s| s.to_string()).collect(),
            selected,
        }
    }

    fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected.set(index);
        }
    }

    /// Lower-cased letter wrapped in `~x~` for row `i`, if any.
    fn shortcut(&self, i: usize) -> Option<char> {
        let label = &self.items[i];
        let start = label.find('~')?;
        label[start + 1..]
            .chars()
            .next()
            .map(|c| c.to_ascii_lowercase())
    }
}

impl View for RadioGroup {
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
        let normal = self.map_color(CLUSTER_NORMAL);
        let focused = self.map_color(CLUSTER_FOCUSED);
        let shortcut = self.map_color(CLUSTER_SHORTCUT);
        let selected = self.selected.get();

        for (i, label) in self.items.iter().enumerate() {
            let y = i as i16;
            if y >= extent.b.y {
                break;
            }
            let color = if self.is_focused() && i == selected {
                focused
            } else {
                normal
            };
            let marker = if i == selected { "(•) " } else { "( ) " };
            let mut buf = DrawBuffer::new(width);
            buf.move_char(0, ' ', normal, width);
            buf.move_str(0, marker, color);
            buf.move_str_with_shortcut(marker.chars().count(), label, color, shortcut);
            write_line_to_terminal(terminal, 0, y, &buf);
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        match event.what {
            EventType::MouseDown if self.extent().contains(event.mouse.pos) => {
                let row = event.mouse.pos.y as usize;
                self.select(row);
                event.clear();
            }
            EventType::Keyboard if self.is_focused() => {
                let current = self.selected.get();
                match event.key_code {
                    KB_UP => {
                        self.select(current.saturating_sub(1));
                        event.clear();
                    }
                    KB_DOWN => {
                        self.select(current + 1);
                        event.clear();
                    }
                    k if k == ' ' as u16 => event.clear(),
                    k => {
                        let ch = (k & 0xff) as u8 as char;
                        let hit = (0..self.items.len())
                            .find(|&i| self.shortcut(i) == Some(ch.to_ascii_lowercase()));
                        if let Some(i) = hit {
                            self.select(i);
                            event.clear();
                        }
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
        use turbo_vision::core::palette::{Palette, palettes};
        Some(Palette::from_slice(palettes::CP_CLUSTER))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turbo_vision::core::event::Event;
    use turbo_vision::views::checkbox::CheckBox;

    #[test]
    fn checkbox_state_read_back_after_add_typed() {
        let mut dialog = Dialog::new(Rect::new(0, 0, 40, 10), "Build Options");
        let handle = dialog.add_typed(CheckBox::new(Rect::new(2, 2, 20, 3), "Obfuscate code"));
        let cb = dialog.group_mut().get_mut::<CheckBox>(handle).unwrap();
        assert!(!cb.is_checked());
        cb.toggle();
        assert!(cb.is_checked());
    }

    fn group(selected: usize) -> (RadioGroup, Rc<Cell<usize>>) {
        let cell = Rc::new(Cell::new(selected));
        let mut g = RadioGroup::new(
            Rect::new(10, 5, 30, 8),
            &["~S~ize", "~B~oth", "S~p~eed"],
            Rc::clone(&cell),
        );
        g.set_focus(true);
        (g, cell)
    }

    #[test]
    fn arrows_move_selection_and_clamp() {
        let (mut g, cell) = group(0);
        g.handle_event(&mut Event::keyboard(KB_UP));
        assert_eq!(cell.get(), 0);
        g.handle_event(&mut Event::keyboard(KB_DOWN));
        g.handle_event(&mut Event::keyboard(KB_DOWN));
        g.handle_event(&mut Event::keyboard(KB_DOWN));
        assert_eq!(cell.get(), 2);
    }

    #[test]
    fn shortcut_letter_selects_row() {
        let (mut g, cell) = group(0);
        g.handle_event(&mut Event::keyboard('p' as u16));
        assert_eq!(cell.get(), 2);
        g.handle_event(&mut Event::keyboard('B' as u16));
        assert_eq!(cell.get(), 1);
    }

    #[test]
    fn unfocused_group_ignores_keys() {
        let (mut g, cell) = group(1);
        g.set_focus(false);
        g.handle_event(&mut Event::keyboard(KB_DOWN));
        assert_eq!(cell.get(), 1);
    }

    #[test]
    fn index_of_maps_options() {
        assert_eq!(index_of(&PROFILES, BuildProfile::Retail), 1);
        assert_eq!(index_of(&GOALS, OptimizeFor::Speed), 2);
    }
}
