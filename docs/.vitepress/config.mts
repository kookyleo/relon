import { defineConfig } from 'vitepress'

export default defineConfig({
  title: "Relon",
  description: "A production-grade, strongly-typed configuration language.",
  base: "/relon/",
  
  locales: {
    root: {
      label: '简体中文',
      lang: 'zh',
      themeConfig: {
        nav: [
          { text: '首页', link: '/' },
          { text: '指南', link: '/guide/introduction' }
        ],
        sidebar: [
          {
            text: '入门',
            items: [
              { text: '什么是 Relon？', link: '/guide/introduction' },
              { text: '基础语法', link: '/guide/syntax' },
            ]
          },
          {
            text: '核心特性',
            items: [
              { text: '函数与闭包', link: '/guide/functions' },
              { text: '类型与契约 (Schema)', link: '/guide/types' },
              { text: '模块与作用域', link: '/guide/modules' },
            ]
          },
          {
            text: '参考',
            items: [
              { text: '标准库', link: '/guide/stdlib' },
            ]
          }
        ],
        footer: {
          message: '在 MIT 许可下发布。',
          copyright: 'Copyright © 2026 kookyleo'
        },
        docFooter: {
          prev: '上一页',
          next: '下一页'
        },
        outline: {
          label: '页面导航'
        }
      }
    },
    en: {
      label: 'English',
      lang: 'en',
      link: '/en/',
      themeConfig: {
        nav: [
          { text: 'Home', link: '/en/' },
          { text: 'Guide', link: '/en/guide/introduction' }
        ],
        sidebar: [
          {
            text: 'Introduction',
            items: [
              { text: 'What is Relon?', link: '/en/guide/introduction' },
            ]
          }
        ],
        footer: {
          message: 'Released under the MIT License.',
          copyright: 'Copyright © 2026 kookyleo'
        }
      }
    }
  },

  themeConfig: {
    socialLinks: [
      { icon: 'github', link: 'https://github.com/kookyleo/relon' }
    ],
    search: {
      provider: 'local'
    }
  }
})
