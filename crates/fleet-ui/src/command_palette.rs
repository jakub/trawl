// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Route inventory, filtering, selection and keyboard rules for ADR-0031.
//! Browser components project their sidebar groups into borrowed inputs,
//! so the palette's grouping is the sidebar's grouping (ADR-0032). These
//! rules need no DOM or consumer routing state.

use std::collections::HashSet;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandInput<'a> {
    pub label: &'a str,
    pub path: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    pub label: String,
    pub path: String,
    /// The sidebar group's heading, absent for an unlabelled group.
    pub group: Option<String>,
    /// Position of the owning group in the inventory, so consecutive
    /// commands can be regrouped without comparing labels.
    pub group_ordinal: usize,
    /// Position in the original flattened inventory, before deduplication.
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

/// Flatten the groups in source order, keeping their labels. The first
/// command at a path wins; equal labels at different paths remain
/// separate.
pub(crate) fn commands_from<'a, G, I>(groups: G) -> Vec<Command>
where
    G: IntoIterator<Item = (Option<&'a str>, I)>,
    I: IntoIterator<Item = CommandInput<'a>>,
{
    let mut paths = HashSet::new();
    groups
        .into_iter()
        .enumerate()
        .flat_map(|(group_ordinal, (label, items))| {
            items
                .into_iter()
                .map(move |input| (group_ordinal, label, input))
        })
        .enumerate()
        .filter(|(_, (_, _, input))| paths.insert(input.path))
        .map(|(ordinal, (group_ordinal, label, input))| Command {
            label: input.label.to_owned(),
            path: input.path.to_owned(),
            group: label.map(str::to_owned),
            group_ordinal,
            ordinal,
        })
        .collect()
}

/// The consecutive runs one group each, as `(label, indices into
/// `commands`)`. Filtering drops commands but never reorders them, so a
/// run is a stretch of equal `group_ordinal`.
pub(crate) fn group_runs(commands: &[Command]) -> Vec<(Option<&str>, Vec<usize>)> {
    let mut runs: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
    let mut open: Option<usize> = None;
    for (index, command) in commands.iter().enumerate() {
        if open != Some(command.group_ordinal) {
            runs.push((command.group.as_deref(), Vec::new()));
            open = Some(command.group_ordinal);
        }
        if let Some(run) = runs.last_mut() {
            run.1.push(index);
        }
    }
    runs
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

/// Whether an input accepts writable text. `input_type` is the normalized
/// HTMLInputElement.type value, so a missing or invalid type arrives as text.
fn text_input_is_editable(input_type: &str, disabled: bool, read_only: bool) -> bool {
    !disabled
        && !read_only
        && matches!(
            input_type,
            "text" | "search" | "url" | "tel" | "email" | "password" | "number"
        )
}

/// Event facts normalized by Shell's browser listener. Editable includes
/// enabled writable text controls and contenteditable descendants.
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
        commands_from([
            (
                None,
                vec![input("Search", "/search"), input("Settings", "/settings")],
            ),
            (
                Some("Sections"),
                vec![
                    input("Schema", "/search/schema"),
                    input("Schema", "/settings/schema"),
                ],
            ),
        ])
    }

    #[test]
    fn commands_from_deduplicates_exact_paths_first_wins() {
        let commands = commands_from([
            (
                None,
                vec![
                    input("Search", "/search"),
                    input("Duplicate mode", "/search"),
                ],
            ),
            (
                Some("Sections"),
                vec![
                    input("Rail search", "/search"),
                    input("Schema", "/search/schema"),
                    input("Schema", "/settings"),
                    input("Health", "/settings"),
                    input("Case matters", "/Settings"),
                    input("Slash matters", "/settings/"),
                ],
            ),
        ]);
        assert_eq!(
            commands
                .iter()
                .map(|c| (
                    c.label.as_str(),
                    c.path.as_str(),
                    c.group.as_deref(),
                    c.ordinal
                ))
                .collect::<Vec<_>>(),
            vec![
                ("Search", "/search", None, 0),
                ("Schema", "/search/schema", Some("Sections"), 3),
                ("Schema", "/settings", Some("Sections"), 4),
                ("Case matters", "/Settings", Some("Sections"), 6),
                ("Slash matters", "/settings/", Some("Sections"), 7),
            ]
        );
    }

    #[test]
    fn filter_requires_every_token_across_label_or_path() {
        let commands = commands_from([(None, [input("Schema browser", "/settings/catalog")])]);
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
    fn unlabelled_group_renders_no_header() {
        // The unlabelled run keeps its options directly under the
        // listbox; only a labelled run earns a role="group" wrapper.
        assert_eq!(
            group_runs(&inventory()),
            vec![(None, vec![0, 1]), (Some("Sections"), vec![2, 3])]
        );
        // Runs follow source order in both directions: an unlabelled
        // group after a labelled one stays after it.
        let mixed = commands_from([
            (Some("Sections"), vec![input("Schema", "/search/schema")]),
            (None, vec![input("Search", "/search")]),
        ]);
        assert_eq!(
            group_runs(&mixed),
            vec![(Some("Sections"), vec![0]), (None, vec![1])]
        );
    }

    #[test]
    fn empty_inventory_has_no_trigger_chord_or_selection() {
        let commands = commands_from::<Vec<(Option<&str>, Vec<CommandInput<'_>>)>, _>(vec![]);
        assert!(!palette_available(&commands));
        let mut state = PaletteState::new(&commands);
        state.set_filter(&commands, "search");
        assert!(state.visible.is_empty());
        assert_eq!(state.selected, None);
        assert!(palette_available(&inventory()));
        assert!(palette_available(&commands_from([(
            None,
            [input("Rail", "/rail")]
        )])));
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
    fn editable_input_table_excludes_non_text_disabled_and_read_only_controls() {
        for input_type in [
            "text", "search", "url", "tel", "email", "password", "number",
        ] {
            for disabled in [false, true] {
                for read_only in [false, true] {
                    assert_eq!(
                        text_input_is_editable(input_type, disabled, read_only),
                        !disabled && !read_only,
                        "type={input_type} disabled={disabled} read_only={read_only}",
                    );
                }
            }
        }
        for input_type in [
            "checkbox",
            "radio",
            "range",
            "file",
            "button",
            "hidden",
            "color",
            "date",
            "datetime-local",
            "month",
            "week",
            "time",
            "submit",
            "reset",
            "image",
        ] {
            assert!(
                !text_input_is_editable(input_type, false, false),
                "{input_type}"
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

#[cfg(target_arch = "wasm32")]
mod component {
    use leptos::ev;
    use leptos::html::Div;
    use leptos::prelude::*;
    use leptos_router::components::A;
    use leptos_router::hooks::use_location;
    use leptos_use::{use_event_listener, use_window};
    use wasm_bindgen::JsCast;

    use super::{Command, PaletteState, group_runs, text_input_is_editable};
    use crate::icon::{Icon, IconView};
    use crate::overlay::{FocusPolicy, OverlayLayer, use_overlay_layer_with};
    use crate::roving::{Nav, next_index};

    /// Read inherited contenteditable state on the actual target. Looking for
    /// any contenteditable ancestor would incorrectly include a nested false
    /// region. The composed path also reaches an editor inside a shadow root.
    pub(crate) fn editable_target(event: &web_sys::KeyboardEvent) -> bool {
        let element = event
            .composed_path()
            .iter()
            .find_map(|node| node.dyn_into::<web_sys::Element>().ok());
        element.is_some_and(|element| {
            if let Some(control) = element.closest("input, textarea").ok().flatten() {
                // :disabled also covers controls disabled by a fieldset.
                let disabled = control.matches(":disabled").unwrap_or(true);
                if let Some(input) = control.dyn_ref::<web_sys::HtmlInputElement>() {
                    return text_input_is_editable(&input.type_(), disabled, input.read_only());
                }
                return control
                    .dyn_ref::<web_sys::HtmlTextAreaElement>()
                    .is_some_and(|textarea| !disabled && !textarea.read_only());
            }
            element
                .dyn_ref::<web_sys::HtmlElement>()
                .is_some_and(web_sys::HtmlElement::is_content_editable)
        })
    }

    #[component]
    pub(crate) fn CommandPalette(
        commands: Memo<Vec<Command>>,
        open: RwSignal<bool>,
        on_close: Callback<()>,
        layer_slot: StoredValue<Option<OverlayLayer>>,
    ) -> impl IntoView {
        let scrim_ref = NodeRef::<Div>::new();
        let panel_ref = NodeRef::<Div>::new();
        let list_ref = NodeRef::<Div>::new();
        let location = use_location();
        let state = RwSignal::new(PaletteState::new(&commands.get_untracked()));
        // Arrow selection changes must not rebuild the options. Keep their
        // DOM stable until the filtered inventory itself changes.
        let visible = Memo::new(move |_| state.with(|state| state.visible.clone()));
        let selected = Signal::derive(move || state.with(|state| state.selected));

        let layer = use_overlay_layer_with(FocusPolicy::Trap, move || {
            panel_ref.get().map(web_sys::Element::from)
        });
        layer_slot.set_value(Some(layer));
        on_cleanup(move || {
            if layer_slot.try_get_value().flatten() == Some(layer) {
                layer_slot.try_set_value(None);
            }
        });

        // Consumer chrome may change while open. Refilter its current routes
        // and reset selection before an old command can be activated.
        Effect::new(move |_| {
            let commands = commands.get();
            let filter = state.with_untracked(|state| state.filter.clone());
            state.update(|state| state.set_filter(&commands, &filter));
        });

        let _ = use_event_listener(use_window(), ev::keydown, move |event| {
            if !event.default_prevented()
                && !event.is_composing()
                && layer.is_topmost()
                && event.key() == "Escape"
            {
                event.prevent_default();
                on_close.run(());
            }
        });

        view! {
            <div
                class="command-palette-scrim"
                node_ref=scrim_ref
                hidden=move || !open.get()
                on:mousedown=move |event: web_sys::MouseEvent| {
                    if !layer.is_topmost() { return; }
                    if let Some(scrim) = scrim_ref.get_untracked()
                        && let Some(target) = event.target()
                        && let Some(element) = target.dyn_ref::<web_sys::Element>()
                        && element.is_same_node(Some(scrim.as_ref()))
                    {
                        event.prevent_default();
                        on_close.run(());
                    }
                }
            >
                <div
                    class="command-palette"
                    role="dialog"
                    aria-label="Command palette"
                    aria-modal="true"
                    tabindex="-1"
                    node_ref=panel_ref
                >
                    <div class="command-palette-search">
                        {palette_input(commands, state, list_ref, layer)}
                        <button
                            type="button"
                            class="command-palette-close"
                            aria-label="Close command palette"
                            title="Close (Esc)"
                            on:click=move |_| on_close.run(())
                        >
                            <IconView icon=Icon::Close size=16 stroke_width=1.5/>
                        </button>
                    </div>
                    <div
                        class="command-palette-list"
                        id="fleet-command-palette-list"
                        role="listbox"
                        aria-label="Pages"
                        node_ref=list_ref
                    >
                        {move || render_groups(&visible.get(), selected, location.pathname, on_close)}
                    </div>
                    <Show when=move || visible.with(Vec::is_empty)>
                        <p class="command-palette-empty">"No matching pages"</p>
                    </Show>
                    <div class="command-palette-status" role="status" aria-live="polite" aria-atomic="true">
                        {move || match visible.with(Vec::len) {
                            0 => "No matching pages".to_string(),
                            1 => "1 page available".to_string(),
                            count => format!("{count} pages available"),
                        }}
                    </div>
                </div>
            </div>
        }
    }

    fn palette_input(
        commands: Memo<Vec<Command>>,
        state: RwSignal<PaletteState>,
        list_ref: NodeRef<Div>,
        layer: OverlayLayer,
    ) -> impl IntoView {
        view! {
            <input
                class="command-palette-input"
                type="text"
                role="combobox"
                aria-label="Find a page"
                aria-autocomplete="list"
                aria-haspopup="listbox"
                aria-controls="fleet-command-palette-list"
                aria-expanded="true"
                aria-activedescendant=move || state.with(|state| {
                    state.selected.and_then(|index| state.visible.get(index)).map(Command::option_id)
                })
                autocomplete="off"
                spellcheck="false"
                placeholder="Go to a page…"
                prop:value=move || state.with(|state| state.filter.clone())
                on:input=move |event| {
                    let filter = event_target_value(&event);
                    commands.with_untracked(|commands| {
                        state.update(|state| state.set_filter(commands, &filter));
                    });
                    if let Some(list) = list_ref.get_untracked() {
                        list.set_scroll_top(0);
                    }
                }
                on:keydown=move |event| on_input_keydown(&event, state, layer)
            />
        }
    }

    fn on_input_keydown(
        event: &web_sys::KeyboardEvent,
        state: RwSignal<PaletteState>,
        layer: OverlayLayer,
    ) {
        if event.default_prevented() || event.is_composing() || !layer.is_topmost() {
            return;
        }
        let nav = match event.key().as_str() {
            "ArrowDown" => Some(Nav::Next),
            "ArrowUp" => Some(Nav::Prev),
            "Enter" => {
                event.prevent_default();
                // Click the same router anchor a pointer activates. No
                // second navigation API and no event for an empty list.
                let id = state.with_untracked(|state| {
                    state
                        .selected
                        .and_then(|index| state.visible.get(index))
                        .map(Command::option_id)
                });
                if let Some(anchor) = id
                    .and_then(|id| document().get_element_by_id(&id))
                    .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
                {
                    anchor.click();
                }
                return;
            }
            // Home/End belong to the input caret, not the option list.
            _ => None,
        };
        let Some(nav) = nav else { return };
        event.prevent_default();
        let next = state.with_untracked(|state| {
            state
                .selected
                .and_then(|selected| next_index(selected, state.visible.len(), nav))
        });
        if let Some(next) = next {
            state.update(|state| state.selected = Some(next));
            let id = state.with_untracked(|state| state.visible[next].option_id());
            if let Some(element) = document().get_element_by_id(&id) {
                let options = web_sys::ScrollIntoViewOptions::new();
                options.set_block(web_sys::ScrollLogicalPosition::Nearest);
                options.set_inline(web_sys::ScrollLogicalPosition::Nearest);
                element.scroll_into_view_with_scroll_into_view_options(&options);
            }
        }
    }

    fn render_groups(
        commands: &[Command],
        selected: Signal<Option<usize>>,
        pathname: Memo<String>,
        on_close: Callback<()>,
    ) -> impl IntoView + use<> {
        group_runs(commands)
            .into_iter()
            .map(|(label, indices)| {
                let options: Vec<_> = indices
                    .into_iter()
                    .map(|index| {
                        render_option(&commands[index], index, selected, pathname, on_close)
                    })
                    .collect();
                // An unlabelled run has no heading and no wrapper: a
                // role="group" with no accessible name is a defect, and
                // the options belong to the listbox either way.
                match label {
                    Some(label) => {
                        let heading = label.to_owned();
                        view! {
                            <div class="command-palette-group" role="group" aria-label=label.to_owned()>
                                <div class="command-palette-group-label" aria-hidden="true">
                                    {heading}
                                </div>
                                {options}
                            </div>
                        }
                        .into_any()
                    }
                    None => options.into_any(),
                }
            })
            .collect::<Vec<_>>()
    }

    fn render_option(
        command: &Command,
        index: usize,
        selected: Signal<Option<usize>>,
        pathname: Memo<String>,
        on_close: Callback<()>,
    ) -> AnyView {
        let command = command.clone();
        let id = command.option_id();
        let href = command.path.clone();
        let path = command.path.clone();
        let label = command.label.clone();
        view! {
            <A
                href=href
                exact=true
                attr:id=id
                attr:class="command-palette-option"
                attr:role="option"
                attr:tabindex="-1"
                attr:aria-selected=move || (selected.get() == Some(index)).to_string()
                on:mousedown=move |event: web_sys::MouseEvent| {
                    // Every pointer press keeps focus on the combobox.
                    // Click and auxclick retain native new-tab behavior.
                    event.prevent_default();
                }
                on:click=move |event: web_sys::MouseEvent| {
                    if ordinary_click(&event) && !event.default_prevented() {
                        on_close.run(());
                    }
                }
            >
                <span class="command-palette-label">{label}</span>
                <span class="command-palette-path">{path}</span>
                {move || command.is_current(&pathname.get()).then(|| view! {
                    <span class="command-palette-current">"current"</span>
                })}
            </A>
        }
        .into_any()
    }

    fn ordinary_click(event: &web_sys::MouseEvent) -> bool {
        event.button() == 0
            && !event.meta_key()
            && !event.ctrl_key()
            && !event.alt_key()
            && !event.shift_key()
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use component::{CommandPalette, editable_target};
