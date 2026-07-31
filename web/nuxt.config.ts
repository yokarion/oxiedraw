export default defineNuxtConfig({
  extends: ['docus'],

  // libadwaita-styled theme overrides (Nuxt UI design tokens + hero background).
  css: ['~/assets/css/main.css'],

  // Default to dark to match the libadwaita dark palette (toggle still available).
  colorMode: {
    preference: 'dark',
  },

  // Bind IPv4 loopback: Nuxt's default 'localhost' resolves to ::1 (IPv6) only on
  // many Linux setups, leaving Firefox (incl. Flatpak) unable to reach the dev server.
  devServer: {
    host: '127.0.0.1',
  },

  site: {
    name: 'OxieDraw',
    url: 'https://oxiedraw.yokarion.com',
  },

  // Static hosting has no image optimizer, so serve images as-is (no /_ipx paths).
  image: {
    provider: 'none',
  },

  // Bundle the icon sets we use so they render offline instead of hitting the
  // Iconify network API (icons are referenced from Markdown/.navigation.yml too).
  icon: {
    serverBundle: {
      collections: ['lucide', 'simple-icons'],
    },
  },

  // Absolute site URL used by nuxt-llms to generate llms.txt.
  llms: {
    domain: 'https://oxiedraw.yokarion.com',
  },

  // Served at the domain root via the oxiedraw.yokarion.com custom domain, so the
  // base URL is '/'. The github_pages Nitro preset is injected via NITRO_PRESET in
  // the deploy workflow.
})
