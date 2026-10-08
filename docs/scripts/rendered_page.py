"""Parse a built documentation page: its ids, its links and its rendered Markdown region."""
from html.parser import HTMLParser

# Void elements never get an end tag, so they must not move the nesting depth.
VOID = {'area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta',
        'source', 'track', 'wbr'}
# Elements that show something without any text inside them.
CONTENT = {'audio', 'canvas', 'embed', 'iframe', 'img', 'math', 'object', 'picture',
           'svg', 'video'}
# Elements whose contents a reader never sees.
HIDDEN = {'script', 'style', 'template'}


class Page(HTMLParser):
    def __init__(self, source):
        super().__init__(convert_charrefs=True)
        self.ids = set()
        self.links = []
        self.markdown_depth = 0
        self.markdown_has_body = False
        self.hidden_depth = 0
        self.empty_markdown = False
        self.feed(source)

    def handle_starttag(self, tag, attributes):
        attrs = dict(attributes)
        if self.markdown_depth:
            # A rendering error leaves the region empty, or holding only empty
            # elements, so only text or a content element counts as a body.
            if tag in HIDDEN:
                self.hidden_depth += 1
            elif tag in CONTENT and not self.hidden_depth:
                self.markdown_has_body = True
            if tag not in VOID:
                self.markdown_depth += 1
        elif 'sl-markdown-content' in attrs.get('class', '').split():
            self.markdown_depth = 1
            self.markdown_has_body = False
        if attrs.get('id'):
            self.ids.add(attrs['id'])
        if tag == 'a' and attrs.get('href'):
            self.links.append(attrs['href'])

    def handle_data(self, data):
        if self.markdown_depth and not self.hidden_depth and data.strip():
            self.markdown_has_body = True

    def handle_endtag(self, tag):
        if self.markdown_depth and tag not in VOID:
            if tag in HIDDEN and self.hidden_depth:
                self.hidden_depth -= 1
            self.markdown_depth -= 1
            if not self.markdown_depth and not self.markdown_has_body:
                self.empty_markdown = True
