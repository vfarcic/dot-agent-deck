/** @type {import('@docusaurus/plugin-content-docs').SidebarsConfig} */
// Grouped by client (PRD #1321): what both the TUI and the desktop app do,
// then what only one of them does. Grouping here moves no URL — a page's URL
// comes from its path under docs/, and nothing redirects a moved one.
const sidebars = {
  docs: [
    'getting-started',
    'installation',
    {
      type: 'category',
      label: 'Both Clients',
      collapsed: false,
      items: [
        'session-management',
        {
          type: 'category',
          label: 'Orchestration',
          link: { type: 'doc', id: 'orchestration' },
          items: ['idle-workers-and-notifications'],
        },
        'dispatcher-mode',
        'scheduled-tasks',
        'configuration',
        {
          type: 'category',
          label: 'Remote Environments',
          link: { type: 'doc', id: 'remote-environments' },
          items: ['remote-requirements', 'remote-recipes'],
        },
      ],
    },
    {
      type: 'category',
      label: 'Terminal UI',
      collapsed: false,
      items: ['keyboard-shortcuts'],
    },
    {
      type: 'category',
      label: 'Desktop App',
      collapsed: false,
      link: { type: 'doc', id: 'desktop/index' },
      items: [
        'desktop/dashboard',
        'desktop/new-agent',
        'desktop/daemons',
        'desktop/settings',
        'desktop/voice',
      ],
    },
    'troubleshooting',
    'license',
  ],
};

module.exports = sidebars;
