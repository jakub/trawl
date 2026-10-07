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
