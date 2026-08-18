use ratatui::style::{Color, Modifier, Style};

// Named terminal colors adapt to both light and dark user themes. In particular,
// body text uses the terminal foreground instead of assuming a dark canvas.
pub const PRIMARY: Color = Color::Reset;
pub const SECONDARY: Color = Color::Gray;
pub const MUTED: Color = Color::DarkGray;
pub const BORDER: Color = Color::DarkGray;
pub const FOCUS_BORDER: Color = Color::Cyan;
pub const USER: Color = Color::LightCyan;
pub const RUNNING: Color = Color::Cyan;
pub const ASSISTANT: Color = Color::LightGreen;
pub const SUCCESS: Color = Color::Green;
pub const WAITING: Color = Color::Yellow;
pub const ERROR: Color = Color::LightRed;
pub const REASONING: Color = Color::LightMagenta;
pub const MARKUP: Color = Color::LightBlue;
pub const CODE: Color = Color::Blue;
pub const SELECTED_FG: Color = Color::Black;

pub fn selected() -> Style {
    Style::default().fg(SELECTED_FG).bg(RUNNING)
}

pub fn panel_border() -> Style {
    Style::default().fg(BORDER)
}

pub fn focus_border() -> Style {
    Style::default()
        .fg(FOCUS_BORDER)
        .add_modifier(Modifier::BOLD)
}

pub fn title(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}
