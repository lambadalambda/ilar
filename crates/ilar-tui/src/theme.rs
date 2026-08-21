//! Colour as a post-render remap.
//!
//! Widgets never see a theme. They style cells with the *role* constants
//! below — named ANSI colours for foregrounds, indexed sentinels for the
//! roles the ANSI namespace ran out of room for — and [`apply`] rewrites
//! every cell once the frame is drawn. Adding a per-theme branch to a
//! widget would defeat the whole arrangement.

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
        definition(self).id
    }

    pub const fn label(self) -> &'static str {
        definition(self).label
    }

    pub const fn description(self) -> &'static str {
        definition(self).description
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.id() == id)
    }
}

struct Definition {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    palette: Palette,
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
    /// `None` on the adaptive theme, which cannot know the canvas it is
    /// painting on and so must not paint one.
    surfaces: Option<Surfaces>,
    syntax: Syntax,
}

/// Backgrounds. Every one is a *tint* — a few percent off the canvas —
/// except the selection, which is meant to be found.
#[derive(Clone, Copy)]
struct Surfaces {
    surface: Color,
    surface_alt: Color,
    code_bg: Color,
    diff_add_bg: Color,
    diff_del_bg: Color,
    selection_bg: Color,
}

/// Code fences speak their own language. Sharing slots with the status
/// colours meant a string in a code block and a passing tool call were
/// the same green.
#[derive(Clone, Copy)]
struct Syntax {
    keyword: Color,
    string: Color,
    number: Color,
    comment: Color,
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

/// Roles with no ANSI name left to borrow. Indexed colours are a
/// namespace nothing else in the TUI emits, so `apply` can recognise one
/// by value; the numbers themselves are never displayed, since every
/// theme — including the adaptive one — resolves them.
pub const SURFACE: Color = Color::Indexed(235);
pub const SURFACE_ALT: Color = Color::Indexed(237);
pub const CODE_BG: Color = Color::Indexed(236);
pub const DIFF_ADD_BG: Color = Color::Indexed(22);
pub const DIFF_DEL_BG: Color = Color::Indexed(52);
pub const SELECTION_BG: Color = Color::Indexed(24);
pub const SYN_KEYWORD: Color = Color::Indexed(141);
pub const SYN_STRING: Color = Color::Indexed(114);
pub const SYN_NUMBER: Color = Color::Indexed(179);
pub const SYN_COMMENT: Color = Color::Indexed(245);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Surface,
    SurfaceAlt,
    CodeBg,
    DiffAddBg,
    DiffDelBg,
    SelectionBg,
    Keyword,
    StringLiteral,
    Number,
    Comment,
}

impl Role {
    fn of(color: Color) -> Option<Self> {
        Some(match color {
            SURFACE => Self::Surface,
            SURFACE_ALT => Self::SurfaceAlt,
            CODE_BG => Self::CodeBg,
            DIFF_ADD_BG => Self::DiffAddBg,
            DIFF_DEL_BG => Self::DiffDelBg,
            SELECTION_BG => Self::SelectionBg,
            SYN_KEYWORD => Self::Keyword,
            SYN_STRING => Self::StringLiteral,
            SYN_NUMBER => Self::Number,
            SYN_COMMENT => Self::Comment,
            _ => return None,
        })
    }
}

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

static TERMINAL: Definition = Definition {
    id: "terminal",
    label: "Terminal Adaptive",
    description: "Uses your terminal's native foreground and background",
    palette: Palette {
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
        surfaces: None,
        syntax: Syntax {
            keyword: MARKUP,
            string: SUCCESS,
            number: WAITING,
            comment: MUTED,
        },
    },
};

static CARBON: Definition = Definition {
    id: "carbon",
    label: "Carbon",
    description: "Deep graphite with bright cyan and green accents",
    palette: Palette {
        canvas: rgb(16, 19, 24),
        primary: rgb(240, 243, 246),
        secondary: rgb(187, 195, 204),
        muted: rgb(140, 150, 162),
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
        surfaces: Some(Surfaces {
            surface: rgb(24, 28, 35),
            surface_alt: rgb(32, 37, 45),
            code_bg: rgb(28, 33, 41),
            diff_add_bg: rgb(20, 44, 32),
            diff_del_bg: rgb(56, 25, 30),
            selection_bg: rgb(35, 62, 82),
        }),
        syntax: Syntax {
            keyword: rgb(198, 160, 246),
            string: rgb(166, 218, 149),
            number: rgb(245, 169, 127),
            comment: rgb(115, 125, 138),
        },
    },
};

static PARCHMENT: Definition = Definition {
    id: "parchment",
    label: "Parchment",
    description: "Warm light canvas with ink-like contrast",
    palette: Palette {
        canvas: rgb(250, 247, 240),
        primary: rgb(23, 26, 31),
        secondary: rgb(56, 65, 74),
        muted: rgb(105, 114, 124),
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
        surfaces: Some(Surfaces {
            surface: rgb(243, 239, 229),
            surface_alt: rgb(236, 231, 218),
            code_bg: rgb(240, 236, 225),
            diff_add_bg: rgb(223, 238, 219),
            diff_del_bg: rgb(247, 223, 223),
            selection_bg: rgb(214, 227, 243),
        }),
        syntax: Syntax {
            keyword: rgb(137, 44, 137),
            string: rgb(24, 92, 60),
            number: rgb(150, 68, 12),
            comment: rgb(122, 130, 138),
        },
    },
};

static FROST: Definition = Definition {
    id: "frost",
    label: "Frost",
    description: "Cool blue-gray with soft arctic accents",
    palette: Palette {
        canvas: rgb(30, 36, 48),
        primary: rgb(236, 239, 244),
        secondary: rgb(199, 206, 219),
        muted: rgb(150, 160, 178),
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
        surfaces: Some(Surfaces {
            surface: rgb(38, 45, 59),
            surface_alt: rgb(46, 54, 70),
            code_bg: rgb(42, 50, 65),
            diff_add_bg: rgb(38, 56, 46),
            diff_del_bg: rgb(64, 42, 48),
            selection_bg: rgb(52, 74, 99),
        }),
        syntax: Syntax {
            keyword: rgb(180, 142, 173),
            string: rgb(163, 190, 140),
            number: rgb(208, 135, 112),
            comment: rgb(126, 137, 156),
        },
    },
};

static HIGH_CONTRAST: Definition = Definition {
    id: "high-contrast",
    label: "High Contrast",
    description: "Maximum separation on a black canvas",
    palette: Palette {
        canvas: rgb(0, 0, 0),
        primary: rgb(255, 255, 255),
        secondary: rgb(208, 208, 208),
        muted: rgb(168, 168, 168),
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
        surfaces: Some(Surfaces {
            surface: rgb(18, 18, 18),
            surface_alt: rgb(30, 30, 30),
            code_bg: rgb(24, 24, 24),
            diff_add_bg: rgb(0, 48, 24),
            diff_del_bg: rgb(56, 0, 12),
            selection_bg: rgb(0, 60, 90),
        }),
        syntax: Syntax {
            keyword: rgb(224, 160, 255),
            string: rgb(128, 255, 159),
            number: rgb(255, 190, 128),
            comment: rgb(160, 160, 160),
        },
    },
};

const fn definition(theme: ThemeId) -> &'static Definition {
    match theme {
        ThemeId::Terminal => &TERMINAL,
        ThemeId::Carbon => &CARBON,
        ThemeId::Parchment => &PARCHMENT,
        ThemeId::Frost => &FROST,
        ThemeId::HighContrast => &HIGH_CONTRAST,
    }
}

const fn palette(theme: ThemeId) -> Palette {
    definition(theme).palette
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
        color => match Role::of(color) {
            Some(Role::Keyword) => palette.syntax.keyword,
            Some(Role::StringLiteral) => palette.syntax.string,
            Some(Role::Number) => palette.syntax.number,
            Some(Role::Comment) => palette.syntax.comment,
            // A surface sentinel used as a foreground is a bug in the
            // caller; leaving it visible beats painting invisible text.
            _ => color,
        },
    }
}

/// Backgrounds resolve to a tint, or — on the adaptive theme, which has
/// none — to nothing. The selection is the exception: it has to be
/// findable, so without a tint it falls back to reverse video, which is
/// what `less` and `vim` do on an unknown canvas.
fn map_background(color: Color, palette: Palette) -> (Color, bool) {
    let Some(role) = Role::of(color) else {
        return (
            if color == Color::Reset {
                palette.canvas
            } else {
                map_color(color, palette)
            },
            false,
        );
    };
    let Some(surfaces) = palette.surfaces else {
        return (palette.canvas, role == Role::SelectionBg);
    };
    let color = match role {
        Role::Surface => surfaces.surface,
        Role::SurfaceAlt => surfaces.surface_alt,
        Role::CodeBg => surfaces.code_bg,
        Role::DiffAddBg => surfaces.diff_add_bg,
        Role::DiffDelBg => surfaces.diff_del_bg,
        Role::SelectionBg => surfaces.selection_bg,
        // Syntax sentinels are foregrounds; as a background they would be
        // a caller bug, so fall back to the canvas rather than guess.
        _ => palette.canvas,
    };
    (color, false)
}

pub fn apply(buffer: &mut ratatui::buffer::Buffer, theme: ThemeId) {
    let palette = palette(theme);
    for cell in &mut buffer.content {
        cell.fg = map_color(cell.fg, palette);
        let (background, reverse) = map_background(cell.bg, palette);
        cell.bg = background;
        if reverse {
            cell.modifier |= Modifier::REVERSED;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn channels(color: Color) -> Option<(f64, f64, f64)> {
        match color {
            Color::Rgb(red, green, blue) => Some((
                f64::from(red) / 255.0,
                f64::from(green) / 255.0,
                f64::from(blue) / 255.0,
            )),
            _ => None,
        }
    }

    /// WCAG relative luminance.
    fn luminance(color: Color) -> Option<f64> {
        let (red, green, blue) = channels(color)?;
        let linear = |channel: f64| {
            if channel <= 0.03928 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        Some(0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue))
    }

    fn contrast(one: Color, other: Color) -> f64 {
        let (Some(one), Some(other)) = (luminance(one), luminance(other)) else {
            return f64::INFINITY;
        };
        let (lighter, darker) = if one > other {
            (one, other)
        } else {
            (other, one)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    fn surfaces(palette: Palette) -> Vec<(&'static str, Color)> {
        let Some(surfaces) = palette.surfaces else {
            return Vec::new();
        };
        vec![
            ("surface", surfaces.surface),
            ("surface_alt", surfaces.surface_alt),
            ("code_bg", surfaces.code_bg),
            ("diff_add_bg", surfaces.diff_add_bg),
            ("diff_del_bg", surfaces.diff_del_bg),
            ("selection_bg", surfaces.selection_bg),
        ]
    }

    /// Legibility is the one property a palette cannot be allowed to get
    /// wrong by eye, and hand-written RGB tables are exactly where it
    /// goes wrong.
    #[test]
    fn every_theme_keeps_text_legible_on_every_surface() {
        for theme in ThemeId::ALL {
            let palette = palette(theme);
            let ratio = contrast(palette.primary, palette.canvas);
            assert!(ratio >= 7.0, "{}: body text {ratio:.1}:1", theme.id());
            let ratio = contrast(palette.muted, palette.canvas);
            assert!(ratio >= 3.0, "{}: muted text {ratio:.1}:1", theme.id());
            for (name, surface) in surfaces(palette) {
                let ratio = contrast(palette.primary, surface);
                assert!(ratio >= 4.5, "{}: text on {name} {ratio:.1}:1", theme.id());
            }
        }
    }

    /// A surface groups; it does not announce. Anything much beyond a few
    /// percent off the canvas reads as a slab — which is how the reversed
    /// inline-code span looked, at 21:1.
    #[test]
    fn surfaces_are_tints_not_slabs() {
        for theme in ThemeId::ALL {
            let palette = palette(theme);
            for (name, surface) in surfaces(palette) {
                assert_ne!(surface, palette.canvas, "{}: {name} is bare", theme.id());
                let ratio = contrast(surface, palette.canvas);
                assert!(
                    ratio <= 2.0,
                    "{}: {name} shouts at {ratio:.1}:1",
                    theme.id()
                );
            }
        }
    }

    /// The adaptive theme cannot know the canvas, so it must paint no
    /// surfaces at all — except the selection, which reverses instead so
    /// a search hit is still findable.
    #[test]
    fn the_adaptive_theme_reverses_rather_than_painting_a_canvas_it_cannot_see() {
        let palette = palette(ThemeId::Terminal);
        for sentinel in [SURFACE, SURFACE_ALT, CODE_BG, DIFF_ADD_BG, DIFF_DEL_BG] {
            assert_eq!(map_background(sentinel, palette), (Color::Reset, false));
        }
        assert_eq!(map_background(SELECTION_BG, palette), (Color::Reset, true));
        // And it leaves every named colour exactly as the terminal set it.
        for named in [PRIMARY, SECONDARY, MUTED, USER, RUNNING, ERROR] {
            assert_eq!(map_color(named, palette), named);
        }
    }

    /// Syntax has its own slots — a faithful palette may still land on
    /// the same green for strings as for success, but the four classes
    /// have to differ from each other and stay readable on the code
    /// surface, or a fence is just noise.
    #[test]
    fn syntax_classes_differ_and_read_on_the_code_surface() {
        for theme in ThemeId::ALL {
            let palette = palette(theme);
            let classes = [
                ("keyword", SYN_KEYWORD),
                ("string", SYN_STRING),
                ("number", SYN_NUMBER),
                ("comment", SYN_COMMENT),
            ]
            .map(|(name, sentinel)| (name, map_color(sentinel, palette)));
            for (index, (name, color)) in classes.iter().enumerate() {
                for (other_name, other) in &classes[index + 1..] {
                    assert_ne!(color, other, "{}: {name} == {other_name}", theme.id());
                }
                let Some(surfaces) = palette.surfaces else {
                    continue;
                };
                // Comments are meant to recede, so they get the lower bar.
                let floor = if *name == "comment" { 3.0 } else { 4.0 };
                let ratio = contrast(*color, surfaces.code_bg);
                assert!(
                    ratio >= floor,
                    "{}: {name} on code {ratio:.1}:1",
                    theme.id()
                );
            }
        }
    }
}
