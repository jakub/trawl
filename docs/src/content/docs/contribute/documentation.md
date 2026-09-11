---
title: Maintain the documentation
description: Build the Astro site, verify examples and links, and keep facts with their owners.
---

The site uses Astro and Starlight. Its content lives under
`docs/src/content/docs/`; `docs/astro.config.mjs` defines navigation. Existing
public page URLs remain in use even when their sidebar section changes.

## Run it locally

Use the Node version required by `docs/package.json` and its locked dependencies.
The current CI uses Node 22.12 or later within the supported Node 22 line.

```bash
cd docs
npm ci
npm run dev
```

Astro prints the loopback URL. For a review with built search and the same static
HTML that will ship, build and use the local preview server:

```bash
TRAWL_DOCS_PREVIEW=1 npm run build
TRAWL_DOCS_ALLOWED_HOSTS=your-host.example.ts.net npm run preview -- --host 0.0.0.0 --port 4321
```

The preview flag adds a visible notice and `noindex` metadata. Hostnames are an
explicit comma-separated allowlist; localhost and IP addresses follow Astro's
normal handling. Binding to `0.0.0.0` makes the preview available on the machine's
IPv4 network interfaces. Stop the process with Ctrl+C when the review ends.
Local preview does not publish the site or change a Trawl deployment.

## Check a change

```bash
npm run check
```

This builds the site, then checks rendered local links and fragment targets,
TOML code-block syntax, and documented DSL stages against the source inventory.
It also confirms that each document has a title and description. These checks
catch broken navigation and mechanical drift; they do not prove a deployment
procedure or the meaning of a query.

Run copyable install and recovery commands in disposable infrastructure. Check
expected records, not only successful exit status. For a configuration fragment,
state where it belongs. For a complete example, exercise the application loader.
Test Helm examples with all documented values. Use the existing full-app runner
for browser scenarios and record which source revision and scenario you checked.

## Put each fact in one place

| Material | Owner |
| --- | --- |
| A task with commands and expected outcomes | Start, Use Trawl, or Operate Trawl |
| Syntax, fields, defaults, permissions, and error contracts | Reference |
| Current mechanisms and boundaries | Architecture |
| A design decision and its amendments | Existing repository ADR |
| A particular test result | A dated evidence record |
| Agent-specific procedure | Project skills under `.agents/skills/` |

A guide should state the reader's starting conditions, the operation, its expected
result, and how to diagnose failure. Link to detailed contracts instead of copying
them into each tutorial. Keep known limitations beside the procedure they affect.
Do not present a page's last commit date as proof that all its commands are current.

## Preserve links

Keep existing routes when reorganizing navigation. When extracting a section,
retain its old heading and replace its body with a short explanation and a link
to the new owner. This preserves old fragment URLs. The rendered-link check verifies
links in the site; reviewers also need to compare removed headings with the old page.

The Docs workflow builds and checks pull requests. Deployment runs only from main
and keeps the existing package repository and symbols directories intact. A local
review does not need a push, tag, package release, or site publication.
