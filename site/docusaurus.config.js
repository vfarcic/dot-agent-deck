// @ts-check

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'Agent Deck',
  tagline: 'A terminal dashboard for running multiple AI coding agents in parallel',
  favicon: 'img/favicon.ico',

  url: 'https://agent-deck.devopstoolkit.ai',
  baseUrl: '/',

  organizationName: 'vfarcic',
  projectName: 'dot-agent-deck',

  onBrokenLinks: 'throw',

  markdown: {
    hooks: {
      onBrokenMarkdownLinks: 'warn',
    },
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          path: '../docs',
          routeBasePath: 'docs',
          sidebarPath: './sidebars.js',
          editUrl: 'https://github.com/vfarcic/dot-agent-deck/edit/main/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      }),
    ],
  ],

  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    ({
      navbar: {
        title: 'Agent Deck',
        logo: {
          alt: 'dot-agent-deck Logo',
          src: 'img/logo.svg',
        },
        items: [
          {
            type: 'docSidebar',
            sidebarId: 'docs',
            position: 'left',
            label: 'Docs',
          },
          {
            href: 'https://github.com/vfarcic/dot-agent-deck',
            label: 'GitHub',
            position: 'right',
          },
        ],
      },
      footer: {
        style: 'dark',
        links: [
          {
            title: 'Documentation',
            items: [
              {
                label: 'Getting Started',
                to: '/docs/getting-started',
              },
              {
                label: 'Installation',
                to: '/docs/installation',
              },
              {
                label: 'Keyboard Shortcuts',
                to: '/docs/keyboard-shortcuts',
              },
            ],
          },
          {
            title: 'Community',
            items: [
              {
                label: 'GitHub Issues',
                href: 'https://github.com/vfarcic/dot-agent-deck/issues',
              },
              {
                label: 'Releases',
                href: 'https://github.com/vfarcic/dot-agent-deck/releases',
              },
            ],
          },
        ],
        copyright: `Copyright \u00a9 ${new Date().getFullYear()} DevOps Toolkit. MIT License.`,
      },
      prism: {
        theme: require('prism-react-renderer').themes.github,
        darkTheme: require('prism-react-renderer').themes.dracula,
        additionalLanguages: ['bash', 'toml', 'rust'],
      },
      colorMode: {
        defaultMode: 'dark',
        respectPrefersColorScheme: true,
      },
    }),
};

module.exports = config;
