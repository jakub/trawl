import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://trawl.sh',
  integrations: [
    starlight({
      title: 'trawl',
      description:
        'Self-hosted log collection, storage, and search for homelabs and small-to-medium infra.',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/jakub/trawl',
        },
      ],
      editLink: {
        baseUrl: 'https://github.com/jakub/trawl/edit/main/docs/',
      },
      lastUpdated: true,
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { slug: 'getting-started' },
            { slug: 'getting-started/first-query' },
            { slug: 'getting-started/vector-integration' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { slug: 'reference/dsl' },
            { slug: 'reference/cli' },
            { slug: 'reference/api' },
            { slug: 'reference/configuration' },
          ],
        },
        {
          label: 'Architecture',
          items: [
            { slug: 'architecture/overview' },
            { slug: 'architecture/data-flow' },
          ],
        },
        {
          label: 'About',
          items: [{ slug: 'about/roadmap' }],
        },
      ],
      favicon: '/favicon.svg',
      credits: false,
    }),
  ],
});
