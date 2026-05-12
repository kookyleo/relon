<!--
  SSR-safe wrapper for the in-browser Relon playground.

  Two reasons this file exists separately from `PlaygroundClient.vue`:

    1. CodeMirror 6 is a pure-ESM library that touches `document` /
       `window` at import time. VitePress builds run under Node where
       neither global exists — even *importing* the module crashes the
       SSR pass. Routing the real component through `<ClientOnly>` plus
       a `defineAsyncComponent` keeps the build green; the async loader
       is only evaluated in the browser.
    2. The wasm runtime is ~1.1 MiB and must download asynchronously.
       Showing a placeholder while the bundle and the wasm hydrate
       gives users immediate feedback rather than a blank slate.

  Mounted from `docs/zh/playground.md` and `docs/en/playground.md`
  (both linked from the per-locale sidebar's "Getting started" group).
-->
<script setup lang="ts">
import { defineAsyncComponent } from 'vue';

// Async load: Vite chunks the heavy CM/playground code out of the main
// docs bundle so navigating to other pages stays cheap.
const PlaygroundClient = defineAsyncComponent(() => import('./PlaygroundClient.vue'));
</script>

<template>
  <ClientOnly>
    <PlaygroundClient />
    <template #fallback>
      <div class="relon-playground-fallback">
        <p>Loading playground…</p>
      </div>
    </template>
  </ClientOnly>
</template>

<style scoped>
.relon-playground-fallback {
  padding: 2rem;
  text-align: center;
  color: var(--vp-c-text-2);
  font-size: 0.9rem;
}
</style>
