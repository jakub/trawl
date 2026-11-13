# An investigation ends in a conclusion that cites its findings

status: accepted (2026-11-12), prep record for the investigation briefing on milestone 1.1 (issues #452, #453, #454 and #455); human rulings of 2026-11-12 recorded below; amends ADR-0059

The investigation page (ADR-0059) reads as a log: the question, the latest finding, the activity, the findings, then the steps. The human asked for a briefing that walks a reader through what an agent found: the bottom line first, then situation, background, assessment and recommendation. The r6 mockup showed that shape, but each word of its bottom line and recommendation was cut by hand from a closing note and from findings. The live product has no field for any of them. A finding has one `statement` of up to 4096 bytes, which mixes the claim with the argument for it. Nothing records the investigation's answer, what it left open, or what should happen next. This ADR records the two additions the briefing needs and how they fit ADR-0059's log.

## Decision

**A finding has a statement and an optional basis.** `statement` keeps its name and becomes the headline: one sentence a reader can check against the cited rows. A new write holds at most 512 bytes (`FINDING_STATEMENT_MAX_BYTES` goes from 4096 to 512). A new optional `basis` of at most 4096 bytes holds what the cited rows show and how they lead to the statement. The basis is attributed text that its author chose to store and that every reader sees. It is not hidden reasoning, so ADR-0059's rule that no hidden reasoning is stored still holds. trawld checks the bounds of the basis, not its truth. Stored findings keep their `statement` and have no basis. They decode unchanged and need no migration, because `EntryPayload` has no `deny_unknown_fields` and the new field defaults to absent. A stored statement longer than 512 bytes still decodes and displays whole. The page never splits a statement at its first sentence, because a split would push a correction later in the text into a fold.

**The log gains a `conclusion` kind.** A conclusion is the investigation's closing summary:

| Field | Holds | Bound |
|---|---|---|
| `answer` | the direct answer to the question, or why there is none; null only in a withdrawal | 1024 bytes, one line |
| `answered` | the parts of the question the evidence settled | list |
| `not_answered` | the parts it did not settle, and why | list |
| `caveats` | what limits the answer | list |
| `next` | checks that would settle what is open | list |
| `fixes` | proposed changes, as proposals only | list |
| `followups` | separate work outside the question | list |
| `based_on_seq` | the investigation `seq` the writer observed, set by trawld | |

A list holds at most 8 items. An item is at most 512 bytes, one line, not blank, and holds no NUL. An absent list is empty. A conclusion cites at most 16 distinct findings. All its authored text together is at most 8192 bytes, and its encoded payload stays under 64 KiB, the size the investigations migration assumes for every kind except `step`. No `method` field exists. The page computes its method line from the steps the server recorded, and authored text could claim work that the steps do not show.

**A conclusion replaces the last one whole.** Each conclusion is complete. Nothing merges with the one before it, and an absent list clears that list. The latest conclusion entry is the investigation's conclusion. Earlier ones stay in the log, and the page lists them. A writer withdraws the conclusion by writing one with `answer: null` and every list empty. That is the only valid shape with a null answer. The withdrawal is a conclusion entry with its own actor and seq, so the page can tell four states apart: no conclusion, a conclusion, a withdrawn conclusion, and a conclusion it has not read yet.

**A conclusion is an overwrite under the investigation's sequence, on its own route.** `PUT /api/v1/investigations/{id}/conclusion` carries `expected_seq`, the `seq` the writer observed, and an optional `seen_seq` for activity. Under the investigation's lock, trawld refuses a closed investigation with 409 `investigation_closed`, then refuses a moved sequence with 409 `sequence_conflict` and `{expected, current}`, then checks the citations, then appends the entry at `seq + 1`. Any write between the writer's read and the `PUT` conflicts: a note, a finding, a status change, a step outcome, or a question edit. trawld stores the accepted `expected_seq` as `based_on_seq`. A resend after a lost answer cannot write twice, because the first write moved the sequence. The route names `policy::Authorized<needs::InvestigationWrite>` ahead of its body (ADR-0059, amendment of 2026-10-25). The write locks, prunes and slides the evidence clock like every other write. `PATCH` still changes exactly one of title, question and status.

**Citations are inline `#N`, read by one tokenizer.** The answer or an item cites finding N by writing `#N` in its text. One function in trawl-api finds the tokens. trawld's validation, the page's chips and the MCP render all call it, so they cannot disagree about what `#12` means. A token is a `#` that is not preceded by an ASCII letter, digit, `_`, `&` or `#`, followed by 1 to 19 ASCII digits with no leading zero, and not followed by an ASCII letter, digit or `_`. Text inside a backtick code span is not a citation. A run of backticks opens a code span only when a run of the same length closes it later in the same text; otherwise the run is literal. Every cited N must be a `finding` entry of this investigation, and the write's transaction checks this. A token that names anything else, including a number past `i64::MAX`, refuses the write with 400 `bad_request` naming the field, the item index and the token. The writer can read the whole log, so naming the token reveals nothing. Each `answered` item cites at least one finding. The answer and the other lists may cite none. Only findings are cited, because the finding carries the evidence: the chain runs row, finding, conclusion. One risk remains. `PR #12` cites finding 12 without warning when 12 is a finding, so the field documentation tells writers to put external references in backticks.

**The read head names the latest conclusion.** `Investigation` gains `conclusion_seq`, the seq of the latest conclusion entry, absent when there is none. Only the conclusion write sets it, inside its own transaction. Every write takes a new sequence, so `updated_seq` is unique within an investigation, and a read with `since=S-1&limit=1` returns entry S. A page that has not read as far as `conclusion_seq` fetches that one entry. It never shows an older conclusion as the latest.

**Freshness is shown as facts.** A conclusion stays as its author wrote it. When something changed after `based_on_seq`, the bottom line says what changed and links to it. The changes counted are an edit of the question; a later step, step outcome, note, keep or finding; and a status write to a cited finding. Each cited finding shows its current status. The page writes no generic needs-review label, because a status is its author's claim and the board judges nothing. A conclusion never changes a finding's status, and a status change never edits a conclusion.

**The closed gate stays.** A closed investigation refuses a conclusion as it refuses every other write. The order is: conclude, then close with the seq the conclusion returned. To revise the conclusion of a closed investigation, reopen it first. A person who closed an investigation never sees a conclusion appear in it afterwards.

**Any writer may conclude.** Any key holding `investigation_write`, a person's or an agent's, may write, replace or withdraw a conclusion. Creator identity is attribution, never a gate (ADR-0059). The page names the actor of every conclusion. The trial operator key (ADR-0045) holds the permission, so it can conclude.

**The MCP input stays flat.** The `investigation` tool takes each list as a root array of strings, which ADR-0058's flat-input rule and its test allow. Typed items with a citation array inside each item would fail that test.

**Clients upgrade with trawld.** A 1.0 client decodes the log strictly, so one conclusion entry makes its whole read of that investigation fail. A 1.0 trawld refuses a `basis` field, because `AppendEntry` has `deny_unknown_fields`. The 1.1 release notes say that every `trawl mcp`, CLI and TUI upgrades together with trawld. The SPA already ships with trawld.

### Human rulings (2026-11-12)

The human ruled on each open question and then confirmed the design. The answers, quoted verbatim:

1. Trust: "Any writer, closed blocks (Recommended)".
2. Old clients: "Lockstep upgrade".
3. Statement: "Cap new writes at 512 (Recommended)".
4. Names: "basis + conclusion (Recommended)".
5. Slicing: "Four issues (Recommended)".
6. Phone order: "Bottom line first (Recommended)".
7. Ledger: "Keep it dropped (Recommended)".
8. Headings: "Neutral (Recommended)".
9. Shared understanding: "Yes, write the issues". The work lands after 1.0, on milestone 1.1.

Rulings 5 to 8 shape the page, not the contracts above. The work is four issues. The bottom line comes first at every width, in one DOM order. No one-line claim ledger is added. The section headings are questions that do not assume an agent wrote the investigation.

## Premise checks

Checked at `feb17b3e`.

- `EntryPayload` is internally tagged and has no `deny_unknown_fields` (`crates/trawl-api/src/investigations.rs:332-335`). `AppendEntry` has it (`:466-467`).
- The only `UPDATE investigation_log` in the store is step completion (`crates/trawl-server/src/store/investigations.rs:1825`), so finding and finding-status rows never change after insert.
- The flat-input checker refuses a root object property and an array inside an array item (`crates/trawl-mcp/tests/schemas.rs:129-163`).
- `git tag -l 'v1*'` prints nothing; the latest tag is v0.9.4.

## Considered and rejected

- **A conclusion as a plain append, where the latest wins.** ADR-0059 lets appends skip the sequence because they overwrite nothing. A conclusion replaces the investigation's answer, so last-arrival-wins would let a writer replace an answer it never saw.
- **A compare-and-swap on the conclusion alone (`replaces`).** It would let notes and steps land during a long write. But a finding refuted, or the question edited, between the writer's read and the write would pass unseen, and freshness would count from the wrong point. The cost of the whole-investigation sequence is accepted: a person who concludes while an agent's steps finish gets a 409 for each outcome and reads again.
- **Writing a conclusion into a closed investigation, or one route that concludes and closes.** Either one adds an exception to the closed gate. Concluding before closing needs no new rule.
- **Renaming `statement` to `claim` and adding `reasoning`, with a data migration.** The rename breaks every 1.0 reader and writer of a finding and rewrites stored rows. The glossary already uses claim for scheduler runs, and the name `reasoning` conflicts with ADR-0059's rule against storing hidden reasoning.
- **Splitting old statements at the first sentence.** A split at `e.g.` or just before a correction changes what the first view says.
- **Typed items `{text, finding_seqs}`.** They fail the flat-input test, and their text would still carry `#N` tokens that the second list could contradict. Flattened `{text, seq_1..seq_4}` items were also rejected: the schema is awkward, and each item could cite only four findings.
- **A bounded briefing projection on every read.** It would carry the conclusion and the statuses of its cited findings on every 5-second poll, up to 1 MiB, as a second snapshot beside the entry pages. The head pointer and one targeted fetch cover the unread conclusion. The observed logs hold 11 to 32 entries, and the page reads up to 2000 entries on its own.
- **A `serde(other)` unknown-kind placeholder before the 1.0 tag.** The human chose a lockstep upgrade. A placeholder would also hide content from a reader that believes it holds the whole log.
- **An authored `method` field.** The steps are the recorded method.
- **A generic needs-review label.** It fires when a cited finding gets stronger as well as weaker, and it does not say what changed or who changed it.
- **Promoting the latest note to the bottom line.** The mockup did this with a closing note that a person chose. A live log can end with any note.

## Consequences

- A new migration re-creates `investigation_log_kind_check` with `conclusion` and adds `investigations.conclusion_seq`. Trawl is forward-only, so the PR that adds it names the Postgres and `[server] results_dir` backup and the restore.
- A 1.0 writer that sends a statement over 512 bytes gets 400. This is a write-contract change, and the 1.1 notes name it.
- A 1.0 client cannot read an investigation that holds a conclusion. The 1.1 notes name the lockstep upgrade.
- The glossary defines Conclusion, and the _Avoid_ list of Finding points to it.
- ADR-0059 is amended in three places: the finding shape (the third paragraph of its decision), the list of kinds (its first paragraph and the 2026-10-25 kind amendment), and the list of overwrites (**Edits are sequenced, and only overwrites can conflict**).
