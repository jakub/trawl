#!/usr/bin/env python3
"""Check built documentation links and small source-owned inventories."""
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urljoin, urlsplit
import re
import sys
import tomllib

DOCS = Path(__file__).resolve().parents[1]
ROOT = DOCS.parent
DIST = DOCS / 'dist'
errors = []


class Page(HTMLParser):
    def __init__(self, source):
        super().__init__(convert_charrefs=True)
        self.ids = set()
        self.links = []
        self.feed(source)

    def handle_starttag(self, tag, attributes):
        attrs = dict(attributes)
        if attrs.get('id'):
            self.ids.add(attrs['id'])
        if tag == 'a' and attrs.get('href'):
            self.links.append(attrs['href'])


pages = {path: Page(path.read_text()) for path in DIST.rglob('*.html')}
if not pages:
    sys.exit('No built HTML found. Run npm run build first.')
links = 0
for path, page in pages.items():
    relative = path.relative_to(DIST).as_posix()
    route = '/' + (relative[:-10] if relative.endswith('index.html') else relative)
    for href in page.links:
        target = urlsplit(urljoin('https://trawl.sh' + route, href))
        if target.scheme not in ('http', 'https') or target.netloc != 'trawl.sh':
            continue
        destination = DIST / unquote(target.path).lstrip('/')
        if destination.is_dir():
            destination /= 'index.html'
        elif not destination.exists() and not destination.suffix:
            destination /= 'index.html'
        if not destination.exists():
            errors.append(f'{relative}: missing local destination {href}')
        elif target.fragment and destination in pages:
            if unquote(target.fragment) not in pages[destination].ids:
                errors.append(f'{relative}: missing fragment {href}')
        links += 1

# These source-owned URLs are literals in both packs, including the Helm
# template. Validate every annotation without needing Helm or a YAML package
# in the docs job. A different source shape must fail rather than skip links.
rule_links = 0
rule_inventories = []
for relative in ('monitoring/prometheus/trawl.rules.yml',
                 'chart/trawl/templates/prometheusrule.yaml'):
    source = (ROOT / relative).read_text()
    starts = list(re.finditer(r'^\s*- alert:\s*(\w+)\s*$', source, re.M))
    alerts = [match[1] for match in starts]
    urls = []
    if not starts:
        errors.append(f'{relative}: alert inventory not found; update the checker for the source shape')
    for index, start in enumerate(starts):
        end = starts[index + 1].start() if index + 1 < len(starts) else len(source)
        block = source[start.end():end]
        fields = re.findall(r'^\s*runbook_url:', block, re.M)
        found = re.findall(r'^\s*runbook_url:\s*"(https://trawl\.sh/[^"\s]+)"\s*$', block, re.M)
        if len(fields) != 1 or len(found) != 1:
            errors.append(f'{relative}: {start[1]} must have one literal absolute trawl.sh runbook_url')
        urls.extend(found)
    if len(set(alerts)) != len(alerts):
        errors.append(f'{relative}: duplicate alert names')
    rule_inventories.append(list(zip(alerts, urls)))
    for href in urls:
        target = urlsplit(href)
        destination = DIST / unquote(target.path).lstrip('/')
        if destination.is_dir() or not destination.suffix:
            destination /= 'index.html'
        if destination not in pages:
            errors.append(f'{relative}: runbook page is not built: {href}')
        elif not target.fragment or unquote(target.fragment) not in pages[destination].ids:
            errors.append(f'{relative}: runbook anchor is missing: {href}')
        rule_links += 1
if rule_inventories[0] != rule_inventories[1]:
    errors.append('Plain and Helm alert/runbook inventories differ')

toml_blocks = 0
for path in (DOCS / 'src/content/docs').rglob('*'):
    if path.suffix not in ('.md', '.mdx'):
        continue
    source = path.read_text()
    frontmatter = re.match(r'^---\n(.*?)\n---', source, re.S)
    if not frontmatter or any(not re.search(rf'^{key}:\s*\S', frontmatter[1], re.M)
                              for key in ('title', 'description')):
        errors.append(f'{path.relative_to(ROOT)}: missing title or description')
    for block in re.finditer(r'^```toml[^\n]*\n(.*?)^```', source, re.M | re.S):
        try:
            tomllib.loads(block[1])
        except tomllib.TOMLDecodeError as error:
            line = source[:block.start()].count('\n') + 1
            errors.append(f'{path.relative_to(ROOT)}:{line}: invalid TOML: {error}')
        toml_blocks += 1

ast = (ROOT / 'crates/trawl-core/src/ast.rs').read_text()
display = ast.split('impl fmt::Display for PipeStage {', 1)[1].split('\n///', 1)[0]
stage_names = re.findall(r'Self::\w+\(_\) => write!\(f, "([a-z]+)"\)', display)
if not stage_names:
    errors.append('PipeStage display inventory was not found; update the checker for the source shape')
dsl = (DOCS / 'src/content/docs/reference/dsl.md').read_text()
headings = re.findall(r'^### (.*)$', dsl, re.M)
for stage in stage_names:
    if not any(re.search(rf'\b{stage}\b', heading.lower()) for heading in headings):
        errors.append(f'reference/dsl.md: no syntax heading for stage {stage}')

if errors:
    print('\n'.join(sorted(set(errors))), file=sys.stderr)
    sys.exit(1)
print(f'Checked {len(pages)} HTML pages, {links} local links, {toml_blocks} TOML blocks, '
      f'{rule_links} rule runbook links, and {len(stage_names)} DSL stages.')
