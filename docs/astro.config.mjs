import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const base = '/projects/sup-xml/docs';

// Astro/Starlight does NOT auto-prefix the configured `base` onto
// root-absolute markdown links (`[Foo](/reference/foo/)`) — only
// sidebar nav and HEAD-level assets get the prefix.  Without this
// rewriter every `[link](/guides/...)` inside Markdown body content
// 404s on the deployed `/projects/sup-xml/docs/` mount.  The plugin
// walks the AST and prepends `base` to any `<a href>` whose target
// starts with `/` but isn't already mounted under `base` and isn't
// a protocol-absolute URL.  Anchors-only (`#foo`) and external
// (`https://…`, `mailto:`, etc.) pass through untouched.
function remarkPrefixBase() {
  return (tree) => {
    const visit = (node) => {
      if (node && node.type === 'link' && typeof node.url === 'string') {
        const u = node.url;
        if (u.startsWith('/') && !u.startsWith('//') && !u.startsWith(base + '/') && u !== base) {
          node.url = base + u;
        }
      }
      if (node && Array.isArray(node.children)) {
        for (const child of node.children) visit(child);
      }
    };
    visit(tree);
  };
}

export default defineConfig({
  site: 'https://supso.org',
  base,
  markdown: {
    remarkPlugins: [remarkPrefixBase],
  },
  integrations: [
    starlight({
      title: 'SupXML',
      description:
        'A memory-safe, fast, spec-compliant XML library for Rust — drop-in replacement for libxml2.',
      favicon: '/favicon.ico',
      head: [
        {
          tag: 'link',
          attrs: {
            rel: 'icon',
            href: `${base}/icon.png`,
            type: 'image/png',
          },
        },
        {
          tag: 'link',
          attrs: {
            rel: 'apple-touch-icon',
            href: `${base}/apple-touch-icon.png`,
          },
        },
        // Open Graph + Twitter cards.  Site-wide defaults; Starlight
        // injects per-page <title> / <meta name="description"> via the
        // frontmatter so we only need to set the image / type / card
        // here.  The og:image lives next to the favicon and is served
        // from the parent site root; size 1200×630 (Twitter
        // recommended) so previews on Slack / X / LinkedIn / HN
        // render correctly.
        {
          tag: 'meta',
          attrs: { property: 'og:type', content: 'website' },
        },
        {
          tag: 'meta',
          attrs: { property: 'og:site_name', content: 'SupXML' },
        },
        {
          tag: 'meta',
          attrs: {
            property: 'og:image',
            content: `https://supso.org${base}/og-image.png`,
          },
        },
        {
          tag: 'meta',
          attrs: { property: 'og:image:width',  content: '1200' },
        },
        {
          tag: 'meta',
          attrs: { property: 'og:image:height', content: '630' },
        },
        {
          tag: 'meta',
          attrs: { name: 'twitter:card', content: 'summary_large_image' },
        },
        {
          tag: 'meta',
          attrs: {
            name: 'twitter:image',
            content: `https://supso.org${base}/og-image.png`,
          },
        },
      ],
      social: {
        github: 'https://github.com/SupsoOrg/sup-xml',
      },
      sidebar: [
        {
          label: 'Basics',
          items: [
            { label: 'Welcome to SupXML', link: '/' },
            { label: 'Why SupXML', slug: 'why' },
            { label: 'Getting started', slug: 'getting-started' },
            { label: 'Licensing', slug: 'licensing' },
            {
              label: 'Migrating from libxml2',
              slug: 'guides/migrating-from-libxml2',
            },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Parsing & serialization', slug: 'guides/parsing' },
            { label: 'XPath 1.0 / 2.0', slug: 'guides/xpath' },
            { label: 'XSD validation', slug: 'guides/xsd' },
            { label: 'XSLT 1.0 / 2.0', slug: 'guides/xslt' },
            { label: 'Schematron', slug: 'guides/schematron' },
            { label: 'Canonical XML', slug: 'guides/canonical' },
            { label: 'HTML5 parsing', slug: 'guides/html' },
            { label: 'Async I/O', slug: 'guides/async' },
            { label: 'Serde deserialize', slug: 'guides/serde' },
            { label: 'Recovery mode', slug: 'guides/recovery' },
            { label: 'Character encodings', slug: 'guides/encodings' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Performance', slug: 'reference/performance' },
            { label: 'W3C conformance', slug: 'reference/conformance' },
            { label: 'Security model', slug: 'reference/security' },
            { label: 'Feature flags', slug: 'reference/features' },
            {
              label: 'Full docs.rs/sup-xml ↗',
              link: 'https://docs.rs/sup-xml',
              attrs: { target: '_blank' },
            },
          ],
        },
        {
          label: 'Contributing',
          items: [
            { label: 'Safety policy', slug: 'contributing/safety' },
            { label: 'Testing & Miri', slug: 'contributing/testing' },
          ],
        },
      ],
      customCss: ['./src/styles/custom.css'],
      components: {
        SiteTitle: './src/components/SiteTitle.astro',
      },
    }),
  ],
});
