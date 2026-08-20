# Geist font assets

`fleet-ui` vendors the normal variable faces for Geist and Geist Mono from
the upstream Geist 1.8.0 release. The stylesheet advertises weights 400–700,
which preserves the UI's existing 400/500/600/700 typography contract while
shipping one WOFF2 file per family.

- Upstream: <https://github.com/vercel/geist-font>
- Release tag: `1.8.0`
- Source commit: `91158e012bdc4abd59fa066d0eae9fc11c2c9f24`
- Geist source: `fonts/Geist/webfonts/Geist[wght].woff2`
- Geist Mono source: `fonts/GeistMono/webfonts/GeistMono[wght].woff2`
- License: SIL Open Font License 1.1; see `OFL.txt`
- Copyright: Copyright 2024 The Geist Project Authors
  (<https://github.com/vercel/geist-font.git>)

The committed files are the build inputs. Builds never download fonts. Verify
their checksums with `sha256sum -c SHA256SUMS` from this directory. The two
WOFF2 files match the pinned upstream bytes exactly; `OFL.txt` preserves the
pinned upstream text with repository whitespace normalization (a final POSIX
newline and removal of one trailing space).
