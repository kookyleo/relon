import { defineConfig } from 'vitepress'

// https://vitepress.dev/reference/site-config
export default defineConfig({
  title: "Relon",
  description: "A production-grade, strongly-typed configuration language and UI template engine.",
  base: "/relon/",
  
  themeConfig: {
    // https://vitepress.dev/reference/default-theme-config
    nav: [
      { text: 'Home', link: '/' },
      { text: 'Guide', link: '/guide/introduction' }
    ],

    sidebar: [
      {
        text: 'Introduction',
        items: [
          { text: 'What is Relon?', link: '/guide/introduction' },
        ]
      }
    ],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/kookyleo/relon' }
    ],

    footer: {
      message: 'Released under the MIT License.',
      copyright: 'Copyright © 2026 kookyleo'
    }
  }
})
