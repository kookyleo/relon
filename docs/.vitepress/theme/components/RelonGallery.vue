<script setup>
import { ref, computed } from 'vue';
import { useData, withBase } from 'vitepress';

const { lang } = useData();
const isZh = computed(() => (lang.value || '').startsWith('zh'));

const examples = [
  {
    id: 'plans',
    svg: '/gallery_plans.svg',
    titleZh: '多租户订阅档位',
    titleEn: 'Multi-tenant Plans',
    blurbZh: 'Plan 是带载荷的 sum-type；产物 JSON 用外部标签展开 Free / Pro / Enterprise——后端鉴权、计费、Webhook 共读一份真相。',
    blurbEn: 'Plan is a payload-bearing sum-type; the output expands as externally-tagged JSON, read by auth, billing and webhook services from one source of truth.',
  },
  {
    id: 'form',
    svg: '/gallery_form.svg',
    titleZh: '员工入职表单',
    titleEn: 'Employee Onboarding Form',
    blurbZh: 'Field 类型用 sum-type 取代 JSON Schema 的 oneOf；前端 form renderer 直接吃 JSON 渲染组件。',
    blurbEn: 'Field types as a sum-type, replacing JSON Schema’s oneOf nightmare; the front-end form renderer ingests the JSON as-is.',
  },
  {
    id: 'comfy',
    svg: '/gallery_comfy.svg',
    titleZh: 'ComfyUI 文生图工作流',
    titleEn: 'ComfyUI txt2img Workflow',
    blurbZh: '不再手编 ComfyUI 那一坨 workflow JSON——每个节点都是 sum-type 变体，参数被类型保护。',
    blurbEn: 'Stop hand-editing ComfyUI workflow JSON — every node is a typed sum-type variant with parameters under type guard.',
  },
  {
    id: 'caddy',
    svg: '/gallery_caddy.svg',
    titleZh: 'Caddy 反向代理',
    titleEn: 'Caddy Reverse Proxy',
    blurbZh: '路由是 Reverse | Redirect | Static 的 sum-type；POST 给 Caddy admin API 立即生效。',
    blurbEn: 'Routes as a Reverse | Redirect | Static sum-type; POST the JSON to Caddy’s admin API to apply.',
  },
  {
    id: 'iam',
    svg: '/gallery_iam.svg',
    titleZh: 'AWS S3 访问策略',
    titleEn: 'AWS S3 Access Policy',
    blurbZh: 'Effect 是 Allow | Deny；字段名直接对齐 AWS 标准——同一份策略可灌进 AWS IAM、OPA 或自家鉴权。',
    blurbEn: 'Effect = Allow | Deny with field names that match AWS verbatim — the same policy feeds AWS IAM, OPA, or a homegrown authorizer.',
  },
];

const selected = ref(0);
const current = computed(() => examples[selected.value]);
const currentTitle = computed(() => isZh.value ? current.value.titleZh : current.value.titleEn);
const currentBlurb = computed(() => isZh.value ? current.value.blurbZh : current.value.blurbEn);

function thumbLabel(ex) {
  return isZh.value ? ex.titleZh : ex.titleEn;
}
</script>

<template>
  <section class="relon-gallery" aria-label="Relon use cases">
    <div class="gallery-inner">
      <header class="gallery-head">
        <h2 class="gallery-title">
          {{ isZh ? '它能帮你写哪些 JSON？' : 'What JSON can Relon write for you?' }}
        </h2>
        <p class="gallery-sub">
          {{ isZh
            ? '同一种「平台库 + 业务 entry → JSON」的写法，覆盖五个真实场景。点缩略图切换。'
            : 'One pattern — platform library plus business entry compiles to JSON — across five real domains. Click a tab to switch.' }}
        </p>
      </header>

      <div class="gallery-stage">
        <div class="gallery-current-title">{{ currentTitle }}</div>
        <div class="gallery-image">
          <img :src="withBase(current.svg)" :alt="currentTitle" />
        </div>
        <p class="gallery-blurb">{{ currentBlurb }}</p>
      </div>

      <div class="gallery-thumbs" role="tablist">
        <button
          v-for="(ex, i) in examples"
          :key="ex.id"
          :class="{ active: selected === i }"
          role="tab"
          :aria-selected="selected === i"
          @click="selected = i"
        >
          {{ thumbLabel(ex) }}
        </button>
      </div>
    </div>
  </section>
</template>

<style scoped>
.relon-gallery {
  position: relative;
  left: 50%;
  right: 50%;
  margin-left: -50vw;
  margin-right: -50vw;
  width: 100vw;
  padding: 3rem 0 4rem;
  background: var(--vp-c-bg-soft);
  border-top: 1px solid var(--vp-c-divider);
  border-bottom: 1px solid var(--vp-c-divider);
}

.gallery-inner {
  max-width: 1440px;
  margin: 0 auto;
  padding: 0 1.5rem;
}

.gallery-head {
  text-align: center;
  margin-bottom: 2rem;
}

.gallery-title {
  font-size: clamp(1.6rem, 3vw, 2.2rem);
  font-weight: 700;
  color: var(--vp-c-text-1);
  margin: 0;
  letter-spacing: -0.01em;
  border: none;
  padding: 0;
}

.gallery-sub {
  margin: 0.6rem auto 0;
  font-size: 0.95rem;
  color: var(--vp-c-text-2);
  max-width: 720px;
}

.gallery-stage {
  background: var(--vp-c-bg);
  border: 1px solid var(--vp-c-divider);
  border-radius: 14px;
  padding: 1.25rem 1.25rem 1.5rem;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.03);
}

.gallery-current-title {
  font-size: 0.85rem;
  font-weight: 600;
  color: var(--vp-c-brand-1);
  letter-spacing: 0.04em;
  text-transform: uppercase;
  margin-bottom: 0.5rem;
  text-align: center;
}

.gallery-image {
  width: 100%;
  line-height: 0;
}

.gallery-image img {
  display: block;
  width: 100%;
  height: auto;
}

.gallery-blurb {
  margin: 1rem auto 0;
  text-align: center;
  font-size: 0.95rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
  max-width: 860px;
}

.gallery-thumbs {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  justify-content: center;
  margin-top: 1.5rem;
}

.gallery-thumbs button {
  padding: 0.55rem 1.1rem;
  font-size: 0.875rem;
  font-weight: 500;
  color: var(--vp-c-text-2);
  background: var(--vp-c-bg);
  border: 1px solid var(--vp-c-divider);
  border-radius: 999px;
  cursor: pointer;
  transition: color 0.15s ease, border-color 0.15s ease, background 0.15s ease;
}

.gallery-thumbs button:hover {
  color: var(--vp-c-text-1);
  border-color: var(--vp-c-brand-2);
}

.gallery-thumbs button.active {
  color: var(--vp-c-bg);
  background: var(--vp-c-brand-1);
  border-color: var(--vp-c-brand-1);
}

@media (max-width: 768px) {
  .relon-gallery {
    padding: 2rem 0 2.5rem;
  }
  .gallery-stage {
    padding: 0.75rem 0.5rem 1rem;
  }
  .gallery-thumbs button {
    padding: 0.45rem 0.85rem;
    font-size: 0.8rem;
  }
}
</style>
