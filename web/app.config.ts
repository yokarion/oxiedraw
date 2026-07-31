export default defineAppConfig({
  seo: {
    title: 'OxieDraw',
    titleTemplate: '%s - OxieDraw',
    description:
      'A fast, clean drawing app for Linux and other desktops - a ProCreate-style experience, GPU-accelerated with Vulkan.',
  },
  header: {
    title: 'OxieDraw',
    logo: {
      light: '/logo.svg',
      dark: '/logo.svg',
      alt: 'OxieDraw',
    },
  },
  github: {
    url: 'https://github.com/yokarion/oxiedraw',
    branch: 'master',
  },
})
