/// Type-aware value editor — a small modal dialog the IDE pops when the
/// user double-clicks a watch row. The dialog filters keypresses to the
/// character class that matches the variable's `VarType`, validates on OK,
/// and returns either the entered value or `None` on cancel/invalid.
///
/// Lives in `bruto-ide` (not in the language crate) so any language plugged
/// into this IDE framework gets the value editor for free.
use std::cell::RefCell;
use std::rc::Rc;

use turbo_vision::app::Application;
use turbo_vision::core::command::{CM_CANCEL, CM_OK};
use turbo_vision::core::geometry::Rect;
use turbo_vision::views::button::Button;
use turbo_vision::views::dialog::Dialog;
use turbo_vision::views::input_line::InputLine;
use turbo_vision::views::label::Label;
use turbo_vision::views::validator::{FilterValidator, ValidatorRef};

use crate::debugger::VarType;

/// Show the dialog. Returns `Some(entered)` on OK with non-empty content,
/// `None` on cancel or an empty submission. The caller is responsible for
/// turning `entered` into an `expr` literal via [`format_setter_expr`].
pub fn prompt_set_value(
    app: &mut Application,
    name: &str,
    ty: VarType,
    current: &str,
) -> Option<String> {
    let (tw, th) = app.terminal.size();
    let dw = 50i16.min(tw as i16 - 4);
    let dh = 9i16;
    let x = ((tw as i16) - dw) / 2;
    let y = ((th as i16) - dh) / 2;
    let bounds = Rect::new(x, y, x + dw, y + dh);

    let title = format!("Set {name} ({})", ty.label());
    let mut dialog = Dialog::new(bounds, &title);

    dialog.add(Box::new(Label::new(Rect::new(2, 2, 12, 3), "Value:")));

    let initial = setter_initial_text(ty, current);
    let data = Rc::new(RefCell::new(initial));
    let mut input = InputLine::new(
        Rect::new(12, 2, dw - 4, 3),
        max_length_for(ty),
        Rc::clone(&data),
    );
    if let Some(v) = type_filter_validator(ty) {
        input.set_validator(v);
    }
    dialog.add(Box::new(input));

    dialog.add(Box::new(Button::new(
        Rect::new(dw - 24, dh - 4, dw - 14, dh - 2),
        "~O~K",
        CM_OK,
        true,
    )));
    dialog.add(Box::new(Button::new(
        Rect::new(dw - 12, dh - 4, dw - 2, dh - 2),
        "~C~ancel",
        CM_CANCEL,
        false,
    )));

    dialog.set_initial_focus();
    let result = dialog.execute(app);
    if result == CM_OK {
        let entered = data.borrow().clone();
        if entered.is_empty() {
            return None;
        }
        Some(entered)
    } else {
        None
    }
}

/// Strip the watch-window display formatting so the input box opens with a
/// value the user can edit directly. Today this only unwraps char quotes;
/// every other type is shown verbatim.
pub fn setter_initial_text(ty: VarType, current: &str) -> String {
    match ty {
        VarType::Char => current
            .trim_start_matches('\'')
            .trim_end_matches('\'')
            .to_string(),
        _ => current.to_string(),
    }
}

pub fn max_length_for(ty: VarType) -> usize {
    match ty {
        VarType::Char => 1,
        VarType::Boolean => 5,
        VarType::Integer => 32,
        VarType::Real => 64,
        _ => 256,
    }
}

pub fn type_filter_validator(ty: VarType) -> Option<ValidatorRef> {
    let chars = match ty {
        VarType::Integer => "0123456789-",
        VarType::Real => "0123456789-+.eE",
        VarType::Boolean => "truefalse",
        VarType::Char => return None,
        _ => return None,
    };
    Some(Rc::new(RefCell::new(FilterValidator::new(chars))))
}

/// Translate the user's typed value into a C/C++ expression suitable for
/// `expr <name> = <expr>`. Returns `None` when the typed value doesn't
/// match the target type.
pub fn format_setter_expr(ty: VarType, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    match ty {
        VarType::Integer => trimmed.parse::<i64>().ok().map(|n| n.to_string()),
        VarType::Real => trimmed.parse::<f64>().ok().map(|f| format!("{f}")),
        VarType::Boolean => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" => Some("true".to_string()),
            "false" | "0" => Some("false".to_string()),
            _ => None,
        },
        VarType::Char => {
            let mut chars = trimmed.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            let escaped: String = match c {
                '\\' => "\\\\".to_string(),
                '\'' => "\\'".to_string(),
                _ => c.to_string(),
            };
            Some(format!("'{escaped}'"))
        }
        VarType::String | VarType::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_setter_round_trips() {
        assert_eq!(
            format_setter_expr(VarType::Integer, "42"),
            Some("42".into())
        );
        assert_eq!(
            format_setter_expr(VarType::Integer, "-7"),
            Some("-7".into())
        );
        assert_eq!(format_setter_expr(VarType::Integer, "abc"), None);
    }

    #[test]
    fn real_setter_round_trips() {
        assert_eq!(format_setter_expr(VarType::Real, "3.14").unwrap(), "3.14");
        assert!(format_setter_expr(VarType::Real, "x").is_none());
    }

    #[test]
    fn boolean_setter_normalises() {
        assert_eq!(
            format_setter_expr(VarType::Boolean, "TRUE"),
            Some("true".into())
        );
        assert_eq!(
            format_setter_expr(VarType::Boolean, "0"),
            Some("false".into())
        );
        assert_eq!(format_setter_expr(VarType::Boolean, "yes"), None);
    }

    #[test]
    fn char_setter_quotes_and_rejects_multiple() {
        assert_eq!(format_setter_expr(VarType::Char, "A"), Some("'A'".into()));
        assert_eq!(format_setter_expr(VarType::Char, "'"), Some("'\\''".into()));
        assert_eq!(format_setter_expr(VarType::Char, "AB"), None);
        assert_eq!(format_setter_expr(VarType::Char, ""), None);
    }

    #[test]
    fn unsupported_types_return_none() {
        assert!(format_setter_expr(VarType::String, "hi").is_none());
        assert!(format_setter_expr(VarType::Other, "1").is_none());
    }
}
