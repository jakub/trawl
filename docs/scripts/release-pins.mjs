import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';

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
  code: ['value', 'meta'],
  link: ['url', 'title'],
  image: ['url', 'title'],
  definition: ['url', 'title'],
};
const VERSION_NODES = new Set(['code', 'inlineCode']);

export function releasePinsPlugin(pins) {
  function location(node, ctx, value, index) {
    const start = node.position?.start?.offset;
    if (start === undefined) return { line: 1, column: 1 };
    const nodeSource = Buffer.from(ctx.source).subarray(start, node.position.end.offset).toString();
    const fieldStart = nodeSource.indexOf(value);
    const localOffset = fieldStart >= 0 ? fieldStart + index : Math.max(0, nodeSource.indexOf('{{release.'));
    const offset = start + Buffer.byteLength(nodeSource.slice(0, localOffset));
    let source = ctx.source;
    if (ctx.fileURL) {
      try {
        const file = readFileSync(ctx.fileURL, 'utf8');
        if (file !== source) {
          const bodyStart = file.indexOf(source);
          if (bodyStart >= 0) {
            source = file;
            return positionAt(source, Buffer.byteLength(file.slice(0, bodyStart)) + offset);
          }
        }
      } catch { /* A virtual or unit-test file has no on-disk source. */ }
    }
    return positionAt(source, offset);
  }

  function positionAt(source, offset) {
    const before = Buffer.from(source).subarray(0, offset).toString();
    const line = before.split('\n').length;
    return { line, column: Array.from(before.slice(before.lastIndexOf('\n') + 1)).length + 1 };
  }

  function visit(node, ctx) {
    const fields = REWRITTEN_FIELDS[node.type] || [];
    for (const [field, value] of Object.entries(node)) {
      if (typeof value === 'string') {
        for (const match of value.matchAll(/\{\{release\./g)) {
          const tail = value.slice(match.index);
          const tag = tail.startsWith('{{release.tag}}');
          const versionPair = field === 'value' && VERSION_NODES.has(node.type)
            && tail.startsWith('{{release.version}}')
            && value.slice(0, match.index).endsWith('--version ');
          if ((tag && fields.includes(field)) || versionPair) continue;
          const file = ctx.fileURL ? fileURLToPath(ctx.fileURL) : '<markdown>';
          const { line, column } = location(node, ctx, value, match.index);
          throw new Error(`${file}:${line}:${column}: invalid release placeholder in ${node.type}.${field}`);
        }
      }
      const rendered = fields.includes(field) && typeof value === 'string'
        ? (VERSION_NODES.has(node.type)
            ? value.replace(VERSION_PAIR, (pair, leading) =>
                pins.version === null ? '' : `${leading}--version ${pins.version}`)
            : value).replace(TAG, pins.tag)
        : value;
      if (rendered !== value) ctx.setProperty(node, field, rendered);
    }
  }

  return {
    name: 'trawl-release-pins',
    options: { position: true, tag: pins.tag, version: pins.version },
    ...Object.fromEntries(NODE_TYPES.map((type) => [type, visit])),
  };
}
