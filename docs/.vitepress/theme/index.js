import DefaultTheme from 'vitepress/theme';
import { h } from 'vue';
import { useData } from 'vitepress';
import SelectionFeedback from './components/SelectionFeedback.vue';
import RelonGallery from './components/RelonGallery.vue';
import Playground from './components/Playground.vue';
import PlaygroundLayout from './components/PlaygroundLayout.vue';
import './style.css';

export default {
  extends: DefaultTheme,
  Layout() {
    const { frontmatter } = useData();
    if (frontmatter.value.layout === 'playground') {
      return h(PlaygroundLayout);
    }
    return h(DefaultTheme.Layout, null, {
      'layout-bottom': () => h(SelectionFeedback),
    });
  },
  enhanceApp({ app }) {
    app.component('RelonGallery', RelonGallery);
    // `<Playground />` is the SSR-safe wrapper; the real CodeMirror /
    // wasm-driven editor in `PlaygroundClient.vue` is loaded lazily on
    // the client. See `components/Playground.vue` for the rationale.
    app.component('Playground', Playground);
  },
};
