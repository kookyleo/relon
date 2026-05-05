import DefaultTheme from 'vitepress/theme';
import { h } from 'vue';
import SelectionFeedback from './components/SelectionFeedback.vue';
import RelonGallery from './components/RelonGallery.vue';
import './style.css';

export default {
  extends: DefaultTheme,
  Layout() {
    return h(DefaultTheme.Layout, null, {
      'layout-bottom': () => h(SelectionFeedback),
    });
  },
  enhanceApp({ app }) {
    app.component('RelonGallery', RelonGallery);
  },
};
