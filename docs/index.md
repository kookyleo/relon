---
layout: false
---

<script setup>
import { onMounted } from 'vue'

onMounted(() => {
  const lang = typeof window !== 'undefined' ? window.navigator.language || window.navigator.userLanguage : 'en';
  if (lang.toLowerCase().includes('zh')) {
    window.location.replace('/relon/zh/');
  } else {
    window.location.replace('/relon/en/');
  }
})
</script>

<div style="display: flex; justify-content: center; align-items: center; height: 100vh; font-family: sans-serif;">
  <p>Redirecting to your preferred language...</p>
</div>
