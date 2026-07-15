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
