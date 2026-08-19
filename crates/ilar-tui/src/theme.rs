use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeId {
    Terminal,
    Carbon,
    Parchment,
    Frost,
    HighContrast,
}

impl ThemeId {
    pub const ALL: [Self; 5] = [
        Self::Terminal,
        Self::Carbon,
        Self::Parchment,
        Self::Frost,
        Self::HighContrast,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Carbon => "carbon",
            Self::Parchment => "parchment",
            Self::Frost => "frost",
            Self::HighContrast => "high-contrast",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal Adaptive",
            Self::Carbon => "Carbon",
            Self::Parchment => "Parchment",
            Self::Frost => "Frost",
            Self::HighContrast => "High Contrast",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Terminal => "Uses your terminal's native foreground and background",
            Self::Carbon => "Deep graphite with bright cyan and green accents",
            Self::Parchment => "Warm light canvas with ink-like contrast",
            Self::Frost => "Cool blue-gray with soft arctic accents",
            Self::HighContrast => "Maximum separation on a black canvas",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.id() == id)
    }
}

#[derive(Clone, Copy)]
struct Palette {
    canvas: Color,
    primary: Color,
    secondary: Color,
    muted: Color,
    running: Color,
    user: Color,
    assistant: Color,
    success: Color,
    waiting: Color,
    error: Color,
    reasoning: Color,
    markup: Color,
    code: Color,
    selected_fg: Color,
}

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

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

const fn palette(theme: ThemeId) -> Palette {
    match theme {
        ThemeId::Terminal => Palette {
            canvas: Color::Reset,
            primary: PRIMARY,
            secondary: SECONDARY,
            muted: MUTED,
            running: RUNNING,
            user: USER,
            assistant: ASSISTANT,
            success: SUCCESS,
            waiting: WAITING,
            error: ERROR,
            reasoning: REASONING,
            markup: MARKUP,
            code: CODE,
            selected_fg: SELECTED_FG,
        },
        ThemeId::Carbon => Palette {
            canvas: rgb(16, 19, 24),
            primary: rgb(240, 243, 246),
            secondary: rgb(187, 195, 204),
            muted: rgb(152, 162, 173),
            running: rgb(84, 214, 232),
            user: rgb(121, 215, 255),
            assistant: rgb(143, 227, 136),
            success: rgb(111, 219, 154),
            waiting: rgb(255, 209, 102),
            error: rgb(255, 123, 134),
            reasoning: rgb(213, 166, 255),
            markup: rgb(130, 183, 255),
            code: rgb(96, 165, 250),
            selected_fg: rgb(7, 16, 20),
        },
        ThemeId::Parchment => Palette {
            canvas: rgb(250, 247, 240),
            primary: rgb(23, 26, 31),
            secondary: rgb(56, 65, 74),
            muted: rgb(89, 99, 110),
            running: rgb(0, 107, 115),
            user: rgb(0, 90, 130),
            assistant: rgb(39, 101, 0),
            success: rgb(22, 101, 52),
            waiting: rgb(122, 77, 0),
            error: rgb(180, 35, 45),
            reasoning: rgb(111, 63, 160),
            markup: rgb(25, 94, 184),
            code: rgb(51, 78, 154),
            selected_fg: rgb(255, 255, 255),
        },
        ThemeId::Frost => Palette {
            canvas: rgb(30, 36, 48),
            primary: rgb(236, 239, 244),
            secondary: rgb(199, 206, 219),
            muted: rgb(167, 176, 192),
            running: rgb(136, 192, 208),
            user: rgb(129, 161, 193),
            assistant: rgb(163, 217, 165),
            success: rgb(163, 190, 140),
            waiting: rgb(235, 203, 139),
            error: rgb(239, 143, 147),
            reasoning: rgb(199, 160, 216),
            markup: rgb(143, 184, 232),
            code: rgb(121, 184, 255),
            selected_fg: rgb(21, 32, 42),
        },
        ThemeId::HighContrast => Palette {
            canvas: rgb(0, 0, 0),
            primary: rgb(255, 255, 255),
            secondary: rgb(208, 208, 208),
            muted: rgb(179, 179, 179),
            running: rgb(0, 229, 255),
            user: rgb(101, 217, 255),
            assistant: rgb(128, 255, 159),
            success: rgb(84, 229, 139),
            waiting: rgb(255, 215, 95),
            error: rgb(255, 123, 123),
            reasoning: rgb(224, 160, 255),
            markup: rgb(140, 180, 255),
            code: rgb(102, 179, 255),
            selected_fg: rgb(0, 0, 0),
        },
    }
}

#[cfg(test)]
pub const fn canvas(theme: ThemeId) -> Color {
    palette(theme).canvas
}

fn map_color(color: Color, palette: Palette) -> Color {
    match color {
        Color::Reset => palette.primary,
        Color::Gray => palette.secondary,
        Color::DarkGray => palette.muted,
        Color::Cyan => palette.running,
        Color::LightCyan => palette.user,
        Color::LightGreen => palette.assistant,
        Color::Green => palette.success,
        Color::Yellow => palette.waiting,
        Color::LightRed => palette.error,
        Color::LightMagenta => palette.reasoning,
        Color::LightBlue => palette.markup,
        Color::Blue => palette.code,
        Color::Black => palette.selected_fg,
        color => color,
    }
}

pub fn apply(buffer: &mut ratatui::buffer::Buffer, theme: ThemeId) {
    if theme == ThemeId::Terminal {
        return;
    }
    let palette = palette(theme);
    for cell in &mut buffer.content {
        cell.fg = map_color(cell.fg, palette);
        cell.bg = if cell.bg == Color::Reset {
            palette.canvas
        } else {
            map_color(cell.bg, palette)
        };
    }
}

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
