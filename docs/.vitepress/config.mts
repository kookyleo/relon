import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vitepress'

// Load the Relon TextMate grammar at config-eval time. Inlining via JSON
// import assertions would tie us to a specific Node/tsx config; reading
// the file ourselves keeps it portable across the toolchain.
const relonGrammar = JSON.parse(
  readFileSync(
    fileURLToPath(new URL('./lang/relon.tmLanguage.json', import.meta.url)),
    'utf8'
  )
)

export default defineConfig({
  title: "Relon",
  description: "A production-grade, strongly-typed configuration language.",
  base: "/relon/",

  // Register the Relon TextMate grammar so shiki highlights every
  // ```relon ...``` block instead of falling back to `txt` (which used
  // to emit one "language not loaded" warning per code fence at dev
  // startup). Token coverage is intentionally aligned with the
  // CodeMirror tokenizer in `theme/components/playground/relon-mode.ts`.
  markdown: {
    languages: [relonGrammar as any],
  },

  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: '/relon/favicon.svg' }],
    ['link', { rel: 'icon', type: 'image/x-icon', href: '/relon/favicon.ico' }]
  ],

  locales: {
    root: {
      label: 'Language',
      lang: 'en', // default for root is just redirect, but VitePress requires a root locale fallback
    },
    zh: {
      label: '简体中文',
      lang: 'zh',
      link: '/zh/',
      themeConfig: {
        nav: [
          { text: '首页', link: '/zh/' },
          { text: '指南', link: '/zh/guide/introduction' }
        ],
        sidebar: [
          {
            text: '入门',
            items: [
              { text: '什么是 Relon？', link: '/zh/guide/introduction' },
              { text: '业务场景与定位', link: '/zh/guide/use-cases' },
              { text: '基础语法', link: '/zh/guide/syntax' },
              { text: 'Playground', link: '/zh/playground' },
            ]
          },
          {
            text: '核心特性',
            items: [
              { text: '函数与闭包', link: '/zh/guide/functions' },
              { text: '类型与契约 (Schema)', link: '/zh/guide/types' },
              { text: '模块与作用域', link: '/zh/guide/modules' },
            ]
          },
          {
            text: '嵌入与安全',
            items: [
              { text: '嵌入宿主', link: '/zh/guide/host-integration' },
              { text: '沙箱与权限', link: '/zh/guide/sandbox' },
            ]
          },
          {
            text: '参考',
            items: [
              { text: '语言规范 (SPEC)', link: '/zh/guide/spec' },
              { text: '标准库', link: '/zh/guide/stdlib' },
              { text: '架构概览', link: '/zh/guide/architecture' },
            ]
          }
        ],
        footer: {
          message: '在 Apache 2.0 许可下发布。',
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
            text: 'Getting started',
            items: [
              { text: 'What is Relon?', link: '/en/guide/introduction' },
              { text: 'Use cases & positioning', link: '/en/guide/use-cases' },
              { text: 'Syntax basics', link: '/en/guide/syntax' },
              { text: 'Playground', link: '/en/playground' },
            ]
          },
          {
            text: 'Core features',
            items: [
              { text: 'Functions & closures', link: '/en/guide/functions' },
              { text: 'Types & schema contracts', link: '/en/guide/types' },
              { text: 'Modules & scope', link: '/en/guide/modules' },
            ]
          },
          {
            text: 'Embedding & sandbox',
            items: [
              { text: 'Host integration', link: '/en/guide/host-integration' },
              { text: 'Sandbox & capabilities', link: '/en/guide/sandbox' },
            ]
          },
          {
            text: 'Reference',
            items: [
              { text: 'Language spec', link: '/en/guide/spec' },
              { text: 'Standard library', link: '/en/guide/stdlib' },
              { text: 'Architecture overview', link: '/en/guide/architecture' },
            ]
          }
        ],
        footer: {
          message: 'Released under the Apache 2.0 License.',
          copyright: 'Copyright © 2026 kookyleo'
        }
      }
    }
  },

  themeConfig: {
    logo: '/logo-mini.svg',
    siteTitle: 'Relon',
    socialLinks: [
      { icon: 'github', link: 'https://github.com/kookyleo/relon' }
    ],
    search: {
      provider: 'local'
    }
  }
})
