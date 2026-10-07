import { fileURLToPath } from 'node:url';

const TAG_PATTERN = /^v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/;
const VERSION_PAIR = /([ \t]*)--version \{\{release\.version\}\}/g;
const TAG = /\{\{release\.tag\}\}/g;

export function releasePins(env) {
  const tag = env.TRAWL_DOCS_RELEASE_TAG || '';
  if (env.TRAWL_DOCS_DEVELOPMENT === '1' || !tag) {
    return { tag: 'main', version: null };
  }
  if (!TAG_PATTERN.test(tag)) {
    throw new Error(`invalid TRAWL_DOCS_RELEASE_TAG: ${tag}`);
  }
  return { tag, version: tag.slice(1) };
}

const NODE_TYPES = [
  'paragraph', 'heading', 'thematicBreak', 'blockquote', 'list', 'listItem',
  'html', 'code', 'definition', 'text', 'emphasis', 'strong', 'inlineCode',
  'break', 'link', 'image', 'linkReference', 'imageReference',
  'footnoteDefinition', 'footnoteReference', 'table', 'tableRow', 'tableCell',
  'delete', 'yaml', 'toml', 'math', 'inlineMath', 'containerDirective',
  'leafDirective', 'textDirective', 'superscript', 'subscript',
  'mdxJsxFlowElement', 'mdxJsxTextElement', 'mdxFlowExpression',
  'mdxTextExpression', 'mdxjsEsm',
];

const REWRITTEN_FIELDS = {
  text: ['value'],
  inlineCode: ['value'],
  code: ['value'],
  link: ['url', 'title'],
  image: ['url', 'title'],
  definition: ['url', 'title'],
};
const VERSION_NODES = new Set(['code', 'inlineCode']);

export function releasePinsPlugin(pins) {
  function visit(node, ctx) {
    const fields = REWRITTEN_FIELDS[node.type] || [];
    for (const [field, value] of Object.entries(node)) {
      const rendered = fields.includes(field) && typeof value === 'string'
        ? (VERSION_NODES.has(node.type)
            ? value.replace(VERSION_PAIR, (pair, leading) =>
                pins.version === null ? '' : `${leading}--version ${pins.version}`)
            : value).replace(TAG, pins.tag)
        : value;
      if (rendered !== value) ctx.setProperty(node, field, rendered);
      if (typeof rendered !== 'string' || !rendered.includes('{{release.')) continue;
      const file = ctx.fileURL ? fileURLToPath(ctx.fileURL) : '<markdown>';
      const token = rendered.slice(rendered.indexOf('{{release.')).match(/^\{\{release\.[^\s}]*\}\}?/)?.[0] || '{{release.';
      const offset = ctx.source.indexOf(token);
      const before = offset >= 0 ? ctx.source.slice(0, offset) : '';
      const line = before.split('\n').length;
      const column = before.length - before.lastIndexOf('\n');
      throw new Error(`${file}:${line}:${column}: invalid release placeholder in ${node.type}.${field}`);
    }
  }

  return {
    name: 'trawl-release-pins',
    ...Object.fromEntries(NODE_TYPES.map((type) => [type, visit])),
  };
}
