# Fleet font assets

`fleet-ui` vendors the upright variable faces for **Albert Sans** (UI text)
and **Chivo Mono** (data and code), pinned to the `google/fonts` repository,
which is where both families publish. The stylesheet advertises weights
400–700, which preserves the UI's existing 400/500/600/700 typography
contract while shipping one file per family.

## Albert Sans

- Upstream: <https://github.com/google/fonts>
- Source commit: `6b612533e5b14370fea6f524095f4d01cdfee18b`
  ("Albert Sans: Version 1.025 added (#4756)", 2022-06-16)
- Source path: `ofl/albertsans/AlbertSans[wght].ttf`
- Committed as: `AlbertSans-Variable.ttf` (renamed for URL safety, bytes
  unchanged), version 1.025, `wght` axis 100–900
- Designer: Andreas Rasmussen; project <https://github.com/usted/Albert-Sans>
- License: SIL Open Font License 1.1; see `OFL-AlbertSans.txt`
- Copyright: Copyright 2021 The Albert Sans Project Authors
  (<https://github.com/usted/Albert-Sans>)

## Chivo Mono

- Upstream: <https://github.com/google/fonts>
- Source commit: `5b62bc464227fcadef4d4acebd73153598d3e05e`
  ("[gftools-packager] Chivo Mono: Version 1.008 added (#5539)", 2022-11-16)
- Source path: `ofl/chivomono/ChivoMono[wght].ttf`
- Committed as: `ChivoMono-Variable.ttf` (renamed for URL safety, bytes
  unchanged), version 1.008, `wght` axis 100–900, default instance 500
- Designer: Omnibus-Type (Héctor Gatti); project
  <https://github.com/Omnibus-Type/Chivo>
- License: SIL Open Font License 1.1; see `OFL-ChivoMono.txt`
- Copyright: Copyright 2019 The Chivo Project Authors
  (<https://github.com/Omnibus-Type/Chivo>)

Neither family declares a Reserved Font Name.

## Why TrueType and not WOFF2

Neither project publishes a variable WOFF2: Chivo ships static per-weight
WOFF2 files only, and Albert Sans ships no webfont at all. Re-cutting a
variable WOFF2 locally would break the rule below, so the distributions ship
the canonical `.ttf` files. `trawl-web` serves the SPA distribution with
precompressed `.br`/`.gz` sidecars produced by `cargo xtask compress-web`,
whose `COMPRESSIBLE_EXTENSIONS` list (`xtask/src/main.rs`) includes `ttf`,
so a release build ships each face at roughly half its raw size (Brotli a
little under that). The faces carry `no-cache` with an ETag because they
are not content-hashed filenames. Both are inside the wire-size budget;
measure with `cargo xtask compress-web`, which prints the raw, gzip and
Brotli totals against the 4 MiB / 2.5 MiB budgets.

## The rule

The committed files are the build inputs. Builds never download fonts. The
two `.ttf` files match the pinned upstream bytes exactly — never re-cut,
instance, subset or convert them. Both OFL texts are the pinned upstream
files, byte for byte. Verify everything with `sha256sum -c SHA256SUMS` from
this directory; regenerate with `sha256sum *.ttf *.txt > SHA256SUMS`.
