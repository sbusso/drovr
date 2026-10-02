//! drovr fork: vendor marks and state colours for the structured sidebar view.
//!
//! Values come from the herdr-radar plugin (MIT, Copyright (c) 2025 qintmb,
//! Copyright (c) 2026 herdr-kit contributors): icon font codepoints from
//! `lib/logos.js`, brand and state colours from `lib/palette.js`, state marks
//! and idle thresholds from `lib/config.js`. The icon font itself is not
//! bundled; `[ui.sidebar] agent_icons = "letter"` or `"none"` avoids it.

use ratatui::style::Color;

use super::projects::Presence;
use crate::app::state::Palette;
use crate::config::AgentIconsConfig;

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
/// the agent's last state change; unknown age counts as stale, as everywhere
/// else in the drovr sidebar.
pub(super) fn tone(presence: Presence, unknown: bool, idle_secs: Option<u64>) -> Tone {
    match presence {
        Presence::Working => Tone::Working,
        Presence::Done => Tone::Done,
        Presence::Blocked => Tone::Blocked,
        Presence::Unread => Tone::Unread,
        Presence::Idle if unknown => Tone::Unknown,
        Presence::Idle => match idle_secs {
            Some(secs) if secs < FRESH_SECS => Tone::IdleFresh,
            Some(secs) if secs < STALE_SECS => Tone::Idle,
            _ => Tone::IdleStale,
        },
    }
}

/// The mark in front of the title. Idle tiers have none: their colour says it.
/// Working uses radar's static mark because the client only redraws on
/// change; the spinner frames would freeze mid-turn.
pub(super) fn lead(tone: Tone) -> Option<&'static str> {
    match tone {
        Tone::Working => Some("○"),
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

/// Light or dark ink, judged from the active palette's main text colour.
pub(super) fn is_light(palette: &Palette) -> bool {
    match palette.text {
        Color::Rgb(r, g, b) => {
            u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114 < 128_000
        }
        _ => false,
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
fn ink(light: bool) -> Color {
    hex(if light { 0x16161c } else { 0xe9e9f0 })
}

/// Workspace header colour (radar `subtle`).
pub(super) fn subtle(light: bool) -> Color {
    hex(if light { 0x7c7f93 } else { 0xa8abbd })
}

/// Title colour and boldness for a state; working wears the vendor's colour.
pub(super) fn title_style(tone: Tone, vendor: Option<&str>, light: bool) -> (Color, bool) {
    match tone {
        Tone::Working => (vendor.and_then(brand).unwrap_or(BRAND_OTHER), true),
        Tone::Done => (hex(0x4c9a5a), false),
        Tone::Blocked => (hex(0xc04a4a), false),
        Tone::Unknown => (hex(0x907aa9), false),
        Tone::Unread => (Color::Yellow, false),
        Tone::IdleFresh => (hex(if light { 0x416c4f } else { 0x95bba2 }), false),
        Tone::Idle => (hex(if light { 0x6b6259 } else { 0xa99e92 }), false),
        Tone::IdleStale => (hex(if light { 0x69696d } else { 0x8b8e9c }), false),
    }
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

/// The vendor mark and its colour; `None` when marks are off. Vendors without
/// a mark get a neutral dot in `neutral`.
pub(super) fn logo(
    vendor: Option<&str>,
    icons: AgentIconsConfig,
    light: bool,
    neutral: Color,
) -> Option<(String, Color)> {
    let mark = match (icons, vendor) {
        (AgentIconsConfig::None, _) => return None,
        (AgentIconsConfig::Radar, Some(vendor)) => font_glyph(vendor).map(String::from),
        (AgentIconsConfig::Letter, Some(vendor)) => vendor
            .chars()
            .next()
            .filter(char::is_ascii_alphanumeric)
            .map(|letter| letter.to_ascii_uppercase().to_string()),
        (_, None) => None,
    };
    Some(match mark {
        Some(mark) => (mark, vendor.and_then(brand).unwrap_or_else(|| ink(light))),
        None => ("•".to_owned(), neutral),
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
        assert_eq!(idle(Some(FRESH_SECS)), Tone::Idle);
        assert_eq!(idle(Some(STALE_SECS - 1)), Tone::Idle);
        assert_eq!(idle(Some(STALE_SECS)), Tone::IdleStale);
        assert_eq!(idle(None), Tone::IdleStale);
        assert_eq!(tone(Presence::Idle, true, Some(1)), Tone::Unknown);
        assert_eq!(tone(Presence::Working, true, None), Tone::Working);
        assert_eq!(tone(Presence::Blocked, false, None), Tone::Blocked);
    }

    #[test]
    fn state_colours_match_radar() {
        assert_eq!(
            title_style(Tone::Working, Some("claude"), false),
            (Color::Rgb(0xd9, 0x77, 0x57), true)
        );
        assert_eq!(
            title_style(Tone::Working, Some("codex"), false),
            (BRAND_OTHER, true)
        );
        assert_eq!(
            title_style(Tone::Done, None, true),
            (Color::Rgb(0x4c, 0x9a, 0x5a), false)
        );
        assert_eq!(
            title_style(Tone::Blocked, None, false).0,
            Color::Rgb(0xc0, 0x4a, 0x4a)
        );
        assert_eq!(
            title_style(Tone::Unknown, None, false).0,
            Color::Rgb(0x90, 0x7a, 0xa9)
        );
        assert_eq!(
            title_style(Tone::IdleFresh, None, false).0,
            Color::Rgb(0x95, 0xbb, 0xa2)
        );
        assert_eq!(
            title_style(Tone::IdleFresh, None, true).0,
            Color::Rgb(0x41, 0x6c, 0x4f)
        );
        assert_eq!(
            title_style(Tone::IdleStale, None, false).0,
            Color::Rgb(0x8b, 0x8e, 0x9c)
        );
        assert_eq!(lead(Tone::Idle), None);
        assert_eq!(lead(Tone::Done), Some("✓"));
    }

    #[test]
    fn logos_by_icon_setting() {
        let neutral = Color::Gray;
        assert_eq!(
            logo(Some("claude"), AgentIconsConfig::Radar, false, neutral),
            Some(("\u{e1a0}".to_owned(), Color::Rgb(0xd9, 0x77, 0x57)))
        );
        assert_eq!(
            logo(Some("codex"), AgentIconsConfig::Letter, false, neutral),
            Some(("C".to_owned(), ink(false)))
        );
        assert_eq!(
            logo(Some("droid"), AgentIconsConfig::Radar, false, neutral),
            Some(("•".to_owned(), neutral))
        );
        assert_eq!(
            logo(None, AgentIconsConfig::Letter, false, neutral),
            Some(("•".to_owned(), neutral))
        );
        assert_eq!(
            logo(Some("claude"), AgentIconsConfig::None, false, neutral),
            None
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
    fn light_detection_uses_text_luminance() {
        let mut palette = Palette::catppuccin();
        assert!(!is_light(&palette));
        palette.text = Color::Rgb(0x34, 0x3b, 0x58);
        assert!(is_light(&palette));
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
