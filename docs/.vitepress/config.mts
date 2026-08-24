import { defineConfig, type Plugin } from 'vitepress'

const TAILSCALE_IP = process.env.TAILSCALE_IP || '100.94.40.126'

function tailscaleNetwork(): Plugin {
  return {
    name: 'tailscale-network',
    configureServer(server) {
      const print = server.printUrls
      server.printUrls = () => {
        if (server.resolvedUrls) {
          const rewritten = server.resolvedUrls.network.map(
            (url: string) => url.replace(/\/\/[^:]+:/, `//${TAILSCALE_IP}:`)
          )
          server.resolvedUrls.network = Array.from(new Set(rewritten))
        }
        print()
      }
    },
  }
}

export default defineConfig({
  title: 'etv-station',
  description: 'Playout-JSON generator daemon for ErsatzTV-next',
  cleanUrls: true,
  themeConfig: {
    nav: [
      { text: 'PRD', link: '/PRD' },
      { text: 'Roadmap', link: '/roadmap' },
      { text: 'Architecture', link: '/architecture' },
      { text: 'Schema', link: '/schema' },
      { text: 'File map', link: '/file-map' },
    ],
    sidebar: [
      {
        text: 'Reference',
        items: [
          { text: 'Product spec (PRD)', link: '/PRD' },
          { text: 'Roadmap', link: '/roadmap' },
          { text: 'Architecture', link: '/architecture' },
          { text: 'Config schema', link: '/schema' },
          { text: 'File map', link: '/file-map' },
        ],
      },
      {
        text: 'Decisions (ADRs)',
        collapsed: false,
        items: [
          { text: 'About these ADRs', link: '/adr/' },
          { text: '0001 — Reload reverts to last-known-good', link: '/adr/0001-reload-generation-revert' },
          { text: '0002 — A scorer plugin replaces a pool\'s expr', link: '/adr/0002-scorer-plugin-replaces-a-pool-expr' },
          { text: '0003 — One file per chunk, honest names', link: '/adr/0003-one-file-per-chunk-honest-names' },
          { text: '0004 — Calendar and clock sit at different seams', link: '/adr/0004-calendar-and-clock-sit-at-different-seams' },
          { text: '0005 — A plugin gets the channel seed', link: '/adr/0005-a-plugin-gets-the-channel-seed' },
          { text: '0006 — A catalog entry is marked missing, never deleted', link: '/adr/0006-catalog-entries-are-soft-deleted' },
          { text: '0007 — The overlay cascade hot-reloads, never respawns', link: '/adr/0007-overlay-cascade-hot-reloads-never-respawns' },
          { text: '0009 — A Plex rating key\'s entry_id is pinned for life', link: '/adr/0009-a-plex-rating-keys-entry-id-is-pinned' },
          { text: '0010 — A published slot moves only for an author', link: '/adr/0010-a-published-slot-moves-only-for-an-author' },
        ],
      },
    ],
    search: { provider: 'local' },
    socialLinks: [
      { icon: 'github', link: 'https://github.com/McBrideMusings/etv-station' },
    ],
    editLink: {
      pattern: 'https://github.com/McBrideMusings/etv-station/edit/main/docs/:path',
      text: 'Edit this page on GitHub',
    },
  },
  vite: {
    plugins: [tailscaleNetwork()],
    server: { host: '0.0.0.0', port: 5193, allowedHosts: true },
  },
})
