---
title: Maintain the documentation
description: Build the documentation site, check it, and keep each fact in one place.
---

The site uses Astro and Starlight. Content lives under `docs/src/content/docs/`,
and `docs/astro.config.mjs` defines the navigation.

## Run the site locally

```bash
cd docs
npm ci
npm run dev
```

Astro prints the loopback URL. Open it and confirm your page renders.

To review the built static HTML with working search, build with the preview flag
and serve the result:

```bash
TRAWL_DOCS_PREVIEW=1 npm run build
TRAWL_DOCS_ALLOWED_HOSTS=docs.example.com npm run preview -- --host 0.0.0.0 --port 4321
```

`TRAWL_DOCS_PREVIEW=1` adds a visible banner and `noindex, nofollow` metadata.
`TRAWL_DOCS_ALLOWED_HOSTS` is a comma-separated allowlist of hostnames. Binding
to `0.0.0.0` publishes the preview on every IPv4 interface of the machine. Stop
the process with Ctrl+C when the review ends. A local preview does not publish
the site and does not touch any Trawl deployment.

## Check a change

```bash
cd docs
npm run check
```

The check builds the site, then runs `docs/scripts/check-docs.py`. That script
verifies local links and fragment targets in the rendered HTML, parses every
TOML code block, confirms each page has a `title` and a `description`, and
requires a `### ` heading in `reference/dsl.md` for every `PipeStage` name in
`crates/trawl-core/src/ast.rs`. It prints the counts it checked.

These checks catch broken navigation and mechanical drift. They do not prove a
procedure works. Run copyable install and recovery commands in disposable
infrastructure, and check the resulting records rather than the exit status. For
a configuration fragment, say where it belongs. For a complete example, load it
with the application. Test Helm examples with every documented value. Use the
[full-app experiment](/contribute/experiments/) for browser scenarios, and note
which revision and scenario you checked.

The Docs workflow builds and checks every pull request that touches `docs/` or
`crates/trawl-core/src/ast.rs`. It deploys from `main` only.

## Documentation and release scope

The published manual follows `main` and can describe changes that are absent
from downloaded releases. The Docs workflow sets
`TRAWL_DOCS_DEVELOPMENT=1` to label its build with the development-manual banner.
Other builds do not enable that banner by default. A local preview uses its own
unpublished-preview banner.

For a stable release, align the manual, binaries, image, chart, and examples to
one verified release commit. A package version alone does not establish that
alignment. Keep development documentation separately identified when the stable
manual is introduced.

Keep unpublished launch notes and pending support decisions under `docs/launch/`,
outside the site's content collection. The repository's `CHANGELOG.md` links to
the draft initial-release notes and the preserved pre-1.0 journal. The historical
journal records intermediate changes and does not define current installation
or upgrade requirements.

## Put each fact in one place

| Material | Owner |
| --- | --- |
| A task with commands and expected results | Start, Use Trawl, or Operate Trawl |
| Syntax, fields, defaults, permissions, error contracts | Reference |
| Mechanisms and boundaries | Architecture |
| An agent-specific procedure | Project skills under `.agents/skills/` |

A guide states the reader's starting conditions, the steps, the expected result,
and how to diagnose a failure. Link to the contract instead of copying it into
each tutorial. Keep a known limitation beside the procedure it affects.

Give each page one task or one topic. When you move content, update the
navigation and every link to it. Remove empty headings and pages that only point
somewhere else. Run `npm run check` after you change a route or a heading.
