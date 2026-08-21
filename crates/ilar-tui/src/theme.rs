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
    Monokai,
    Dracula,
    GruvboxDark,
    GruvboxLight,
    SolarizedDark,
    SolarizedLight,
    TokyoNight,
    CatppuccinMocha,
    OneDark,
    RosePine,
}

impl ThemeId {
    pub const ALL: [Self; 15] = [
        Self::Terminal,
        Self::Carbon,
        Self::Parchment,
        Self::Frost,
        Self::HighContrast,
        Self::Monokai,
        Self::Dracula,
        Self::GruvboxDark,
        Self::GruvboxLight,
        Self::SolarizedDark,
        Self::SolarizedLight,
        Self::TokyoNight,
        Self::CatppuccinMocha,
        Self::OneDark,
        Self::RosePine,
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

impl Default for ThemeId {
    /// A tuned dark theme rather than the adaptive one: the hierarchy
    /// this palette encodes — surfaces, damped chrome, a syntax set — is
    /// what a first run should show, and `terminal` can offer none of it.
    fn default() -> Self {
        Self::Carbon
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

static MONOKAI: Definition = Definition {
    id: "monokai",
    label: "Monokai",
    description: "The classic high-chroma dark palette",
    palette: Palette {
        canvas: rgb(39, 40, 34),
        primary: rgb(248, 248, 242),
        secondary: rgb(202, 202, 196),
        muted: rgb(122, 118, 100),
        running: rgb(102, 217, 239),
        user: rgb(102, 217, 239),
        assistant: rgb(166, 226, 46),
        success: rgb(166, 226, 46),
        waiting: rgb(230, 219, 116),
        error: rgb(249, 38, 114),
        reasoning: rgb(174, 129, 255),
        markup: rgb(102, 217, 239),
        code: rgb(102, 217, 239),
        selected_fg: rgb(39, 40, 34),
        surfaces: Some(Surfaces {
            surface: rgb(54, 55, 49),
            surface_alt: rgb(68, 69, 63),
            code_bg: rgb(60, 61, 55),
            diff_add_bg: rgb(67, 81, 37),
            diff_del_bg: rgb(85, 40, 52),
            selection_bg: rgb(56, 87, 88),
        }),
        syntax: Syntax {
            keyword: rgb(249, 118, 163),
            string: rgb(230, 219, 116),
            number: rgb(180, 139, 254),
            comment: rgb(143, 140, 124),
        },
    },
};

static DRACULA: Definition = Definition {
    id: "dracula",
    label: "Dracula",
    description: "Violet-forward dark with pastel accents",
    palette: Palette {
        canvas: rgb(40, 42, 54),
        primary: rgb(248, 248, 242),
        secondary: rgb(202, 203, 201),
        muted: rgb(104, 119, 167),
        running: rgb(139, 233, 253),
        user: rgb(189, 147, 249),
        assistant: rgb(80, 250, 123),
        success: rgb(80, 250, 123),
        waiting: rgb(241, 250, 140),
        error: rgb(255, 85, 85),
        reasoning: rgb(255, 121, 198),
        markup: rgb(189, 147, 249),
        code: rgb(139, 233, 253),
        selected_fg: rgb(40, 42, 54),
        surfaces: Some(Surfaces {
            surface: rgb(55, 56, 67),
            surface_alt: rgb(69, 71, 80),
            code_bg: rgb(61, 63, 73),
            diff_add_bg: rgb(49, 88, 69),
            diff_del_bg: rgb(87, 51, 61),
            selection_bg: rgb(88, 76, 116),
        }),
        syntax: Syntax {
            keyword: rgb(255, 121, 198),
            string: rgb(241, 250, 140),
            number: rgb(189, 147, 249),
            comment: rgb(131, 143, 181),
        },
    },
};

static GRUVBOX_DARK: Definition = Definition {
    id: "gruvbox-dark",
    label: "Gruvbox Dark",
    description: "Retro warm earth tones on brown-black",
    palette: Palette {
        canvas: rgb(40, 40, 40),
        primary: rgb(235, 219, 178),
        secondary: rgb(192, 180, 148),
        muted: rgb(146, 131, 116),
        running: rgb(142, 192, 124),
        user: rgb(131, 165, 152),
        assistant: rgb(184, 187, 38),
        success: rgb(184, 187, 38),
        waiting: rgb(250, 189, 47),
        error: rgb(251, 73, 52),
        reasoning: rgb(211, 134, 155),
        markup: rgb(131, 165, 152),
        code: rgb(142, 192, 124),
        selected_fg: rgb(40, 40, 40),
        surfaces: Some(Surfaces {
            surface: rgb(54, 53, 50),
            surface_alt: rgb(67, 65, 59),
            code_bg: rgb(60, 58, 54),
            diff_add_bg: rgb(72, 72, 40),
            diff_del_bg: rgb(86, 47, 43),
            selection_bg: rgb(69, 80, 76),
        }),
        syntax: Syntax {
            keyword: rgb(246, 123, 95),
            string: rgb(184, 187, 38),
            number: rgb(212, 137, 156),
            comment: rgb(150, 135, 118),
        },
    },
};

static GRUVBOX_LIGHT: Definition = Definition {
    id: "gruvbox-light",
    label: "Gruvbox Light",
    description: "The same earth tones on warm paper",
    palette: Palette {
        canvas: rgb(251, 241, 199),
        primary: rgb(60, 56, 54),
        secondary: rgb(102, 97, 86),
        muted: rgb(124, 111, 100),
        running: rgb(66, 123, 88),
        user: rgb(7, 102, 120),
        assistant: rgb(121, 116, 14),
        success: rgb(121, 116, 14),
        waiting: rgb(181, 118, 20),
        error: rgb(157, 0, 6),
        reasoning: rgb(143, 63, 113),
        markup: rgb(7, 102, 120),
        code: rgb(66, 123, 88),
        selected_fg: rgb(251, 241, 199),
        surfaces: Some(Surfaces {
            surface: rgb(238, 228, 189),
            surface_alt: rgb(224, 215, 179),
            code_bg: rgb(232, 222, 184),
            diff_add_bg: rgb(222, 214, 158),
            diff_del_bg: rgb(230, 188, 157),
            selection_bg: rgb(173, 197, 174),
        }),
        syntax: Syntax {
            keyword: rgb(157, 0, 6),
            string: rgb(110, 105, 21),
            number: rgb(143, 63, 113),
            comment: rgb(124, 111, 100),
        },
    },
};

static SOLARIZED_DARK: Definition = Definition {
    id: "solarized-dark",
    label: "Solarized Dark",
    description: "Precision-balanced cyan-tinted dark",
    palette: Palette {
        canvas: rgb(0, 43, 54),
        primary: rgb(147, 161, 161),
        secondary: rgb(115, 135, 137),
        muted: rgb(100, 120, 126),
        running: rgb(42, 161, 152),
        user: rgb(38, 139, 210),
        assistant: rgb(133, 153, 0),
        success: rgb(133, 153, 0),
        waiting: rgb(181, 137, 0),
        error: rgb(220, 50, 47),
        reasoning: rgb(211, 54, 130),
        markup: rgb(38, 139, 210),
        code: rgb(42, 161, 152),
        selected_fg: rgb(0, 43, 54),
        surfaces: Some(Surfaces {
            surface: rgb(10, 51, 61),
            surface_alt: rgb(16, 56, 66),
            code_bg: rgb(15, 55, 65),
            diff_add_bg: rgb(17, 57, 47),
            diff_del_bg: rgb(48, 45, 52),
            selection_bg: rgb(5, 55, 74),
        }),
        syntax: Syntax {
            keyword: rgb(138, 156, 61),
            string: rgb(80, 161, 155),
            number: rgb(157, 144, 156),
            comment: rgb(113, 131, 135),
        },
    },
};

static SOLARIZED_LIGHT: Definition = Definition {
    id: "solarized-light",
    label: "Solarized Light",
    description: "The same balance on antique paper",
    palette: Palette {
        canvas: rgb(253, 246, 227),
        primary: rgb(88, 110, 117),
        secondary: rgb(124, 140, 141),
        muted: rgb(123, 141, 143),
        running: rgb(42, 161, 152),
        user: rgb(38, 139, 210),
        assistant: rgb(133, 153, 0),
        success: rgb(133, 153, 0),
        waiting: rgb(181, 137, 0),
        error: rgb(220, 50, 47),
        reasoning: rgb(211, 54, 130),
        markup: rgb(38, 139, 210),
        code: rgb(42, 161, 152),
        selected_fg: rgb(253, 246, 227),
        surfaces: Some(Surfaces {
            surface: rgb(246, 240, 222),
            surface_alt: rgb(246, 240, 222),
            code_bg: rgb(246, 240, 222),
            diff_add_bg: rgb(246, 240, 213),
            diff_del_bg: rgb(252, 238, 220),
            selection_bg: rgb(242, 241, 226),
        }),
        syntax: Syntax {
            keyword: rgb(99, 120, 89),
            string: rgb(78, 121, 125),
            number: rgb(196, 61, 128),
            comment: rgb(119, 137, 140),
        },
    },
};

static TOKYO_NIGHT: Definition = Definition {
    id: "tokyo-night",
    label: "Tokyo Night",
    description: "Deep blue night with neon accents",
    palette: Palette {
        canvas: rgb(26, 27, 38),
        primary: rgb(192, 202, 245),
        secondary: rgb(155, 164, 199),
        muted: rgb(97, 106, 148),
        running: rgb(125, 207, 255),
        user: rgb(122, 162, 247),
        assistant: rgb(158, 206, 106),
        success: rgb(158, 206, 106),
        waiting: rgb(224, 175, 104),
        error: rgb(247, 118, 142),
        reasoning: rgb(187, 154, 247),
        markup: rgb(122, 162, 247),
        code: rgb(125, 207, 255),
        selected_fg: rgb(26, 27, 38),
        surfaces: Some(Surfaces {
            surface: rgb(38, 39, 52),
            surface_alt: rgb(49, 52, 67),
            code_bg: rgb(43, 44, 59),
            diff_add_bg: rgb(55, 66, 53),
            diff_del_bg: rgb(75, 47, 61),
            selection_bg: rgb(57, 70, 105),
        }),
        syntax: Syntax {
            keyword: rgb(187, 154, 247),
            string: rgb(158, 206, 106),
            number: rgb(255, 158, 100),
            comment: rgb(111, 121, 163),
        },
    },
};

static CATPPUCCIN_MOCHA: Definition = Definition {
    id: "catppuccin-mocha",
    label: "Catppuccin Mocha",
    description: "Soft pastels on a muted lavender-black",
    palette: Palette {
        canvas: rgb(30, 30, 46),
        primary: rgb(205, 214, 244),
        secondary: rgb(166, 174, 200),
        muted: rgb(108, 112, 134),
        running: rgb(148, 226, 213),
        user: rgb(137, 180, 250),
        assistant: rgb(166, 227, 161),
        success: rgb(166, 227, 161),
        waiting: rgb(249, 226, 175),
        error: rgb(243, 139, 168),
        reasoning: rgb(203, 166, 247),
        markup: rgb(137, 180, 250),
        code: rgb(148, 226, 213),
        selected_fg: rgb(30, 30, 46),
        surfaces: Some(Surfaces {
            surface: rgb(42, 43, 60),
            surface_alt: rgb(54, 56, 74),
            code_bg: rgb(48, 48, 66),
            diff_add_bg: rgb(60, 73, 71),
            diff_del_bg: rgb(77, 54, 73),
            selection_bg: rgb(62, 75, 107),
        }),
        syntax: Syntax {
            keyword: rgb(203, 166, 247),
            string: rgb(166, 227, 161),
            number: rgb(250, 179, 135),
            comment: rgb(122, 126, 149),
        },
    },
};

static ONE_DARK: Definition = Definition {
    id: "one-dark",
    label: "One Dark",
    description: "Atom's balanced slate-blue dark",
    palette: Palette {
        canvas: rgb(40, 44, 52),
        primary: rgb(171, 178, 191),
        secondary: rgb(142, 149, 160),
        muted: rgb(116, 123, 136),
        running: rgb(86, 182, 194),
        user: rgb(97, 175, 239),
        assistant: rgb(152, 195, 121),
        success: rgb(152, 195, 121),
        waiting: rgb(229, 192, 123),
        error: rgb(224, 108, 117),
        reasoning: rgb(198, 120, 221),
        markup: rgb(97, 175, 239),
        code: rgb(86, 182, 194),
        selected_fg: rgb(40, 44, 52),
        surfaces: Some(Surfaces {
            surface: rgb(49, 53, 62),
            surface_alt: rgb(58, 63, 71),
            code_bg: rgb(53, 57, 66),
            diff_add_bg: rgb(58, 68, 63),
            diff_del_bg: rgb(80, 58, 66),
            selection_bg: rgb(50, 67, 85),
        }),
        syntax: Syntax {
            keyword: rgb(191, 135, 213),
            string: rgb(152, 195, 121),
            number: rgb(209, 154, 102),
            comment: rgb(128, 135, 148),
        },
    },
};

static ROSE_PINE: Definition = Definition {
    id: "rose-pine",
    label: "Rosé Pine",
    description: "Muted rose and pine on charcoal ink",
    palette: Palette {
        canvas: rgb(25, 23, 36),
        primary: rgb(224, 222, 244),
        secondary: rgb(180, 178, 198),
        muted: rgb(110, 106, 134),
        running: rgb(156, 207, 216),
        user: rgb(49, 116, 143),
        assistant: rgb(156, 207, 216),
        success: rgb(156, 207, 216),
        waiting: rgb(246, 193, 119),
        error: rgb(235, 111, 146),
        reasoning: rgb(196, 167, 231),
        markup: rgb(49, 116, 143),
        code: rgb(156, 207, 216),
        selected_fg: rgb(25, 23, 36),
        surfaces: Some(Surfaces {
            surface: rgb(39, 37, 51),
            surface_alt: rgb(53, 51, 65),
            code_bg: rgb(45, 43, 57),
            diff_add_bg: rgb(54, 63, 76),
            diff_del_bg: rgb(71, 42, 60),
            selection_bg: rgb(33, 53, 70),
        }),
        syntax: Syntax {
            keyword: rgb(196, 167, 231),
            string: rgb(246, 193, 119),
            number: rgb(235, 188, 186),
            comment: rgb(124, 120, 147),
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
        ThemeId::Monokai => &MONOKAI,
        ThemeId::Dracula => &DRACULA,
        ThemeId::GruvboxDark => &GRUVBOX_DARK,
        ThemeId::GruvboxLight => &GRUVBOX_LIGHT,
        ThemeId::SolarizedDark => &SOLARIZED_DARK,
        ThemeId::SolarizedLight => &SOLARIZED_LIGHT,
        ThemeId::TokyoNight => &TOKYO_NIGHT,
        ThemeId::CatppuccinMocha => &CATPPUCCIN_MOCHA,
        ThemeId::OneDark => &ONE_DARK,
        ThemeId::RosePine => &ROSE_PINE,
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
    ///
    /// The floor is WCAG AA for body text rather than AAA, because the
    /// ported palettes ship their published values: Solarized Dark is
    /// 5.6:1 by design and brightening it would make it a different
    /// theme. What ilar authors itself is held higher — see
    /// [`the_default_theme_is_held_to_a_higher_bar`].
    #[test]
    fn every_theme_keeps_text_legible_on_every_surface() {
        for theme in ThemeId::ALL {
            let palette = palette(theme);
            let ratio = contrast(palette.primary, palette.canvas);
            assert!(ratio >= 4.5, "{}: body text {ratio:.1}:1", theme.id());
            let ratio = contrast(palette.muted, palette.canvas);
            assert!(ratio >= 3.0, "{}: muted text {ratio:.1}:1", theme.id());
            for (name, surface) in surfaces(palette) {
                let ratio = contrast(palette.primary, surface);
                assert!(ratio >= 4.5, "{}: text on {name} {ratio:.1}:1", theme.id());
            }
        }
    }

    /// The theme most people will never change is the one that has to be
    /// right, so it clears AAA on the canvas and on every surface.
    #[test]
    fn the_default_theme_is_held_to_a_higher_bar() {
        let palette = palette(ThemeId::default());
        let ratio = contrast(palette.primary, palette.canvas);
        assert!(ratio >= 7.0, "default body text {ratio:.1}:1");
        for (name, surface) in surfaces(palette) {
            let ratio = contrast(palette.primary, surface);
            assert!(ratio >= 7.0, "default text on {name} {ratio:.1}:1");
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
