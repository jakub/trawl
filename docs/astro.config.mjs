import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const preview = process.env.TRAWL_DOCS_PREVIEW === '1';
const allowedHosts = (process.env.TRAWL_DOCS_ALLOWED_HOSTS || '')
  .split(',').map((host) => host.trim()).filter(Boolean);

export default defineConfig({
  site: 'https://trawl.sh',
  server: { allowedHosts },
  devToolbar: { enabled: false },
  integrations: [
    starlight({
      title: 'trawl',
      description: 'Collect, investigate, and operate your logs with Trawl.',
      logo: { src: './src/assets/trawl.png', alt: '' },
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/jakub/trawl' }],
      editLink: { baseUrl: 'https://github.com/jakub/trawl/edit/main/docs/' },
      lastUpdated: false,
      customCss: ['./src/styles/docs.css'],
      components: { Banner: './src/components/PreviewBanner.astro' },
      head: preview ? [{ tag: 'meta', attrs: { name: 'robots', content: 'noindex, nofollow' } }] : [],
      sidebar: [
        {
          label: 'Start here',
          items: ['start/overview', 'start/connect', 'start/local-parquet', 'getting-started', 'getting-started/first-query'].map((slug) => ({ slug })),
        },
        {
          label: 'Use Trawl',
          items: ['reference/web-ui', 'use/query-tutorial', 'use/cli-tui', 'use/live-tail', 'use/saved-reports', 'use/sharing-export'].map((slug) => ({ slug })),
        },
        {
          label: 'Operate Trawl', collapsed: true,
          items: ['operate/deployment', 'operate/ingestion', 'getting-started/vector-integration', 'operate/access', 'operate/health', 'operate/catalog', 'operate/retention', 'operate/backup-restore', 'reference/crash-dumps'].map((slug) => ({ slug })),
        },
        {
          label: 'Reference', collapsed: true,
          items: ['dsl', 'cli', 'api', 'configuration', 'events'].map((name) => ({ slug: `reference/${name}` })),
        },
        {
          label: 'Architecture', collapsed: true,
          items: ['overview', 'data-flow', 'catalog', 'query-execution', 'recovery', 'reports-telemetry'].map((name) => ({ slug: `architecture/${name}` })),
        },
        {
          label: 'Contribute', collapsed: true,
          items: ['getting-started/development', 'contribute/source-map', 'contribute/testing', 'contribute/experiments', 'contribute/documentation'].map((slug) => ({ slug })),
        },
      ],
      favicon: '/favicon.svg',
      credits: false,
    }),
  ],
});
