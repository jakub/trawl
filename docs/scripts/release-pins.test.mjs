import assert from 'node:assert/strict';
import test from 'node:test';
import { satteri } from '@astrojs/markdown-satteri';
import { releasePins, releasePinsPlugin } from './release-pins.mjs';

async function render(markdown, env = {}) {
  const processor = satteri({
    features: { smartPunctuation: true },
    mdastPlugins: [releasePinsPlugin(releasePins(env))],
  });
  const renderer = await processor.createRenderer({ syntaxHighlight: false });
  const result = await renderer.render(markdown, {
    fileURL: new URL('file:///docs/example.md'),
  });
  return result.code;
}

const sample = `Download {{release.tag}} and run:

\`trawl --version {{release.version}}\`

\`\`\`sh
trawl --version {{release.version}}
\`\`\`

[chart](https://github.com/jakub/trawl/tree/{{release.tag}}/chart/trawl)
`;

test('release values render in text, inline code, fenced code, and links with smart punctuation', async () => {
  const html = await render(sample, { TRAWL_DOCS_RELEASE_TAG: 'v0.9.1' });
  assert.match(html, /Download v0\.9\.1 and run:/);
  assert.match(html, /<code>trawl --version 0\.9\.1<\/code>/);
  assert.match(html, /<pre><code[^>]*>trawl --version 0\.9\.1\n<\/code><\/pre>/);
  assert.match(html, /href="https:\/\/github\.com\/jakub\/trawl\/tree\/v0\.9\.1\/chart\/trawl"/);
  assert.doesNotMatch(html, /\{\{release\./);
});

test('development removes the version pair and its leading spaces', async () => {
  const html = await render(sample.replaceAll(' --version', '  \t--version'));
  assert.match(html, /Download main and run:/);
  assert.match(html, /<code>trawl<\/code>/);
  assert.match(html, /<pre><code[^>]*>trawl\n<\/code><\/pre>/);
  assert.match(html, /href="https:\/\/github\.com\/jakub\/trawl\/tree\/main\/chart\/trawl"/);
  assert.doesNotMatch(html, /--version|\{\{release\./);
});

test('unknown names fail with file and position', async () => {
  await assert.rejects(render('bad {{release.nope}}'), /example\.md:1:5: invalid release placeholder/);
});

test('diagnostic locates a bad token after frontmatter and a valid token', async () => {
  const markdown = '---\ntitle: Example\n---\n\n`tool --version {{release.version}}` then {{release.version}}';
  await assert.rejects(render(markdown),
    /example\.md:5:43: invalid release placeholder in text\.value/);
});

test('diagnostic counts a non-ASCII prefix on the bad token line', async () => {
  await assert.rejects(render('é then {{release.nope}}'),
    /example\.md:1:8: invalid release placeholder in text\.value/);
});

test('bare version fails in both modes', async () => {
  for (const env of [{}, { TRAWL_DOCS_RELEASE_TAG: 'v0.9.1' }]) {
    await assert.rejects(render('{{release.version}}', env), /invalid release placeholder/);
  }
});

test('version pair in prose fails with file and position in both modes', async () => {
  for (const env of [{}, { TRAWL_DOCS_RELEASE_TAG: 'v0.9.1' }]) {
    await assert.rejects(render('run --version {{release.version}}', env),
      /example\.md:1:15: invalid release placeholder in text\.value/);
  }
});

test('leftover placeholders in HTML fail', async () => {
  await assert.rejects(render('<span data-tag="{{release.tag}}">hello</span>'), /invalid release placeholder in html\.value/);
});

test('code fence metadata rewrites tags and rejects every other release pin', async () => {
  const html = await render('```sh title="{{release.tag}}"\necho ok\n```',
    { TRAWL_DOCS_RELEASE_TAG: 'v0.9.1' });
  assert.doesNotMatch(html, /\{\{release\./);
  const source = '```sh title="{{release.tag}}"\necho ok\n```';
  const node = { type: 'code', value: 'echo ok', meta: 'title="{{release.tag}}"',
    position: { start: { offset: 0 }, end: { offset: source.length } } };
  releasePinsPlugin({ tag: 'v0.9.1', version: '0.9.1' }).code(node, {
    source,
    setProperty(target, field, value) { target[field] = value; },
  });
  assert.equal(node.meta, 'title="v0.9.1"');
  for (const token of ['{{release.version}}', '{{release.nope}}']) {
    await assert.rejects(render(`\`\`\`sh title="${token}"\necho ok\n\`\`\``),
      /example\.md:1:14: invalid release placeholder in code\.meta/);
  }
});

test('invalid release tag fails', () => {
  assert.throws(() => releasePins({ TRAWL_DOCS_RELEASE_TAG: 'release-0.9.1' }), /invalid TRAWL_DOCS_RELEASE_TAG/);
});

test('development flag wins over a populated tag', async () => {
  const html = await render(sample, {
    TRAWL_DOCS_DEVELOPMENT: '1',
    TRAWL_DOCS_RELEASE_TAG: 'v0.9.1',
  });
  assert.match(html, /Download main and run:/);
  assert.doesNotMatch(html, /--version|v0\.9\.1/);
});

test('plugin options expose the selected pins to Astro config digest', () => {
  assert.deepEqual(releasePinsPlugin(releasePins({})).options,
    { position: true, tag: 'main', version: null });
  assert.deepEqual(releasePinsPlugin(releasePins({ TRAWL_DOCS_RELEASE_TAG: 'v0.9.1' })).options,
    { position: true, tag: 'v0.9.1', version: '0.9.1' });
});
