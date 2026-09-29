//! Typed form controls. Keyboard, paste and mouse use the same field operations.
use crate::model::{Field, FieldPicker};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

impl Field {
    #[cfg(test)]
    pub fn boolean(label: &str, value: bool) -> Self {
        let mut field = Self::text(label, if value { "true" } else { "false" });
        field.kind = "boolean".into();
        field
    }
    #[cfg(test)]
    pub fn choice(label: &str, value: &str, options: &[&str]) -> Self {
        let mut field = Self::text(label, value);
        field.kind = "choice".into();
        field.options = options.iter().map(|s| (*s).into()).collect();
        field
    }
    #[cfg(test)]
    pub fn integer(label: &str, value: i64, minimum: i64, maximum: i64) -> Self {
        let mut field = Self::text(label, &value.to_string());
        field.kind = "integer".into();
        field.minimum = Some(minimum);
        field.maximum = Some(maximum);
        field
    }
    #[cfg(test)]
    pub fn readonly(label: &str, value: &str) -> Self {
        let mut field = Self::text(label, value);
        field.readonly = true;
        field
    }
    pub fn is_choice(&self) -> bool {
        !self.options.is_empty()
            || matches!(
                self.kind.as_str(),
                "choice" | "enum" | "select" | "multi_choice"
            )
    }
    pub fn is_text(&self) -> bool {
        !self.readonly && self.kind != "boolean" && !self.is_choice()
    }
    pub fn choices(&self) -> Vec<usize> {
        let query = self
            .picker
            .as_ref()
            .map_or("", |p| p.query.as_str())
            .to_lowercase();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, value)| value.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect()
    }
    pub fn activate(&mut self) {
        if self.readonly {
            return;
        }
        if self.kind == "boolean" {
            self.value = (self.value != "true").to_string();
            self.clear = false;
            self.inherit = false;
        } else if self.is_choice() {
            let selected = self
                .options
                .iter()
                .position(|v| v == &self.value)
                .unwrap_or(0);
            self.picker = Some(FieldPicker {
                query: String::new(),
                selected,
            });
        }
    }
    pub fn choose(&mut self, index: usize) {
        if self.readonly {
            return;
        }
        let Some(value) = self.options.get(index).cloned() else {
            return;
        };
        if self.kind == "multi_choice" {
            let Ok(mut selected) = self.selected_choices() else {
                return;
            };
            if let Some(i) = selected.iter().position(|s| s == &value) {
                selected.remove(i);
            } else {
                selected.push(value);
            }
            self.value = json!(selected).to_string();
        } else {
            self.value = value;
            self.picker = None;
        }
        self.cursor = self.value.len();
        self.clear = false;
        self.inherit = false;
    }
    fn selected_choices(&self) -> Result<Vec<String>, String> {
        if self.value.is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&self.value)
            .map_err(|_| format!("{} requires a list of selected options", self.label))
    }
    pub fn paste(&mut self, text: &str) -> Result<(), String> {
        if self.readonly {
            return Err(format!("{} is read-only", self.label));
        }
        let text = crate::text::clean(text).replace('\n', " ");
        if self.is_choice() {
            if self.picker.is_none() {
                self.activate();
            }
            if let Some(picker) = &mut self.picker {
                picker.query.push_str(&text);
                picker.selected = 0;
            }
            return Ok(());
        }
        if self.kind == "boolean" {
            return Err("Use Space or Enter to toggle this switch".into());
        }
        self.normalize_cursor();
        let mut next = self.value.clone();
        next.insert_str(self.cursor, &text);
        if self.kind == "integer" && !integer_draft(&next) {
            return Err(format!("{} requires a whole number", self.label));
        }
        self.value = next;
        self.cursor += text.len();
        self.clear = false;
        self.inherit = false;
        Ok(())
    }
    /// Returns true when the field consumed the event, false for form navigation.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        self.normalize_cursor();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if ctrl && matches!(key.code, KeyCode::Char('s' | 'p' | 'f' | 'd' | 'k')) {
            return Ok(false);
        }
        if self.picker.is_some() {
            let choices = self.choices();
            match key.code {
                KeyCode::Esc => {
                    self.picker = None;
                    return Ok(true);
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    let picker = self.picker.as_mut().expect("open picker");
                    let delta = match key.code {
                        KeyCode::Up => -1,
                        KeyCode::PageUp => -5,
                        KeyCode::PageDown => 5,
                        _ => 1,
                    };
                    picker.selected = picker
                        .selected
                        .saturating_add_signed(delta)
                        .min(choices.len().saturating_sub(1));
                    return Ok(true);
                }
                KeyCode::Enter => {
                    if let Some(index) = self
                        .picker
                        .as_ref()
                        .and_then(|p| choices.get(p.selected))
                        .copied()
                    {
                        self.choose(index);
                    }
                    return Ok(true);
                }
                KeyCode::Backspace => {
                    let picker = self.picker.as_mut().expect("open picker");
                    pop(&mut picker.query);
                    picker.selected = 0;
                    return Ok(true);
                }
                KeyCode::Char(c) if plain => {
                    self.paste(&c.to_string())?;
                    return Ok(true);
                }
                KeyCode::Tab | KeyCode::BackTab => self.picker = None,
                _ => {}
            }
        }
        if matches!(
            key.code,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down
        ) {
            return Ok(false);
        }
        if self.readonly {
            return Ok(true);
        }
        match key.code {
            KeyCode::Char('e') if ctrl && self.allow_clear => {
                self.value.clear();
                self.cursor = 0;
                self.clear = true;
                self.inherit = false;
            }
            KeyCode::Char('u') if ctrl && self.is_text() => {
                self.value.clear();
                self.cursor = 0;
                self.clear = false;
                self.inherit = false;
            }
            KeyCode::Enter | KeyCode::Char(' ')
                if key.code == KeyCode::Enter
                    || plain && (self.kind == "boolean" || self.is_choice()) =>
            {
                self.activate();
                if self.is_text() {
                    return Ok(false);
                }
            }
            KeyCode::Left | KeyCode::Right if self.kind == "boolean" => {
                self.value = (key.code == KeyCode::Right).to_string();
                self.clear = false;
                self.inherit = false;
            }
            KeyCode::Left | KeyCode::Right if self.is_choice() => {
                if !self.options.is_empty() && self.kind != "multi_choice" {
                    let current = self
                        .options
                        .iter()
                        .position(|s| s == &self.value)
                        .unwrap_or(0);
                    let index = if key.code == KeyCode::Left {
                        (current + self.options.len() - 1) % self.options.len()
                    } else {
                        (current + 1) % self.options.len()
                    };
                    self.choose(index);
                } else {
                    self.activate();
                }
            }
            KeyCode::Left if self.is_text() => self.cursor = previous(&self.value, self.cursor),
            KeyCode::Right if self.is_text() => self.cursor = next(&self.value, self.cursor),
            KeyCode::Home if self.is_text() => self.cursor = 0,
            KeyCode::End if self.is_text() => self.cursor = self.value.len(),
            KeyCode::Backspace if self.is_text() => {
                let start = previous(&self.value, self.cursor);
                self.value.replace_range(start..self.cursor, "");
                self.cursor = start;
                self.clear = false;
                self.inherit = false;
            }
            KeyCode::Delete if self.is_text() => {
                let end = next(&self.value, self.cursor);
                self.value.replace_range(self.cursor..end, "");
                self.clear = false;
                self.inherit = false;
            }
            KeyCode::Char(c) if plain => {
                self.paste(&c.to_string())?;
            }
            _ => {}
        }
        Ok(true)
    }
    fn normalize_cursor(&mut self) {
        self.cursor = self
            .value
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(self.value.len()))
            .take_while(|&i| i <= self.cursor)
            .last()
            .unwrap_or(0);
    }
    pub fn parsed_value(&self) -> Result<Value, String> {
        if self.required && self.value.trim().is_empty() {
            return Err(format!("{} is required", self.label));
        }
        if !self.required
            && self.value.is_empty()
            && (self.is_choice() && self.kind != "multi_choice" || self.kind == "boolean")
        {
            return Ok(Value::Null);
        }
        let invalid = || format!("{} requires a valid {} value", self.label, self.kind);
        let value = match self.kind.as_str() {
            "boolean" => Value::Bool(self.value.parse().map_err(|_| invalid())?),
            "integer" => {
                let n = self
                    .value
                    .parse::<i64>()
                    .map_err(|_| format!("{} requires a whole number", self.label))?;
                if self.minimum.is_some_and(|min| n < min)
                    || self.maximum.is_some_and(|max| n > max)
                {
                    return Err(format!(
                        "{} must be in {}..{}",
                        self.label,
                        self.minimum.map_or("−∞".into(), |v| v.to_string()),
                        self.maximum.map_or("∞".into(), |v| v.to_string())
                    ));
                }
                json!(n)
            }
            "number" => {
                let value: Value = serde_json::from_str(&self.value).map_err(|_| invalid())?;
                if !value.is_number() {
                    return Err(invalid());
                }
                value
            }
            "list" | "json" => {
                let value: Value = serde_json::from_str(&self.value).map_err(|_| invalid())?;
                if self.kind == "list" && !value.is_array() {
                    return Err(invalid());
                }
                value
            }
            "multi_choice" => {
                let selected = self.selected_choices()?;
                if self.required && selected.is_empty() {
                    return Err(format!("{} is required", self.label));
                }
                json!(selected)
            }
            _ => Value::String(self.value.clone()),
        };
        if self.is_choice() {
            let selected = if self.kind == "multi_choice" {
                self.selected_choices()?
            } else {
                vec![self.value.clone()]
            };
            if selected
                .iter()
                .any(|value| !self.options.iter().any(|option| option == value))
            {
                return Err(format!("{}: choose an available option", self.label));
            }
        }
        if self.is_choice()
            && self.kind != "multi_choice"
            && let Some(canonical) = self
                .options
                .iter()
                .position(|option| option == &self.value)
                .and_then(|index| self.option_values.get(index))
        {
            return Ok(canonical.clone());
        }
        Ok(value)
    }
    pub fn display_value(&self) -> String {
        if self.inherit {
            return "Inherit configured value".into();
        }
        if self.clear {
            return "Clear explicit value".into();
        }
        if self.private {
            return if self.value.is_empty() {
                "Unchanged · value hidden".into()
            } else {
                "•".repeat(self.value.graphemes(true).count())
            };
        }
        if self.kind == "boolean" {
            return match self.value.as_str() {
                "true" => "● On",
                "false" => "○ Off",
                "" => "○ Unset",
                _ => "! Invalid value",
            }
            .into();
        }
        if self.kind == "multi_choice" {
            return match self.selected_choices() {
                Ok(values) if values.is_empty() => "None selected".into(),
                Ok(values) => json!(values).to_string(),
                Err(_) => "! Invalid selection".into(),
            };
        }
        if self.value.is_empty() {
            "(empty)".into()
        } else {
            self.value.clone()
        }
    }
}
fn integer_draft(s: &str) -> bool {
    s.strip_prefix('-')
        .unwrap_or(s)
        .chars()
        .all(|c| c.is_ascii_digit())
}
fn previous(s: &str, cursor: usize) -> usize {
    s[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}
fn next(s: &str, cursor: usize) -> usize {
    s[cursor..]
        .graphemes(true)
        .next()
        .map_or(cursor, |s| cursor + s.len())
}
fn pop(s: &mut String) {
    s.truncate(previous(s, s.len()));
}

pub fn validate(fields: &[Field]) -> Result<(), (usize, String)> {
    for (index, field) in fields.iter().enumerate() {
        if field.readonly
            || field.clear
            || field.private && field.value.is_empty()
            || field.allow_clear && field.value == field.initial
        {
            continue;
        }
        field.parsed_value().map_err(|error| (index, error))?;
    }
    Ok(())
}

#[derive(Default)]
pub struct FormGeometry {
    pub fields: Vec<(usize, Rect)>,
    pub choices: Vec<(usize, Rect)>,
    pub apply: Option<Rect>,
    pub cursor: Option<(u16, u16)>,
}
/// Render compact setting rows; an open choice picker replaces the rows in-place.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    fields: &[Field],
    selected: usize,
    status: &str,
    palette: crate::view::Palette,
) -> FormGeometry {
    let mut geometry = FormGeometry::default();
    if area.width == 0 || area.height == 0 {
        return geometry;
    }
    let line = |buf: &mut Buffer, y: u16, spans: Vec<Span<'static>>| {
        if y < area.bottom() {
            Paragraph::new(Line::from(spans)).render(Rect::new(area.x, y, area.width, 1), buf);
        }
    };
    let plain = |s: String, color| Span::styled(s, Style::default().fg(color));
    if let Some(field) = fields.get(selected).filter(|field| field.picker.is_some()) {
        let picker = field.picker.as_ref().expect("open picker");
        line(
            buf,
            area.y,
            vec![plain(
                format!(
                    "{}  ›  Choose{}",
                    field.label,
                    if field.kind == "multi_choice" {
                        " multiple"
                    } else {
                        " one"
                    }
                ),
                palette.accent,
            )],
        );
        line(
            buf,
            area.y + 1,
            vec![plain(format!("/ {}", picker.query), palette.fg)],
        );
        geometry.cursor = Some((
            area.x + (2 + picker.query.width()).min(area.width.saturating_sub(1) as usize) as u16,
            area.y + 1,
        ));
        let matches = field.choices();
        let cap = area.height.saturating_sub(4) as usize;
        let start = picker.selected.saturating_sub(cap.saturating_sub(1));
        for (rank, index) in matches.iter().enumerate().skip(start).take(cap) {
            let y = area.y + 2 + (rank - start) as u16;
            let active = rank == picker.selected;
            let checked = if field.kind == "multi_choice" {
                field
                    .selected_choices()
                    .is_ok_and(|values| values.contains(&field.options[*index]))
            } else {
                field.value == field.options[*index]
            };
            let marker = if field.kind == "multi_choice" {
                if checked { "[✓] " } else { "[ ] " }
            } else if checked {
                "✓ "
            } else {
                "  "
            };
            line(
                buf,
                y,
                vec![Span::styled(
                    format!(
                        "{} {}{}",
                        if active { "›" } else { " " },
                        marker,
                        field.options[*index]
                    ),
                    Style::default()
                        .fg(if active { palette.accent } else { palette.fg })
                        .add_modifier(if active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                )],
            );
            geometry
                .choices
                .push((*index, Rect::new(area.x, y, area.width, 1)));
        }
        if matches.is_empty() {
            line(
                buf,
                area.y + 2,
                vec![plain("  No matching options".into(), palette.muted)],
            );
        }
        if area.height >= 2 {
            line(
                buf,
                area.bottom() - 1,
                vec![plain(
                    "Type to filter · ↑↓ Move · Enter Select · Esc Back".into(),
                    palette.muted,
                )],
            );
        }
        return geometry;
    }
    let cap = area.height.saturating_sub(4).max(1) as usize;
    let start = selected
        .min(fields.len().saturating_sub(1))
        .saturating_sub(cap.saturating_sub(1));
    let label_width = fields
        .iter()
        .map(|f| f.label.width())
        .max()
        .unwrap_or(0)
        .min((area.width as usize / 2).saturating_sub(3));
    for (i, field) in fields.iter().enumerate().skip(start).take(cap) {
        let y = area.y + (i - start) as u16;
        let active = i == selected;
        let color = if field.readonly {
            palette.muted
        } else if active {
            palette.accent
        } else {
            palette.fg
        };
        let label = crate::text::clipped(&field.label, label_width);
        let padding = label_width.saturating_sub(label.width());
        let suffix = if field.readonly {
            "  read-only"
        } else if field.is_choice() {
            "  ▾"
        } else {
            ""
        };
        let mut display = field.display_value();
        let mut cursor_column = if field.private {
            field.value[..field.cursor.min(field.value.len())]
                .graphemes(true)
                .count()
        } else {
            field.value[..field.cursor.min(field.value.len())].width()
        };
        let available = (area.width as usize)
            .saturating_sub(label_width + 4 + suffix.width())
            .max(1);
        if active && field.is_text() && cursor_column >= available && available > 2 {
            let wanted = cursor_column.saturating_sub(available - 2);
            let byte = crate::text::cell_byte(&display, wanted);
            let skipped = display[..byte].width();
            display = format!("‹{}", &display[byte..]);
            cursor_column = cursor_column.saturating_sub(skipped) + 1;
        }
        line(
            buf,
            y,
            vec![
                plain(
                    format!(
                        "{} {}{}  ",
                        if active { "›" } else { " " },
                        label,
                        " ".repeat(padding)
                    ),
                    color,
                ),
                plain(
                    display,
                    if field.kind == "boolean" && field.value == "true" {
                        palette.success
                    } else {
                        color
                    },
                ),
                plain(suffix.into(), palette.muted),
            ],
        );
        geometry
            .fields
            .push((i, Rect::new(area.x, y, area.width, 1)));
        if active && field.is_text() {
            geometry.cursor = Some((
                area.x
                    + (label_width + 4 + cursor_column).min(area.width.saturating_sub(1) as usize)
                        as u16,
                y,
            ));
        }
    }
    if area.height >= 4 {
        let y = area.bottom() - 3;
        line(
            buf,
            y,
            vec![
                plain(
                    format!(
                        "{} Apply changes",
                        if selected == fields.len() { "›" } else { " " }
                    ),
                    palette.accent,
                ),
                plain("   Ctrl+P Preview · Ctrl+F Refresh".into(), palette.muted),
            ],
        );
        geometry.apply = Some(Rect::new(area.x, y, 17.min(area.width), 1));
        line(buf, y + 1, vec![plain(status.into(), palette.muted)]);
        line(
            buf,
            y + 2,
            vec![plain(
                "↑↓ Field · Enter/Space Change · Ctrl+S Apply · Esc Back".into(),
                palette.muted,
            )],
        );
    }
    geometry
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn long_unicode_field_keeps_cursor_and_tail_visible() {
        let field = Field::text("Name", &format!("{}TAIL", "中文".repeat(20)));
        let area = Rect::new(0, 0, 24, 6);
        let mut buffer = Buffer::empty(area);
        let geometry = render(
            &mut buffer,
            area,
            &[field],
            0,
            "",
            crate::view::Palette::new(false, false),
        );
        let row = (0..24).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
        assert!(row.contains("TAIL"));
        assert!(geometry.cursor.is_some_and(|p| area.contains(p.into())));
    }
    #[test]
    fn boolean_rejects_typing_and_paste_without_corrupting_true() {
        let mut field = Field::boolean("Enabled", true);
        assert!(field.handle_key(key(KeyCode::Char('x'))).is_err());
        assert!(field.paste("false").is_err());
        assert_eq!(field.parsed_value().unwrap(), json!(true));
        field.handle_key(key(KeyCode::Char(' '))).unwrap();
        assert_eq!(field.parsed_value().unwrap(), json!(false));
    }
    #[test]
    fn enum_search_does_not_commit_free_text() {
        let mut field = Field::choice("Model", "a", &["a", "qwen", "b"]);
        field.paste("qw").unwrap();
        assert_eq!(field.value, "a");
        field.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(field.parsed_value().unwrap(), json!("qwen"));
        field.value = "unknown".into();
        assert!(field.parsed_value().is_err());
    }
    #[test]
    fn multichoice_preserves_unselected_values_and_toggles_individually() {
        let mut field = Field::choice("Checks", r#"["tests"]"#, &["tests", "cleanup"]);
        field.kind = "multi_choice".into();
        field.choose(1);
        assert_eq!(field.parsed_value().unwrap(), json!(["tests", "cleanup"]));
        field.choose(0);
        assert_eq!(field.parsed_value().unwrap(), json!(["cleanup"]));
    }
    #[test]
    fn integer_rejects_fraction_object_overflow_and_out_of_range() {
        let mut field = Field::integer("Budget", 1, 1, 100);
        for invalid in ["1.5", "{}", "9223372036854775808", "0", "101"] {
            field.value = invalid.into();
            assert!(field.parsed_value().is_err(), "{invalid}");
        }
    }
    #[test]
    fn readonly_ignores_keyboard_and_paste() {
        let mut field = Field::readonly("Version", "v1");
        field.handle_key(key(KeyCode::Backspace)).unwrap();
        assert!(field.paste("bad").is_err());
        assert_eq!(field.value, "v1");
    }
    #[test]
    fn unicode_text_deletes_and_inserts_whole_graphemes() {
        let mut field = Field::text("Name", "中文e\u{301}👩‍💻");
        field.handle_key(key(KeyCode::Backspace)).unwrap();
        field.handle_key(key(KeyCode::Left)).unwrap();
        field.paste("好").unwrap();
        assert_eq!(field.value, "中文好e\u{301}");
    }
    #[test]
    fn validation_fails_atomically_without_mutating_other_fields() {
        let fields = vec![
            Field::boolean("Enabled", true),
            Field::integer("Budget", 0, 1, 100),
        ];
        assert_eq!(validate(&fields).unwrap_err().0, 1);
        assert_eq!(fields[0].value, "true");
    }
    #[test]
    fn secret_clear_is_distinct_from_empty_unchanged_input() {
        let mut field = Field::text("Token", "");
        field.private = true;
        field.allow_clear = true;
        assert!(validate(&[field.clone()]).is_ok());
        field
            .handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(field.clear);
        field.paste("secret").unwrap();
        assert!(!field.clear);
        assert!(!field.display_value().contains("secret"));
    }
    #[test]
    fn host_enum_retains_numeric_canonical_value() {
        let mut field = Field::choice("Retries", "2", &["1", "2"]);
        field.option_values = vec![json!(1), json!(2)];
        assert_eq!(field.parsed_value().unwrap(), json!(2));
    }
    #[test]
    fn untouched_absent_configuration_does_not_block_other_edits() {
        let mut field = Field::choice("Optional", "", &["a", "b"]);
        field.allow_clear = true;
        assert!(validate(&[field]).is_ok());
    }
    #[test]
    fn cursor_after_private_submission_is_normalized() {
        let mut field = Field::text("Token", "secret");
        field.value.clear();
        field.handle_key(key(KeyCode::Backspace)).unwrap();
        assert_eq!(field.cursor, 0);
    }
    #[test]
    fn optional_unset_choice_serializes_as_null_for_host() {
        let field = Field::choice("Optional", "", &["a", "b"]);
        assert_eq!(field.parsed_value().unwrap(), Value::Null);
    }
    #[test]
    fn required_whitespace_is_rejected_before_host_submission() {
        let mut field = Field::text("Reason", "  ");
        field.required = true;
        assert!(field.parsed_value().is_err());
    }
    #[test]
    fn absent_boolean_is_explicit_and_required_choice_must_be_set() {
        let mut field = Field::boolean("Enabled", false);
        field.value.clear();
        assert_eq!(field.display_value(), "○ Unset");
        assert_eq!(field.parsed_value().unwrap(), Value::Null);
        field.required = true;
        assert!(field.parsed_value().is_err());
        field.handle_key(key(KeyCode::Right)).unwrap();
        field.handle_key(key(KeyCode::Right)).unwrap();
        assert_eq!(field.parsed_value().unwrap(), json!(true));
        field.handle_key(key(KeyCode::Left)).unwrap();
        field.handle_key(key(KeyCode::Left)).unwrap();
        assert_eq!(field.parsed_value().unwrap(), json!(false));
        field.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(field.parsed_value().unwrap(), json!(true));
    }
    #[test]
    fn multi_choice_preserves_commas_and_significant_whitespace() {
        let mut field = Field::choice("Checks", "[]", &["a,b", " padded ", "中文,检查"]);
        field.kind = "multi_choice".into();
        field.choose(0);
        field.choose(1);
        field.choose(2);
        assert_eq!(
            field.parsed_value().unwrap(),
            json!(["a,b", " padded ", "中文,检查"])
        );
        field.choose(0);
        assert_eq!(
            field.parsed_value().unwrap(),
            json!([" padded ", "中文,检查"])
        );
        assert!(field.display_value().contains(" padded "));
    }
    #[test]
    fn required_multi_choice_rejects_empty_json_array() {
        let mut field = Field::choice("Checks", "[]", &["tests"]);
        field.kind = "multi_choice".into();
        field.required = true;
        assert!(field.parsed_value().is_err());
    }
}
