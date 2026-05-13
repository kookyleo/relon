<!--
  Standalone layout for /zh/playground and /en/playground.

  The playground is not rendered inside the default VitePress doc shell
  (navbar/sidebar/footer/typography) — it's a full-viewport app, reached
  via the top nav and presented chrome-free. The Layout switch happens
  in `theme/index.js` based on the page's `layout: playground`
  frontmatter.
-->
<script setup lang="ts">
import Playground from './Playground.vue';
</script>

<template>
  <div class="playground-standalone">
    <Playground />
  </div>
</template>

<style scoped>
.playground-standalone {
  position: fixed;
  inset: 0;
  background: var(--vp-c-bg);
  display: flex;
  flex-direction: column;
}

/* Override `min-height: 600px` from the playground itself — in
   standalone mode we want it to fit the viewport, even on small
   screens (where the internal panes' min-heights still apply). */
.playground-standalone :deep(.relon-playground) {
  flex: 1 1 auto;
  height: auto;
  min-height: 0;
  border: none;
  border-radius: 0;
}
</style>
