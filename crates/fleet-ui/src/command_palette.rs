// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Route inventory, filtering, selection and keyboard rules for ADR-0031.
//! Browser components project their mode tabs and rail items into borrowed
//! inputs. These rules need no DOM or consumer routing state.

use std::collections::HashSet;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandInput<'a> {
    pub label: &'a str,
    pub path: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandGroup {
    Modes,
    Sections,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    pub label: String,
    pub path: String,
    pub group: CommandGroup,
    /// Position in the original mode-then-rail inventory, before deduplication.
    pub ordinal: usize,
}

impl Command {
    pub fn option_id(&self) -> String {
        format!("fleet-command-palette-option-{}", self.ordinal)
    }

    pub fn is_current(&self, pathname: &str) -> bool {
        self.path == pathname
    }
}

/// Preserve source order and labels. A mode wins over any rail entry with
/// the same exact path; equal labels at different paths remain separate.
pub(crate) fn commands_from<'a>(
    modes: impl IntoIterator<Item = CommandInput<'a>>,
    rail: impl IntoIterator<Item = CommandInput<'a>>,
) -> Vec<Command> {
    let mut paths = HashSet::new();
    modes
        .into_iter()
        .map(|input| (input, CommandGroup::Modes))
        .chain(
            rail.into_iter()
                .map(|input| (input, CommandGroup::Sections)),
        )
        .enumerate()
        .filter(|(_, (input, _))| paths.insert(input.path))
        .map(|(ordinal, (input, group))| Command {
            label: input.label.to_owned(),
            path: input.path.to_owned(),
            group,
            ordinal,
        })
        .collect()
}

/// Empty inventories omit the trigger and disable the global chord.
pub(crate) fn palette_available(commands: &[Command]) -> bool {
    !commands.is_empty()
}

pub(crate) fn filter_commands(commands: &[Command], filter: &str) -> Vec<Command> {
    let lower = filter.to_lowercase();
    let tokens: Vec<_> = lower.split_whitespace().collect();
    commands
        .iter()
        .filter(|command| {
            let label = command.label.to_lowercase();
            let path = command.path.to_lowercase();
            tokens
                .iter()
                .all(|token| label.contains(token) || path.contains(token))
        })
        .cloned()
        .collect()
}

/// Each mount starts with an empty filter. Every filter change selects the
/// first match, or nothing when no command matches. Arrow moves update only
/// `selected`, which indexes `visible`, never the original inventory.
#[derive(Clone, Debug)]
pub(crate) struct PaletteState {
    pub filter: String,
    pub visible: Vec<Command>,
    pub selected: Option<usize>,
}

impl PaletteState {
    pub fn new(commands: &[Command]) -> Self {
        let mut state = Self {
            filter: String::new(),
            visible: Vec::new(),
            selected: None,
        };
        state.set_filter(commands, "");
        state
    }

    pub fn set_filter(&mut self, commands: &[Command], filter: &str) {
        filter.clone_into(&mut self.filter);
        self.visible = filter_commands(commands, filter);
        self.selected = (!self.visible.is_empty()).then_some(0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlatformKbdHint {
    pub label: &'static str,
    pub aria_keyshortcuts: &'static str,
    pub is_macos: bool,
}

/// iPad/iPhone/iPod use the Meta hint but do not inherit macOS kill-line.
/// A desktop-mode iPad UA that reports only Macintosh is indistinguishable
/// from macOS by UA alone, so it deliberately follows the Macintosh rule.
pub(crate) fn platform_kbd_hint(ua: &str) -> PlatformKbdHint {
    let ua = ua.to_ascii_lowercase();
    let ios = ["ipad", "iphone", "ipod"]
        .iter()
        .any(|name| ua.contains(name));
    let is_macos = !ios && (ua.contains("macintosh") || ua.contains("mac os x"));
    let apple = ios || is_macos;
    PlatformKbdHint {
        label: if apple { "⌘K" } else { "Ctrl+K" },
        aria_keyshortcuts: if apple { "Meta+K" } else { "Control+K" },
        is_macos,
    }
}

/// Event facts normalized by Shell's browser listener. Editable includes
/// inputs, textareas and contenteditable descendants.
#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent browser event flags.
pub(crate) struct ChordFacts<'a> {
    pub key: &'a str,
    pub ctrl: bool,
    pub meta: bool,
    pub alt: bool,
    pub shift: bool,
    pub repeat: bool,
    pub default_prevented: bool,
    pub composing: bool,
    pub editable: bool,
}

pub(crate) fn is_palette_chord(facts: ChordFacts<'_>, is_macos: bool) -> bool {
    facts.key.eq_ignore_ascii_case("k")
        && (facts.ctrl ^ facts.meta)
        && !facts.alt
        && !facts.shift
        && !facts.repeat
        && !facts.default_prevented
        && !facts.composing
        && !(is_macos && facts.ctrl && facts.editable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(label: &'a str, path: &'a str) -> CommandInput<'a> {
        CommandInput { label, path }
    }

    fn inventory() -> Vec<Command> {
        commands_from(
            [input("Search", "/search"), input("Settings", "/settings")],
            [
                input("Schema", "/search/schema"),
                input("Schema", "/settings/schema"),
            ],
        )
    }

    #[test]
    fn commands_from_deduplicates_exact_paths_first_wins() {
        let commands = commands_from(
            [
                input("Search", "/search"),
                input("Duplicate mode", "/search"),
            ],
            [
                input("Rail search", "/search"),
                input("Schema", "/search/schema"),
                input("Schema", "/settings"),
                input("Health", "/settings"),
                input("Case matters", "/Settings"),
                input("Slash matters", "/settings/"),
            ],
        );
        assert_eq!(
            commands
                .iter()
                .map(|c| (c.label.as_str(), c.path.as_str(), c.group, c.ordinal))
                .collect::<Vec<_>>(),
            vec![
                ("Search", "/search", CommandGroup::Modes, 0),
                ("Schema", "/search/schema", CommandGroup::Sections, 3),
                ("Schema", "/settings", CommandGroup::Sections, 4),
                ("Case matters", "/Settings", CommandGroup::Sections, 6),
                ("Slash matters", "/settings/", CommandGroup::Sections, 7),
            ]
        );
    }

    #[test]
    fn filter_requires_every_token_across_label_or_path() {
        let commands = commands_from([], [input("Schema browser", "/settings/catalog")]);
        for filter in [
            "",
            " \t\n ",
            "SCHEMA",
            "/SETTINGS",
            "browser catalog",
            " settings\nSCHEMA\t",
        ] {
            assert_eq!(filter_commands(&commands, filter), commands, "{filter:?}");
        }
        for filter in ["schema missing", "settings missing", "browser/catalog"] {
            assert!(filter_commands(&commands, filter).is_empty(), "{filter:?}");
        }
    }

    #[test]
    fn filter_preserves_order_paths_and_original_option_ids() {
        let commands = inventory();
        let matches = filter_commands(&commands, "schema");
        assert_eq!(matches, commands[2..]);
        assert_eq!(matches[0].option_id(), "fleet-command-palette-option-2");
        assert_eq!(matches[1].option_id(), "fleet-command-palette-option-3");
        assert_ne!(matches[0].option_id(), matches[1].option_id());
    }

    #[test]
    fn current_affix_requires_exact_pathname() {
        let command = &inventory()[0];
        assert!(command.is_current("/search"));
        for pathname in ["/search/schema", "/search/", "/Search", "/"] {
            assert!(!command.is_current(pathname), "{pathname}");
        }
    }

    #[test]
    fn selection_resets_on_filter_changes_and_reopen() {
        let commands = inventory();
        let mut state = PaletteState::new(&commands);
        assert_eq!(state.selected, Some(0));
        state.selected = Some(3);
        state.set_filter(&commands, "schema");
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.visible.len(), 2);
        state.set_filter(&commands, "no match");
        assert_eq!(state.selected, None);
        assert!(state.visible.is_empty());
        state.set_filter(&commands, "schema");
        assert_eq!(state.selected, Some(0));
        let reopened = PaletteState::new(&commands);
        assert_eq!(reopened.filter, "");
        assert_eq!(reopened.visible, commands);
        assert_eq!(reopened.selected, Some(0));
    }

    #[test]
    fn empty_inventory_has_no_trigger_chord_or_selection() {
        let commands = commands_from([], []);
        assert!(!palette_available(&commands));
        let mut state = PaletteState::new(&commands);
        state.set_filter(&commands, "search");
        assert!(state.visible.is_empty());
        assert_eq!(state.selected, None);
        assert!(palette_available(&inventory()));
        assert!(palette_available(&commands_from(
            [],
            [input("Rail", "/rail")]
        )));
    }

    #[test]
    fn platform_table_drives_primary_hint_aria_and_macos_classification() {
        for (ua, label, aria, is_macos) in [
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
                "⌘K",
                "Meta+K",
                true,
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
                "Ctrl+K",
                "Control+K",
                false,
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64)",
                "Ctrl+K",
                "Control+K",
                false,
            ),
            (
                "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)",
                "⌘K",
                "Meta+K",
                false,
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)",
                "⌘K",
                "Meta+K",
                false,
            ),
            (
                "Mozilla/5.0 (iPod touch; CPU iPhone OS 15_0 like Mac OS X)",
                "⌘K",
                "Meta+K",
                false,
            ),
            // iPadOS desktop mode can present this same UA as desktop Safari.
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605.1.15 Version/13.0 Safari/605.1.15",
                "⌘K",
                "Meta+K",
                true,
            ),
            (
                "Mozilla/5.0 (Linux; Android 14)",
                "Ctrl+K",
                "Control+K",
                false,
            ),
            ("MACINTOSH", "⌘K", "Meta+K", true),
            ("", "Ctrl+K", "Control+K", false),
            ("unknown", "Ctrl+K", "Control+K", false),
        ] {
            assert_eq!(
                platform_kbd_hint(ua),
                PlatformKbdHint {
                    label,
                    aria_keyshortcuts: aria,
                    is_macos
                },
                "{ua}"
            );
        }
    }

    #[test]
    fn chord_modifier_and_editor_matrix() {
        for is_macos in [false, true] {
            for editable in [false, true] {
                for ctrl in [false, true] {
                    for meta in [false, true] {
                        let facts = ChordFacts {
                            key: "k",
                            ctrl,
                            meta,
                            editable,
                            ..Default::default()
                        };
                        assert_eq!(
                            is_palette_chord(facts, is_macos),
                            (ctrl ^ meta) && !(is_macos && ctrl && editable),
                            "macos={is_macos}, {facts:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn chord_rejects_altered_consumed_repeated_and_composing_events() {
        for (ctrl, meta) in [(true, false), (false, true)] {
            let base = ChordFacts {
                key: "k",
                ctrl,
                meta,
                ..Default::default()
            };
            for facts in [
                ChordFacts { key: "x", ..base },
                ChordFacts {
                    key: "KeyK",
                    ..base
                },
                ChordFacts { key: "", ..base },
                ChordFacts { alt: true, ..base },
                ChordFacts {
                    shift: true,
                    ..base
                },
                ChordFacts {
                    repeat: true,
                    ..base
                },
                ChordFacts {
                    default_prevented: true,
                    ..base
                },
                ChordFacts {
                    composing: true,
                    ..base
                },
            ] {
                for is_macos in [false, true] {
                    assert!(!is_palette_chord(facts, is_macos), "{facts:?}");
                }
            }
            assert!(is_palette_chord(ChordFacts { key: "K", ..base }, false));
        }
    }
}
