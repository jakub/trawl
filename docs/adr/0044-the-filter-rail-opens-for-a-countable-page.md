# The filter rail opens for a countable page, and a hand choice holds for the session

status: accepted (2026-09-25), prep record for #231

At 900px and wider the Search page gives the filter rail a fixed 224px
column, whether or not the rail has anything to count
(`crates/trawl-web-ui/src/components/facet_sidebar.rs`, `.facet-panel` in
`crates/trawl-web-ui/styles/main.css`). Before the first query, on a
zero-row page, and on an aggregation-shaped result, that column holds a
header and an empty value search. The results table loses the width.
Below 900px the rail is already a disclosure above the results that starts
closed and opens only by hand.

The prep for #231 ran a cross-family dialectic and a grill. The human chose
what opens the rail, how long a hand choice lasts, and how the closed rail
looks.

## Decision

**At 900px and wider, the rail opens for a countable page and closes for a
settled answer that is not countable.** A countable page is a settled
result on screen, the snapshot page or the live ring, with at least one
field the rail may count. The rail decides this after the capability gate
(`Capabilities::raw_facets` and `input_field`) and #238's eligibility
rules, and before the value search narrows the list. Active filters alone
do not open the rail. The executed-scope strip already shows each filter
with its own remove control, and the closed rail shows the active count.

**Only a settled answer moves the rail.** A settled answer is a snapshot
response that succeeded or failed, a live frame, or a malformed link. While a
snapshot request is pending, the rail keeps its last state. A page turn, a
Haul of the same query, and a switch between the Events and Visualization
tabs do not move it. A live stream starts closed and opens on the first
countable frame. The rail changes width in the same frame as the results
table, and the change is not animated.

**A hand choice holds for the rest of the browser session.** When the
reader presses the rail's control, the rail stays open or closed as the
reader left it, whatever later answers arrive. The choice survives route
changes inside the app. Reload or sign-out clears it, and the rail returns
to automatic behaviour. No control resets the rail to automatic behaviour.
The choice is neither search URL state (ADR-0027) nor a `UiPrefs`
preference (ADR-0032), because a stored open or closed value cannot say
"follow the result". The app records the choice from the reader's press,
never from the `<details>` `toggle` event. That event also fires when the
rail opens or closes automatically, so a choice recorded from it would end
automatic behaviour on the first automatic open.

**The closed rail is a 32px strip at the sheet's left edge.** The strip is
the rail's `<summary>`, set vertically: a chevron, "Filters", and "· N
active" when filters exist. When the rail is open, the same `<summary>` is
the rail's header row, and Clear all sits beside it. One mounted
`<details>` still serves both layouts, so the value search text and the
group state survive a breakpoint crossing. If the rail closes while focus
is inside it, focus moves to the `<summary>`.

**An open rail with nothing to count says so.** It hides the value search
and shows one hint, "No field values to count." On an aggregation-shaped
result the hint is "Field values are not counted for an aggregate result."
The header, the active count, and Clear all remain, as #180 decided. On a
malformed link the open rail shows its header only (ADR-0027).

**Below 900px nothing changes.** The rail stays the closed-by-default
disclosure above the results. It opens only by hand, and its open state is
separate from the wide choice.

## Considered options

**Open the rail for active filters too.** Rejected. On an aggregation or a
zero-row page the rail would spend 224px on a header and a Clear all
control. The chips already remove filters one at a time, and Clear all is
one press behind the strip.

**Keep a hand choice until the reader leaves Search.** This matches the
lifetime of the chart type choice (ADR-0038), and it was the recommended
option. The human chose the browser session instead. A reader who closes
the rail, visits Health, and comes back finds the rail still closed.

**A "Filters" button in a heading row above the rail and the sheet, with no
closed width.** Rejected. The button moves the page's `h1` out of the
sheet, spends a row in every state, and sits away from the column it opens.
A button beside View and Export in the result header was rejected for the
same distance. Both buttons would also give the narrow and the wide layout
different controls.

**Hold an automatic change while the pointer or focus is in the results.**
Rejected. A held change lands later, on an unrelated press, which is the
jump the hold was meant to prevent. Tying the change to a settled answer
already makes it land with the table redraw.

**Share one hand choice across both widths.** Rejected. Closing the narrow
disclosure means "show me the rows now". Closing the wide rail means "I do
not want filters here". Separate choices also leave the narrow behaviour
and its tests unchanged.

## Consequences

- Desktop Playwright specs that expect an open rail at load must load a
  countable page or open the rail first.
- The user docs call the control the **Filters** rail, so "sidebar" names
  only the navigation sidebar.
- fleet-ui does not change, so Coastwatch is not affected.
