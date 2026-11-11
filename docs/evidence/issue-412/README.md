`retained-events.tsv` is the inventory that issue #412's third acceptance
criterion asks for. It has one row for every INFO, WARN and ERROR emission in
non-test `crates/trawl-server/src` code, and one row for every call of a
logging helper that takes an error value from its caller. Each row has a
disposition: `changed`, `unchanged: <reason>` or `D2b`.

- `enumerate.py` finds the rows. It is the list. The `rg` search in the TSV
  header is only a help: it misses shorthand fields, plain string fields and
  dynamic messages.
- `dispositions.tsv` holds the two columns a person judged, source type and
  disposition. A key is file, event_type, level, kind and the ordinal of that
  combination in the file, so a line shift keeps every key.
- `retained-events.tsv` is the join. Its header names the commit the rows
  describe.

Run each command from the repository root:

```sh
python3 -I docs/evidence/issue-412/enumerate.py           # rewrite the TSV
python3 -I docs/evidence/issue-412/enumerate.py --check   # exit 1 if it is stale
python3 -I docs/evidence/issue-412/enumerate.py --keys    # each row's key and macro body
python3 -I docs/evidence/issue-412/enumerate.py --hits    # search hits outside every row and DEBUG event
python3 -I docs/evidence/issue-412/enumerate.py --helpers # candidate logging helpers
```

`--hits` prints each line that matches the search but sits outside every row
and every DEBUG event. Those lines build a `String` or an error from an error.
They are sources, not log events, so the list is not empty.
`string-builders.tsv` records where each one's text ends up, as read at
`cb5ef2a2`.

`--check` compares every line except the `# sha:` line. The script exits
non-zero and names the row when a site has no disposition, when a disposition
has no site, or when a disposition is blank or not one of the three forms.
After a fix commit moves lines, run the script again. If a commit adds or
removes an emission, add or remove its key in `dispositions.tsv` first.

The dispositions follow these rules:

- `changed`: a commit of this pull request changed the emission. It now
  logs `error_class` and `cause_kind`, a fixed sentence or
  `join_failure_text`, and puts the error text in a DEBUG event.
- `D2b`: the error text comes only from `std::io::Error`, directly or through
  a `String` built from one with trawl's own words and paths. Issue #413
  handles these rows.
- `unchanged: <reason>`: the event carries no foreign error text, or a
  decision keeps it. The reason says which.

The script has these limits. Its docstring gives the details.

- A production build is a build with `test` and every Cargo feature off.
  Items that a `#[cfg]` removes under those settings are masked, including
  whole files behind a `#[cfg(test)] mod x;`. Other cfg atoms keep their code.
- The default target is the module path, inline `mod name { }` blocks
  included. An explicit `target:` must be a string literal or a `&str`
  constant in the crate.
- The script stops on any `event!` or `log::` macro, because it does not
  read them. None exist at this commit.
- `HELPERS` in the script names each helper. The script does not detect a new
  helper by itself. Use `--helpers` to find candidates. A helper reached
  through a function pointer is not seen.
