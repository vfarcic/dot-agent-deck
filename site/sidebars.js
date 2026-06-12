/** @type {import('@docusaurus/plugin-content-docs').SidebarsConfig} */
const sidebars = {
  docs: [
    'getting-started',
    'installation',
    'session-management',
    'keyboard-shortcuts',
    'orchestration',
    'workspace-modes',
    'scheduled-tasks',
    'configuration',
    'experimental-flag',
    {
      type: 'category',
      label: 'Remote Environments',
      link: { type: 'doc', id: 'remote-environments' },
      items: ['remote-requirements', 'remote-recipes'],
    },
    'troubleshooting',
    'license',
  ],
};

module.exports = sidebars;
