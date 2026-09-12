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
      f'and {len(stage_names)} DSL stages.')
