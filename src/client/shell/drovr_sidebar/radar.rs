//! drovr fork: vendor marks and state colours for the structured sidebar view.
//!
//! Values come from the herdr-radar plugin (MIT, Copyright (c) 2025 qintmb,
//! Copyright (c) 2026 herdr-kit contributors): icon font codepoints from
//! `lib/logos.js`, brand and state colours from `lib/palette.js`, state marks,
//! spinner frames and idle thresholds from `lib/config.js`, spinner cadence
//! from `lib/frame.js`. The icon font itself is not bundled: the default
//! `[ui.sidebar] agent_icons = "radar"` needs it installed, `"letter"` draws
//! the vendor's initial instead.

use ratatui::style::Color;

use super::projects::Presence;
use crate::app::state::Palette;
use crate::config::AgentIconsConfig;
use crate::terminal_theme::HostAppearance;

/// An idle agent reads as recently active for this long (radar's
/// `activity_fresh_minutes` default).
const FRESH_SECS: u64 = 15 * 60;
/// After this long idle it reads as stale (radar's `activity_stale_minutes`).
const STALE_SECS: u64 = 120 * 60;

/// What an agent row's title colour says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Working,
    Done,
    Blocked,
    /// Marked unread by hand (drovr only; radar has no such state).
    Unread,
    IdleFresh,
    Idle,
    IdleStale,
    Unknown,
}

/// Radar's state scale from drovr's presence. `idle_secs` is the time since
/// the agent's last state change. Unknown age is plain idle, as in radar's
/// `freshness()`: an agent never seen working is a restart, not neglect.
pub(super) fn tone(presence: Presence, unknown: bool, idle_secs: Option<u64>) -> Tone {
    match presence {
        Presence::Working => Tone::Working,
        Presence::Done => Tone::Done,
        Presence::Blocked => Tone::Blocked,
        Presence::Unread => Tone::Unread,
        Presence::Idle if unknown => Tone::Unknown,
        Presence::Idle => match idle_secs {
            None => Tone::Idle,
            Some(secs) if secs <= FRESH_SECS => Tone::IdleFresh,
            Some(secs) if secs < STALE_SECS => Tone::Idle,
            Some(_) => Tone::IdleStale,
        },
    }
}

/// Radar's spinner frames (`lib/config.js` `FRAMES`).
const FRAMES: [&str; 8] = ["⣷", "⣯", "⣟", "⡿", "⢿", "⣻", "⣽", "⣾"];
/// How long each spinner frame shows (radar's `SPIN_MS`, `lib/frame.js`).
const SPIN_MS: u128 = 150;

/// The spinner frame for wall-clock time `now_ms` (ms since the Unix epoch),
/// so every redraw within the same 150 ms step draws the same frame.
pub(super) fn spin_frame(now_ms: u128) -> &'static str {
    FRAMES[spin_index(now_ms) as usize % FRAMES.len()]
}

/// The spinner step at `now_ms`; it changes once per frame.
pub(super) fn spin_index(now_ms: u128) -> u64 {
    (now_ms / SPIN_MS) as u64
}

/// The mark in front of the title. Idle tiers have none: their colour says it.
/// Working spins; `now_ms` picks the frame (see `spin_frame`).
pub(super) fn lead(tone: Tone, now_ms: u128) -> Option<&'static str> {
    match tone {
        Tone::Working => Some(spin_frame(now_ms)),
        Tone::Done => Some("✓"),
        Tone::Blocked => Some("?"),
        Tone::Unknown => Some("◌"),
        Tone::Unread => Some("●"),
        Tone::IdleFresh | Tone::Idle | Tone::IdleStale => None,
    }
}

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Which ink set the view draws neutral text in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ground {
    /// Radar's dark-background values.
    Dark,
    /// Radar's light-background values.
    Light,
    /// Background unknown (16-colour theme, no host report): palette colours.
    Palette,
}

/// The background the view draws on: judged from an RGB palette's text
/// colour, else from the host terminal's reported appearance.
pub(super) fn ground(palette: &Palette, host: Option<HostAppearance>) -> Ground {
    match (palette.text, host) {
        (Color::Rgb(r, g, b), _) => {
            if u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114 < 128_000 {
                Ground::Light
            } else {
                Ground::Dark
            }
        }
        (_, Some(HostAppearance::Light)) => Ground::Light,
        (_, Some(HostAppearance::Dark)) => Ground::Dark,
        (_, None) => Ground::Palette,
    }
}

/// `dark` or `light` for a known background, else the palette's `fallback`.
fn pick(ground: Ground, dark: u32, light: u32, fallback: Color) -> Color {
    match ground {
        Ground::Dark => hex(dark),
        Ground::Light => hex(light),
        Ground::Palette => fallback,
    }
}

/// A vendor's own hue, if it publishes one (radar `palette.brand`).
fn brand(vendor: &str) -> Option<Color> {
    Some(hex(match vendor {
        "claude" => 0xd97757,
        "gemini" => 0x4285f4,
        "kimi" => 0x1783ff,
        "deepseek" => 0x4d6bfe,
        "qwen" => 0x615ced,
        "kiro" => 0x9046ff,
        "cline" => 0x586876,
        "kilo" => 0x9a9808,
        _ => return None,
    }))
}

/// Working title colour for vendors without a hue (radar `brand.other`).
const BRAND_OTHER: Color = hex(0xc78a1f);

/// The ink a hueless brand signs in.
fn ink(ground: Ground, palette: &Palette) -> Color {
    pick(ground, 0xe9e9f0, 0x16161c, palette.text)
}

/// Workspace header colour (radar `subtle`).
pub(super) fn subtle(ground: Ground, palette: &Palette) -> Color {
    pick(ground, 0xa8abbd, 0x7c7f93, palette.subtext0)
}

/// Title colour and boldness for a state; working wears the vendor's colour.
pub(super) fn title_style(
    tone: Tone,
    vendor: Option<&str>,
    ground: Ground,
    palette: &Palette,
) -> (Color, bool) {
    match tone {
        Tone::Working => (vendor.and_then(brand).unwrap_or(BRAND_OTHER), true),
        Tone::Done => (hex(0x4c9a5a), false),
        Tone::Blocked => (hex(0xc04a4a), false),
        Tone::Unknown => (hex(0x907aa9), false),
        Tone::Unread => (Color::Yellow, false),
        Tone::IdleFresh => (pick(ground, 0x95bba2, 0x416c4f, palette.green), false),
        Tone::Idle => (pick(ground, 0xa99e92, 0x6b6259, palette.subtext0), false),
        Tone::IdleStale => (pick(ground, 0x8b8e9c, 0x69696d, palette.overlay0), false),
    }
}

/// The vendor's product name (radar `DISPLAY`, `lib/logos.js`): what an agent
/// that sets no topic puts in the terminal title.
pub(super) fn display_name(vendor: &str) -> Option<&'static str> {
    Some(match vendor {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "omp" => "Oh My Pi",
        "cline" => "Cline",
        "mastracode" => "Mastra",
        "kimi" => "Kimi",
        "kilo" => "Kilo",
        "maki" => "Maki",
        "pi" => "Pi",
        "hermes" => "Hermes",
        "cursor" => "Cursor",
        "copilot" => "Copilot",
        "deepseek" => "DeepSeek",
        "gemini" => "Gemini",
        "gpt" => "GPT",
        "qwen" => "Qwen",
        "grok" => "grok",
        "agy" => "Antigravity",
        "kiro" => "Kiro",
        "amp" => "Amp",
        "devin" => "Devin",
        "qodercli" => "Qoder",
        "glm" => "GLM",
        _ => return None,
    })
}

/// Radar icon font codepoint for a vendor (radar `PUA`).
fn font_glyph(vendor: &str) -> Option<char> {
    Some(match vendor {
        "claude" => '\u{e1a0}',
        "codex" => '\u{e1a1}',
        "opencode" => '\u{e1a2}',
        "omp" => '\u{e1a3}',
        "cline" => '\u{e1a4}',
        "mastracode" => '\u{e1a5}',
        "kimi" => '\u{e1a6}',
        "kilo" => '\u{e1a7}',
        "maki" => '\u{e1a8}',
        "pi" => '\u{e1a9}',
        "hermes" => '\u{e1aa}',
        "cursor" => '\u{e1ab}',
        "copilot" => '\u{e1ac}',
        "deepseek" => '\u{e1ad}',
        "gemini" => '\u{e1ae}',
        "gpt" => '\u{e1af}',
        "qwen" => '\u{e1b0}',
        "grok" => '\u{e1b1}',
        "agy" => '\u{e1b2}',
        "kiro" => '\u{e1b3}',
        "amp" => '\u{e1b4}',
        "devin" => '\u{e1b5}',
        "qodercli" => '\u{e1b6}',
        "glm" => '\u{e1b7}',
        _ => return None,
    })
}

/// The vendor mark and its colour; `None` when marks are off. Vendors radar
/// does not know get a neutral dot in `palette.overlay0`, in both mark modes.
pub(super) fn logo(
    vendor: Option<&str>,
    icons: AgentIconsConfig,
    ground: Ground,
    palette: &Palette,
) -> Option<(String, Color)> {
    let known = vendor.filter(|vendor| font_glyph(vendor).is_some());
    let mark = match (icons, known) {
        (AgentIconsConfig::None, _) => return None,
        (AgentIconsConfig::Radar, Some(vendor)) => font_glyph(vendor).map(String::from),
        (AgentIconsConfig::Letter, Some(vendor)) => vendor
            .chars()
            .next()
            .map(|letter| letter.to_ascii_uppercase().to_string()),
        (_, None) => None,
    };
    Some(match mark {
        Some(mark) => (
            mark,
            known
                .and_then(brand)
                .unwrap_or_else(|| ink(ground, palette)),
        ),
        None => ("•".to_owned(), palette.overlay0),
    })
}

/// `text` cut to `width` display columns, ending in `…` when cut.
pub(super) fn fit(text: &str, width: u16) -> String {
    crate::ui::text::truncate_end(text, usize::from(width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_tiers_follow_radar_thresholds() {
        let idle = |secs| tone(Presence::Idle, false, secs);
        assert_eq!(idle(Some(60)), Tone::IdleFresh);
        assert_eq!(idle(Some(FRESH_SECS)), Tone::IdleFresh);
        assert_eq!(idle(Some(FRESH_SECS + 1)), Tone::Idle);
        assert_eq!(idle(Some(STALE_SECS - 1)), Tone::Idle);
        assert_eq!(idle(Some(STALE_SECS)), Tone::IdleStale);
        assert_eq!(idle(None), Tone::Idle);
        assert_eq!(tone(Presence::Idle, true, Some(1)), Tone::Unknown);
        assert_eq!(tone(Presence::Working, true, None), Tone::Working);
        assert_eq!(tone(Presence::Blocked, false, None), Tone::Blocked);
    }

    #[test]
    fn state_colours_match_radar() {
        let palette = Palette::catppuccin();
        assert_eq!(
            title_style(Tone::Working, Some("claude"), Ground::Dark, &palette),
            (Color::Rgb(0xd9, 0x77, 0x57), true)
        );
        assert_eq!(
            title_style(Tone::Working, Some("codex"), Ground::Dark, &palette),
            (BRAND_OTHER, true)
        );
        assert_eq!(
            title_style(Tone::Done, None, Ground::Light, &palette),
            (Color::Rgb(0x4c, 0x9a, 0x5a), false)
        );
        assert_eq!(
            title_style(Tone::Blocked, None, Ground::Dark, &palette).0,
            Color::Rgb(0xc0, 0x4a, 0x4a)
        );
        assert_eq!(
            title_style(Tone::Unknown, None, Ground::Dark, &palette).0,
            Color::Rgb(0x90, 0x7a, 0xa9)
        );
        assert_eq!(
            title_style(Tone::IdleFresh, None, Ground::Dark, &palette).0,
            Color::Rgb(0x95, 0xbb, 0xa2)
        );
        assert_eq!(
            title_style(Tone::IdleFresh, None, Ground::Light, &palette).0,
            Color::Rgb(0x41, 0x6c, 0x4f)
        );
        assert_eq!(
            title_style(Tone::IdleStale, None, Ground::Dark, &palette).0,
            Color::Rgb(0x8b, 0x8e, 0x9c)
        );
        let terminal = Palette::terminal();
        assert_eq!(
            title_style(Tone::Idle, None, Ground::Palette, &terminal).0,
            terminal.subtext0
        );
        assert_eq!(subtle(Ground::Palette, &terminal), terminal.subtext0);
        assert_eq!(lead(Tone::Idle, 0), None);
        assert_eq!(lead(Tone::Done, 0), Some("✓"));
    }

    #[test]
    fn spinner_frame_follows_wall_clock_at_radar_cadence() {
        assert_eq!(spin_frame(0), "⣷");
        assert_eq!(spin_frame(149), "⣷");
        assert_eq!(spin_frame(150), "⣯");
        assert_eq!(spin_frame(7 * 150), "⣾");
        assert_eq!(spin_frame(8 * 150), "⣷");
        assert_eq!(lead(Tone::Working, 300), Some("⣟"));
    }

    #[test]
    fn logos_by_icon_setting() {
        let palette = Palette::catppuccin();
        let neutral = ("•".to_owned(), palette.overlay0);
        let logo = |vendor, icons| logo(vendor, icons, Ground::Dark, &palette);
        assert_eq!(
            logo(Some("claude"), AgentIconsConfig::Radar),
            Some(("\u{e1a0}".to_owned(), Color::Rgb(0xd9, 0x77, 0x57)))
        );
        assert_eq!(
            logo(Some("codex"), AgentIconsConfig::Letter),
            Some(("C".to_owned(), hex(0xe9e9f0)))
        );
        assert_eq!(
            logo(Some("droid"), AgentIconsConfig::Radar),
            Some(neutral.clone())
        );
        assert_eq!(
            logo(Some("droid"), AgentIconsConfig::Letter),
            Some(neutral.clone())
        );
        assert_eq!(logo(None, AgentIconsConfig::Letter), Some(neutral));
        assert_eq!(logo(Some("claude"), AgentIconsConfig::None), None);
        // Unknown background: hueless brands sign in the palette's text colour.
        let terminal = Palette::terminal();
        assert_eq!(
            super::logo(
                Some("codex"),
                AgentIconsConfig::Letter,
                Ground::Palette,
                &terminal
            ),
            Some(("C".to_owned(), terminal.text))
        );
    }

    #[test]
    fn fit_truncates_by_display_width() {
        assert_eq!(fit("Fix auth", 20), "Fix auth");
        let cut = fit("修复认证流程 gateway", 9);
        assert_eq!(cut, "修复认证…");
        assert!(crate::ui::text::display_width(&cut) <= 9);
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn ground_from_palette_then_host_appearance() {
        let mut palette = Palette::catppuccin();
        assert_eq!(ground(&palette, Some(HostAppearance::Light)), Ground::Dark);
        palette.text = Color::Rgb(0x34, 0x3b, 0x58);
        assert_eq!(ground(&palette, None), Ground::Light);
        let terminal = Palette::terminal();
        assert_eq!(
            ground(&terminal, Some(HostAppearance::Light)),
            Ground::Light
        );
        assert_eq!(ground(&terminal, Some(HostAppearance::Dark)), Ground::Dark);
        assert_eq!(ground(&terminal, None), Ground::Palette);
    }

    #[test]
    fn agent_icons_config_parses() {
        let config: crate::config::Config =
            toml::from_str("[ui.sidebar]\nagent_icons = \"letter\"\n").expect("valid config");
        assert_eq!(config.ui.sidebar.agent_icons, AgentIconsConfig::Letter);
        assert_eq!(
            crate::config::Config::default().ui.sidebar.agent_icons,
            AgentIconsConfig::Radar
        );
    }
}
