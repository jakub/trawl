# Issue 196 consumer validation

The current Coastwatch consumer compile failed against both the candidate and
the pre-issue baseline. Both commands exited 101 with the same three shell API
errors. Acceptance criterion 7 remains unmet. Proceeding without a passing
consumer compile requires an explicit user exception; that decision is pending.

## Source identities

| Input | Revision |
| --- | --- |
| Coastwatch current HEAD tested | `efd3a4d292c5fb5bbbb8168cee1e036e3addb8ce` |
| Candidate Trawl containing Fleet | `ce9dc49e6ecd78d45e3ebd4805f0930da49ade8d` |
| Trawl baseline control | `16a2916ed280109289aa8d9999a8da320f505ebd` |
| Coastwatch's unchanged CI `TRAWL_REV` | `cdd12a5c857e4632cb5719c7f97cc949881558e5` |

The root validator used disposable sibling `coastwatch/` and `trawl/`
directories. No Coastwatch source migration, CI-pin change, or Fleet
compatibility shim is part of this issue. The candidate check compiled Fleet
and reached `coastwatch-web-ui`; it reported these three errors in
`crates/web-ui/src/components/shell.rs`:

```text
error[E0432]: unresolved import `fleet_ui::AppLink`
  --> crates/web-ui/src/components/shell.rs:11:16
error[E0425]: cannot find type `ModeTab` in crate `fleet_ui`
   --> crates/web-ui/src/components/shell.rs:162:37
error[E0599]: no method named `rail_items` found for struct `fleet_ui::shell::ShellPropsBuilder<TypedBuilderFields>` in the current scope
   --> crates/web-ui/src/components/shell.rs:215:25
error: could not compile `coastwatch-web-ui` (bin "coastwatch-web-ui") due to 3 previous errors
```

The baseline control reported the same error codes, messages, and source
locations. These shell incompatibilities therefore predate this issue. This
comparison does not establish that the whole consumer or its theme readers
compile against the candidate. No consumer compatibility pass is claimed.

The complete compiler outputs are retained in
[the candidate log](coastwatch-candidate.log) and
[the baseline log](coastwatch-baseline.log). Only local directory prefixes were
replaced with `<DISPOSABLE_ROOT>`, `<CARGO_TARGET_DIR>`, and `<CARGO_HOME>`.
The logs contain compiler progress, warnings, and diagnostics; no credentials
or network endpoints were found during the sanitization check.

## Executed commands and lockfile resolution

The root validator executed the following commands. Paths below use named
placeholders for the corresponding local directories; flags and environment
settings match the runs. Each `DISPOSABLE_ROOT` contained sibling `coastwatch/`
and `trawl/` archives at the revisions above. Each target directory was separate.

Candidate, from `<CANDIDATE_DISPOSABLE_ROOT>/coastwatch`:

```bash
env -u NO_COLOR CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="<CANDIDATE_TARGET_DIR>" \
  cargo check -p coastwatch-web-ui --target wasm32-unknown-unknown
```

Baseline, from `<BASELINE_DISPOSABLE_ROOT>/coastwatch`:

```bash
env -u NO_COLOR CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="<BASELINE_TARGET_DIR>" \
  cargo check -p coastwatch-web-ui --target wasm32-unknown-unknown
```

Both returned exit code **101**. Neither used `--locked`. Cargo added `"serde"`
to the `fleet-ui` dependency list in the candidate disposable `Cargo.lock`.
That is the only lockfile difference between the candidate copy and baseline
copy; no package or version was added. The baseline lockfile also matches the
original Coastwatch working-copy lockfile byte for byte. This comparison used
the on-disk original lockfile, not a separate extraction of its Git HEAD blob.
The original Coastwatch source, lockfile, and CI pin were not changed.

## Reproduce in disposable sibling copies

The following is a reproduction procedure, not a transcript of commands
already executed. It requires local repositories containing the named objects,
the repository Rust toolchain, and its Wasm target. Run from a shell where
`TRAWL_SOURCE` and `COASTWATCH_SOURCE` identify the original repositories.

```bash
consumer_check_dir=$(mktemp -d /tmp/theme-consumer.XXXXXX)
mkdir "$consumer_check_dir/trawl" "$consumer_check_dir/coastwatch"
git -C "$TRAWL_SOURCE" archive ce9dc49e6ecd78d45e3ebd4805f0930da49ade8d |
  tar -x -C "$consumer_check_dir/trawl"
git -C "$COASTWATCH_SOURCE" archive efd3a4d292c5fb5bbbb8168cee1e036e3addb8ce |
  tar -x -C "$consumer_check_dir/coastwatch"
cd "$consumer_check_dir/coastwatch"
env -u NO_COLOR CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$consumer_check_dir/target" \
  cargo check -p coastwatch-web-ui --target wasm32-unknown-unknown
```

For the control, repeat in a new temporary directory with Trawl revision
`16a2916ed280109289aa8d9999a8da320f505ebd` and the same Coastwatch revision,
toolchain, target, command, and relevant environment. Preserve both exit codes
and diagnostics. Do not alter either consumer source to make the comparison
pass. Record any lockfile or environment adjustment explicitly.

## First-paint boundary

Coastwatch's unchanged HTML starts with `data-theme="light"` and does not load
the shared bootstrap. This issue makes no Coastwatch first-paint claim.
Adopting `theme-bootstrap.js` with `data-storage-key="coastwatch.ui"` before
all styles and Wasm is a separate Coastwatch change. Preserve its existing
same-origin script policy and per-response nonce handling for the Wasm loader.
See the [development guide](../../src/content/docs/getting-started/development.md#fleet-theme-preferences-and-consumer-adoption)
for the adoption contract.
