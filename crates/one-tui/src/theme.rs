//! OpenCode default dark palette (`opencode` theme).
//!
//! Source: packages/tui/src/theme/assets/opencode.json

use ratatui::style::{Color, Modifier, Style};

/// Colors mirror OpenCode dark steps / accents.
pub struct Theme;

impl Theme {
    // Steps
    pub const BG: Color = Color::Rgb(0x0a, 0x0a, 0x0a); // darkStep1
    pub const PANEL: Color = Color::Rgb(0x14, 0x14, 0x14); // darkStep2
    pub const ELEMENT: Color = Color::Rgb(0x1e, 0x1e, 0x1e); // darkStep3
    pub const BORDER: Color = Color::Rgb(0x48, 0x48, 0x48); // darkStep7
    pub const BORDER_ACTIVE: Color = Color::Rgb(0x60, 0x60, 0x60); // darkStep8
    pub const MUTED: Color = Color::Rgb(0x80, 0x80, 0x80); // darkStep11
    pub const FG: Color = Color::Rgb(0xee, 0xee, 0xee); // darkStep12

    // Accents (OpenCode defaults)
    pub const PRIMARY: Color = Color::Rgb(0xfa, 0xb2, 0x83); // peach — agent / user bar
    pub const SECONDARY: Color = Color::Rgb(0x5c, 0x9c, 0xf5); // blue
    pub const ACCENT: Color = Color::Rgb(0x9d, 0x7c, 0xd8); // purple
    pub const SUCCESS: Color = Color::Rgb(0x7f, 0xd8, 0x8f);
    pub const ERROR: Color = Color::Rgb(0xe0, 0x6c, 0x75);
    pub const WARNING: Color = Color::Rgb(0xf5, 0xa7, 0x42);
    pub const INFO: Color = Color::Rgb(0x56, 0xb6, 0xc2);
    pub const CODE: Color = Color::Rgb(0x7f, 0xd8, 0x8f);

    pub fn bg() -> Style {
        Style::default().bg(Self::BG).fg(Self::FG)
    }

    pub fn border() -> Style {
        // Always paint panel bg on border cells — prevents chat bleed-through.
        Style::default().fg(Self::BORDER).bg(Self::PANEL)
    }

    pub fn title() -> Style {
        Style::default().fg(Self::MUTED).bg(Self::PANEL)
    }

    /// Float footer keybinding strip (on border bottom title).
    pub fn float_footer() -> Style {
        Style::default()
            .fg(Self::MUTED)
            .bg(Self::PANEL)
            .add_modifier(Modifier::ITALIC)
    }

    /// Action chip in float rows — `[edit]` / `[select]` looks clickable.
    pub fn float_action() -> Style {
        Style::default().bg(Self::PANEL).fg(Self::BORDER_ACTIVE)
    }

    pub fn float_action_selected() -> Style {
        Style::default()
            .bg(Self::ELEMENT)
            .fg(Self::MUTED)
            .add_modifier(Modifier::DIM)
    }

    /// Primary CTA row (Import / Fetch models) — blue label, stands out from data.
    pub fn float_cta() -> Style {
        Style::default()
            .bg(Self::PANEL)
            .fg(Self::SECONDARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn float_cta_desc() -> Style {
        Style::default().bg(Self::PANEL).fg(Self::INFO)
    }

    pub fn float_cta_chip() -> Style {
        Style::default()
            .bg(Self::ELEMENT)
            .fg(Self::SECONDARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn float_cta_selected() -> Style {
        Style::default()
            .bg(Self::ELEMENT)
            .fg(Self::SECONDARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn float_cta_selected_desc() -> Style {
        Style::default().bg(Self::ELEMENT).fg(Self::INFO)
    }

    pub fn float_cta_chip_selected() -> Style {
        Style::default()
            .bg(Self::SECONDARY)
            .fg(Self::BG)
            .add_modifier(Modifier::BOLD)
    }

    /// Active filter field (user is typing a query).
    pub fn float_filter_active() -> Style {
        Style::default()
            .bg(Self::ELEMENT)
            .fg(Self::FG)
            .add_modifier(Modifier::BOLD)
    }

    pub fn float_filter_label() -> Style {
        Style::default().bg(Self::ELEMENT).fg(Self::MUTED)
    }

    pub fn status() -> Style {
        Style::default().fg(Self::MUTED)
    }

    /// Keybinding tokens in the status strip (slightly brighter than labels).
    pub fn status_key() -> Style {
        Style::default().fg(Self::FG)
    }

    pub fn status_faint() -> Style {
        Style::default().fg(Self::BORDER_ACTIVE)
    }

    /// Base style for the prompt panel (applied via Paragraph::style).
    pub fn input() -> Style {
        Style::default().fg(Self::FG).bg(Self::ELEMENT)
    }

    /// Explicit text style for typed characters (never muted).
    pub fn input_text() -> Style {
        Style::default()
            .fg(Self::FG)
            .bg(Self::ELEMENT)
            .add_modifier(Modifier::BOLD)
    }

    pub fn input_placeholder() -> Style {
        Style::default().fg(Self::MUTED).bg(Self::ELEMENT)
    }

    /// Slash popup panel background.
    pub fn slash_panel() -> Style {
        Style::default().bg(Self::PANEL).fg(Self::FG)
    }

    pub fn slash_selected() -> Style {
        Style::default()
            .bg(Self::ELEMENT)
            .fg(Self::PRIMARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn slash_item() -> Style {
        Style::default().bg(Self::PANEL).fg(Self::FG)
    }

    pub fn slash_desc() -> Style {
        Style::default().bg(Self::PANEL).fg(Self::MUTED)
    }

    pub fn slash_title() -> Style {
        Style::default()
            .bg(Self::PANEL)
            .fg(Self::MUTED)
            .add_modifier(Modifier::ITALIC)
    }

    /// Allow chaining modifiers on theme styles.
    pub fn with_modifier(style: Style, m: Modifier) -> Style {
        style.add_modifier(m)
    }

    /// Prompt left rail — agent color (primary peach). Interaction focus only.
    pub fn prompt_bar() -> Style {
        Style::default().fg(Self::PRIMARY)
    }

    pub fn prompt_bar_busy() -> Style {
        Style::default().fg(Self::WARNING)
    }

    /// Mode / agent name on the meta strip (Build · Plan).
    /// Copper — identity tag, deliberately *not* PRIMARY so selection stays unique.
    pub fn mode_label() -> Style {
        Style::default().fg(Color::Rgb(0xa8, 0x86, 0x68))
    }

    /// Float list / section text while in-float value edit owns focus.
    pub fn float_dim() -> Style {
        Style::default()
            .bg(Self::PANEL)
            .fg(Self::MUTED)
            .add_modifier(Modifier::DIM)
    }

    pub fn float_dim_desc() -> Style {
        Style::default()
            .bg(Self::PANEL)
            .fg(Self::BORDER_ACTIVE)
            .add_modifier(Modifier::DIM)
    }

    /// In-float editor label (`api_key:`) — claims the sole focus color.
    pub fn float_edit_label() -> Style {
        Style::default()
            .bg(Self::ELEMENT)
            .fg(Self::PRIMARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn float_edit_text() -> Style {
        Style::default()
            .bg(Self::ELEMENT)
            .fg(Self::FG)
            .add_modifier(Modifier::BOLD)
    }

    /// Internal hairlines (search/list split, section rules) — below outer border.
    pub fn hairline() -> Style {
        Style::default().bg(Self::PANEL).fg(Self::BORDER)
    }

    /// Streaming caret in assistant text.
    pub fn cursor() -> Style {
        Style::default().fg(Self::PRIMARY)
    }

    /// Input caret on — software typewriter bar (▌).
    pub fn input_cursor_on() -> Style {
        Style::default().fg(Self::PRIMARY).bg(Self::ELEMENT)
    }

    /// Input caret off — same cell width, blends into panel.
    pub fn input_cursor_off() -> Style {
        Style::default().fg(Self::ELEMENT).bg(Self::ELEMENT)
    }

    pub fn user_bar() -> Style {
        Style::default().fg(Self::PRIMARY)
    }

    pub fn user_body() -> Style {
        // Slightly elevated panel so the bubble lifts off pure BG.
        Style::default().fg(Self::FG).bg(Self::ELEMENT)
    }

    pub fn user_pad() -> Style {
        Style::default().bg(Self::ELEMENT)
    }

    pub fn assistant_body() -> Style {
        Style::default().fg(Self::FG)
    }

    /// Thinking block chevron / accent.
    pub fn thinking_chevron() -> Style {
        Style::default().fg(Self::ACCENT)
    }

    pub fn thinking_title() -> Style {
        Style::default()
            .fg(Self::ACCENT)
            .add_modifier(Modifier::DIM)
    }

    pub fn thinking_meta() -> Style {
        Style::default().fg(Self::MUTED)
    }

    pub fn thinking_body() -> Style {
        Style::default()
            .fg(Self::MUTED)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn heading() -> Style {
        Style::default()
            .fg(Self::ACCENT)
            .add_modifier(Modifier::BOLD)
    }

    pub fn heading_sub() -> Style {
        Style::default()
            .fg(Self::SECONDARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn strong() -> Style {
        Style::default()
            .fg(Self::WARNING)
            .add_modifier(Modifier::BOLD)
    }

    pub fn code() -> Style {
        Style::default().fg(Self::CODE)
    }

    /// Fenced code block body (panel fill + green text).
    pub fn code_block() -> Style {
        Style::default().fg(Self::CODE).bg(Self::PANEL)
    }

    pub fn code_lang() -> Style {
        Style::default()
            .fg(Self::MUTED)
            .bg(Self::ELEMENT)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn link() -> Style {
        Style::default()
            .fg(Self::SECONDARY)
            .add_modifier(Modifier::UNDERLINED)
    }

    pub fn blockquote() -> Style {
        Style::default().fg(Self::BORDER_ACTIVE)
    }

    pub fn table_border() -> Style {
        Style::default().fg(Self::BORDER)
    }

    pub fn table_header() -> Style {
        Style::default().fg(Self::FG).add_modifier(Modifier::BOLD)
    }

    pub fn table_cell() -> Style {
        Style::default().fg(Self::FG)
    }

    pub fn system_body() -> Style {
        Style::default()
            .fg(Self::MUTED)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn meta() -> Style {
        Style::default().fg(Self::MUTED)
    }

    pub fn tool_icon() -> Style {
        Style::default().fg(Self::BORDER_ACTIVE)
    }

    pub fn tool_icon_running() -> Style {
        Style::default().fg(Self::PRIMARY)
    }

    pub fn tool_icon_done() -> Style {
        Style::default().fg(Self::SUCCESS)
    }

    pub fn tool_icon_error() -> Style {
        Style::default().fg(Self::ERROR)
    }

    pub fn tool_text() -> Style {
        Style::default().fg(Self::MUTED)
    }

    pub fn tool_text_running() -> Style {
        Style::default().fg(Self::FG)
    }

    pub fn tool_text_error() -> Style {
        Style::default().fg(Self::ERROR)
    }

    pub fn tool_name_running() -> Style {
        Style::default()
            .fg(Self::PRIMARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn tool_name_done() -> Style {
        Style::default().fg(Self::FG).add_modifier(Modifier::BOLD)
    }

    pub fn tool_name_error() -> Style {
        Style::default()
            .fg(Self::ERROR)
            .add_modifier(Modifier::BOLD)
    }

    pub fn tool_detail_running() -> Style {
        Style::default().fg(Self::FG)
    }

    pub fn tool_detail_done() -> Style {
        Style::default().fg(Self::MUTED)
    }

    /// Soft secondary line under a tool (exit code / summary).
    /// In-app text selection highlight (mouse drag → OSC 52 copy).
    pub fn selection() -> Style {
        Style::default()
            .bg(Color::Rgb(60, 80, 120))
            .fg(Color::Rgb(230, 235, 245))
    }

    pub fn tool_summary_ok() -> Style {
        Style::default().fg(Self::SUCCESS)
    }

    pub fn tool_summary_err() -> Style {
        Style::default().fg(Self::ERROR)
    }

    pub fn tool_tree() -> Style {
        Style::default().fg(Self::BORDER)
    }

    /// Per-tool accent for the name chip (subtle variety without rainbow soup).
    pub fn tool_kind(name: &str) -> Style {
        let fg = match name {
            "bash" | "shell" => Self::WARNING,
            "read" | "ls" | "find" | "grep" => Self::SECONDARY,
            "edit" => Self::ACCENT,
            "write" => Self::SUCCESS,
            "web_search" | "web_fetch" => Self::INFO,
            _ => Self::FG,
        };
        Style::default().fg(fg).add_modifier(Modifier::BOLD)
    }

    pub fn turn_glyph() -> Style {
        Style::default().fg(Self::PRIMARY)
    }

    pub fn turn_mode() -> Style {
        Style::default().fg(Self::FG)
    }

    pub fn busy() -> Style {
        Style::default().fg(Self::MUTED)
    }

    pub fn error_bar() -> Style {
        Style::default().fg(Self::ERROR)
    }

    pub fn error_body() -> Style {
        Style::default().fg(Self::MUTED).bg(Self::PANEL)
    }

    // Diff palette — soft VS Code / Cursor dark (not neon full-row blocks).
    const DIFF_ADD_BG: Color = Color::Rgb(0x14, 0x2a, 0x1c);
    const DIFF_ADD_BG_HI: Color = Color::Rgb(0x1f, 0x4a, 0x2c);
    const DIFF_ADD_FG: Color = Color::Rgb(0xc8, 0xea, 0xd0);
    const DIFF_ADD_MARK: Color = Color::Rgb(0x4e, 0xc9, 0x7a);
    const DIFF_DEL_BG: Color = Color::Rgb(0x2a, 0x14, 0x16);
    const DIFF_DEL_BG_HI: Color = Color::Rgb(0x4a, 0x1f, 0x24);
    const DIFF_DEL_FG: Color = Color::Rgb(0xf0, 0xc8, 0xcc);
    const DIFF_DEL_MARK: Color = Color::Rgb(0xe0, 0x6c, 0x75);
    const DIFF_GUTTER_BG: Color = Color::Rgb(0x0e, 0x0e, 0x0e);

    /// IDE-style insert line body.
    pub fn diff_add() -> Style {
        Style::default().fg(Self::DIFF_ADD_FG).bg(Self::DIFF_ADD_BG)
    }

    /// Word-level highlight on an insert line (brighter green wash).
    pub fn diff_add_word() -> Style {
        Style::default()
            .fg(Color::Rgb(0xe8, 0xff, 0xee))
            .bg(Self::DIFF_ADD_BG_HI)
            .add_modifier(Modifier::BOLD)
    }

    /// IDE-style delete line body.
    pub fn diff_del() -> Style {
        Style::default().fg(Self::DIFF_DEL_FG).bg(Self::DIFF_DEL_BG)
    }

    /// Word-level highlight on a delete line.
    pub fn diff_del_word() -> Style {
        Style::default()
            .fg(Color::Rgb(0xff, 0xe0, 0xe4))
            .bg(Self::DIFF_DEL_BG_HI)
            .add_modifier(Modifier::BOLD)
    }

    pub fn diff_meta() -> Style {
        Style::default().fg(Self::MUTED)
    }

    /// 1-col left accent rail (add).
    pub fn diff_mark_add() -> Style {
        Style::default().fg(Self::DIFF_ADD_MARK).bg(Self::DIFF_ADD_BG)
    }

    /// 1-col left accent rail (delete).
    pub fn diff_mark_del() -> Style {
        Style::default().fg(Self::DIFF_DEL_MARK).bg(Self::DIFF_DEL_BG)
    }

    pub fn diff_mark_ctx() -> Style {
        Style::default().fg(Self::BORDER).bg(Self::DIFF_GUTTER_BG)
    }

    /// Gutter line number on context rows.
    pub fn diff_ln() -> Style {
        Style::default()
            .fg(Color::Rgb(0x5a, 0x5a, 0x5a))
            .bg(Self::DIFF_GUTTER_BG)
    }

    pub fn diff_ln_add() -> Style {
        Style::default()
            .fg(Self::DIFF_ADD_MARK)
            .bg(Self::DIFF_ADD_BG)
    }

    pub fn diff_ln_del() -> Style {
        Style::default()
            .fg(Self::DIFF_DEL_MARK)
            .bg(Self::DIFF_DEL_BG)
    }

    /// Thin gutter / body separator.
    pub fn diff_gutter_sep() -> Style {
        Style::default()
            .fg(Color::Rgb(0x2a, 0x2a, 0x2a))
            .bg(Self::DIFF_GUTTER_BG)
    }

    pub fn diff_gutter_sep_add() -> Style {
        Style::default()
            .fg(Color::Rgb(0x1f, 0x3a, 0x28))
            .bg(Self::DIFF_ADD_BG)
    }

    pub fn diff_gutter_sep_del() -> Style {
        Style::default()
            .fg(Color::Rgb(0x3a, 0x1f, 0x22))
            .bg(Self::DIFF_DEL_BG)
    }

    /// Unchanged context code in an expanded edit diff.
    pub fn diff_context() -> Style {
        Style::default()
            .fg(Color::Rgb(0xb0, 0xb0, 0xb0))
            .bg(Self::BG)
    }

    /// Ellipsis / skipped-lines meta inside a diff block.
    pub fn diff_skip() -> Style {
        Style::default()
            .fg(Self::MUTED)
            .bg(Self::DIFF_GUTTER_BG)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn tool_group() -> Style {
        Style::default().fg(Self::MUTED)
    }

    pub fn tool_group_title() -> Style {
        Style::default().fg(Self::FG).add_modifier(Modifier::BOLD)
    }

    /// Number / check badge on a solid chip (terminal-safe; avoids ☑/① tofu).
    pub fn badge_primary() -> Style {
        Style::default()
            .fg(Self::BG)
            .bg(Self::PRIMARY)
            .add_modifier(Modifier::BOLD)
    }

    pub fn badge_success() -> Style {
        Style::default()
            .fg(Self::BG)
            .bg(Self::SUCCESS)
            .add_modifier(Modifier::BOLD)
    }

    pub fn badge_muted() -> Style {
        Style::default().fg(Self::MUTED).bg(Self::ELEMENT)
    }
}
