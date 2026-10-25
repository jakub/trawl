// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The DSL reference as MCP resources, and the essentials block `orient`
//! prints (ADR-0058).
//!
//! The reference is the published page, `reference/dsl.md`, compiled in.
//! It is served in sections so no resource read exceeds the response hard
//! cap: one resource per `## ` section, and when a section is larger than
//! [`SECTION_CAP`], one for its text before the first `### ` heading and
//! one per `### ` section. The text before the first `## ` heading, without
//! the page's front matter, is the `overview`. Headings inside fenced code
//! blocks do not split. URIs are `trawl://dsl/<slug>`, or
//! `trawl://dsl/<slug>/<sub-slug>` for a split section, in page order.

use std::sync::LazyLock;

/// The published DSL reference page.
pub const REFERENCE: &str = include_str!("../../../docs/src/content/docs/reference/dsl.md");

/// The most bytes of markdown one resource holds: the hard response cap.
pub const SECTION_CAP: usize = 32 * 1024;

/// Every DSL resource URI starts with this.
pub const URI_PREFIX: &str = "trawl://dsl/";

/// The MIME type of every DSL resource.
pub const MIME_TYPE: &str = "text/markdown";

/// The DSL in a few lines, for a client that reads no resources. Every
/// backticked span and every indented example line is a whole query that
/// parses and validates; a test holds them to it.
pub const ESSENTIALS: &str = "\
DSL essentials (the full reference is in the trawl://dsl/ resources):
- A query is a search stage, then pipe stages, each after `|`.
- The search stage ANDs its terms: `service=nginx status>=500` keeps events matching both. \
Use OR, NOT and parentheses to combine: `(service=nginx OR service=apache) NOT status=200`.
- Operators: = != > >= < <=, a comma list for membership `status=401,403`, a glob \
`path=/api/*`, a regex `message=/timeout|refused/`. Quote values with spaces: `host=\"db host\"`.
- A bare word or quoted phrase searches message and _raw: `\"connection refused\"`.
- Time bounds go in the search stage: `last=15m` (units s, m, h, d, w), or \
`earliest=\"2026-03-14T03:00:00Z\" latest=\"2026-03-14T04:00:00Z\"`, half-open. \
Never mix last= with earliest= or latest=.
- Examples:
  _severity>=error last=1h | stats count() by service | sort -count
  service=nginx status>=500 last=24h | stats count() by host, status
  \"connection refused\" last=2h | sort -_time | head 20
  last=6h | timechart span=5m count() by service
  status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10
";

/// One DSL reference resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// `trawl://dsl/<slug>` or `trawl://dsl/<slug>/<sub-slug>`.
    pub uri: String,
    /// The slug path after [`URI_PREFIX`], used as the resource name.
    pub name: String,
    /// The heading, or `<heading>: <sub-heading>` for a split section.
    pub title: String,
    /// The section's markdown, its heading line included.
    pub text: String,
}

/// The sections of [`REFERENCE`], in page order.
#[must_use]
pub fn sections() -> &'static [Section] {
    static SECTIONS: LazyLock<Vec<Section>> = LazyLock::new(|| split(REFERENCE, SECTION_CAP));
    &SECTIONS
}

/// The section a URI names.
#[must_use]
pub fn section(uri: &str) -> Option<&'static Section> {
    sections().iter().find(|section| section.uri == uri)
}

/// Split a markdown page into sections as the module doc describes.
fn split(page: &str, cap: usize) -> Vec<Section> {
    let (title, body) = strip_front_matter(page);
    let chunks = chunks_at(body, "## ");
    let mut sections = Vec::new();
    for (heading, text) in chunks {
        let Some(heading) = heading else {
            if !text.trim().is_empty() {
                sections.push(section_for(
                    &["overview"],
                    title.unwrap_or("Overview"),
                    text,
                ));
            }
            continue;
        };
        let slug = slugify(heading);
        if text.len() <= cap {
            sections.push(section_for(&[&slug], heading, text));
            continue;
        }
        for (sub, sub_text) in chunks_at(text, "### ") {
            match sub {
                None => sections.push(section_for(&[&slug], heading, sub_text)),
                Some(sub) => sections.push(section_for(
                    &[&slug, &slugify(sub)],
                    &format!("{heading}: {sub}"),
                    sub_text,
                )),
            }
        }
    }
    sections
}

fn section_for(slugs: &[&str], title: &str, text: &str) -> Section {
    let name = slugs.join("/");
    Section {
        uri: format!("{URI_PREFIX}{name}"),
        name,
        title: title.to_owned(),
        text: text.trim_end().to_owned() + "\n",
    }
}

/// The page's `title:` and the body after its `---` front matter.
fn strip_front_matter(page: &str) -> (Option<&str>, &str) {
    let Some(rest) = page.strip_prefix("---\n") else {
        return (None, page);
    };
    let Some(end) = rest.find("\n---\n") else {
        return (None, page);
    };
    let title = rest[..end]
        .lines()
        .find_map(|line| line.strip_prefix("title:"))
        .map(str::trim);
    (title, &rest[end + "\n---\n".len()..])
}

/// Cut `text` before every line that starts with `marker` outside a fenced
/// code block. The first chunk has no heading; each later one carries its
/// heading text and starts at its heading line.
fn chunks_at<'a>(text: &'a str, marker: &str) -> Vec<(Option<&'a str>, &'a str)> {
    let mut chunks = Vec::new();
    let mut heading = None;
    let mut start = 0;
    let mut in_fence = false;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && let Some(title) = line.strip_prefix(marker) {
            chunks.push((heading, &text[start..offset]));
            heading = Some(title.trim());
            start = offset;
        }
        offset += line.len();
    }
    chunks.push((heading, &text[start..]));
    chunks
}

/// A heading as a URI segment: ASCII alphanumerics lowercased, every other
/// run of characters one `-`, none at either end.
fn slugify(heading: &str) -> String {
    let mut slug = String::new();
    for c in heading.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_section_is_under_the_cap_with_a_unique_uri() {
        let sections = sections();
        assert!(sections.len() > 5, "the page splits into sections");
        let mut uris = BTreeSet::new();
        for section in sections {
            assert!(
                section.text.len() <= SECTION_CAP,
                "{} is {} bytes",
                section.uri,
                section.text.len()
            );
            assert!(section.uri.starts_with(URI_PREFIX));
            assert!(
                section.name.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'-' | b'/')),
                "{}",
                section.uri
            );
            assert!(uris.insert(section.uri.clone()), "{} repeats", section.uri);
        }
    }

    #[test]
    fn the_sections_cover_the_page_body_in_order() {
        let (_, body) = strip_front_matter(REFERENCE);
        let joined: String = sections().iter().map(|s| s.text.trim_end()).collect();
        let expected: String = body.split_whitespace().collect();
        let got: String = joined.split_whitespace().collect();
        assert_eq!(got, expected, "no text is lost, duplicated or reordered");
        assert_eq!(sections()[0].uri, "trawl://dsl/overview");
        assert_eq!(sections()[0].title, "Query Language (DSL)");
        assert!(section("trawl://dsl/search-stage").is_some());
        assert!(section("trawl://dsl/examples").is_some());
        assert!(section("trawl://dsl/nope").is_none());
    }

    #[test]
    fn an_oversized_section_splits_at_its_subheadings_but_not_inside_fences() {
        let page = "---\ntitle: T\n---\nintro\n## Big\nlead\n```\n### not a heading\n```\n\
                    ### One\naaaa\n### Two `x`\nbbbb\n## Small\nc\n";
        let sections = split(page, 40);
        let uris: Vec<_> = sections.iter().map(|s| s.uri.as_str()).collect();
        assert_eq!(
            uris,
            [
                "trawl://dsl/overview",
                "trawl://dsl/big",
                "trawl://dsl/big/one",
                "trawl://dsl/big/two-x",
                "trawl://dsl/small",
            ]
        );
        assert!(sections[1].text.contains("### not a heading"));
        assert_eq!(sections[3].title, "Big: Two `x`");
    }

    #[test]
    fn slugs_fold_punctuation_to_single_hyphens() {
        assert_eq!(
            slugify("`timestamp` and `@timestamp`"),
            "timestamp-and-timestamp"
        );
        assert_eq!(slugify("limit / head"), "limit-head");
        assert_eq!(slugify("Severity: `_severity`"), "severity-severity");
    }

    /// The whole queries in [`ESSENTIALS`]: its indented example lines and
    /// its single-backticked spans.
    fn essential_queries() -> Vec<&'static str> {
        let mut queries: Vec<&str> = ESSENTIALS
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .collect();
        for line in ESSENTIALS.lines().filter(|line| !line.starts_with("  ")) {
            queries.extend(line.split('`').skip(1).step_by(2));
        }
        queries
    }

    #[test]
    fn every_essentials_query_parses_and_validates() {
        let queries = essential_queries();
        assert!(queries.len() >= 12, "{queries:?}");
        for query in queries {
            if query == "|" {
                continue;
            }
            let ast = trawl_core::parser::parse(query)
                .unwrap_or_else(|errors| panic!("{query:?} does not parse: {errors:?}"));
            trawl_core::emitter::validate_pipeline(&ast.pipeline)
                .unwrap_or_else(|error| panic!("{query:?} does not validate: {error:?}"));
        }
    }

    #[test]
    fn the_essentials_block_is_short() {
        assert!(
            ESSENTIALS.lines().count() <= 16,
            "{}",
            ESSENTIALS.lines().count()
        );
        assert!(ESSENTIALS.len() < 2048, "{}", ESSENTIALS.len());
    }
}
