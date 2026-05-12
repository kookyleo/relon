<!--
  Real Relon playground UI. Loaded client-side only by Playground.vue.

  Composition:
    - left  : CodeMirror 6 editor over the active file
    - right : JSON view of the last successful evaluate (read-only CM)
              + a disabled "Rendered" tab placeholder for Wave 3
    - bottom: collapsible error panel surfacing every ErrorReport span,
              with click-through that re-focuses the offending file and
              scrolls the editor to the offset.

  Wasm is loaded once via dynamic `import()` of the wasm-pack `--target
  web` glue under `/wasm/relon/`. `import.meta.env.BASE_URL` handles
  VitePress's `base: '/relon/'`. Evaluate / format run synchronously on
  the main thread; they're millisecond-scale at the source sizes the
  playground supports, so a worker is not warranted yet (see design doc
  §6 risk #4).

  Capabilities are the wasm crate's `Context::sandboxed()` defaults: fs,
  net, clock, env, rng all denied. Trying e.g. `fs.read` from the
  playground surfaces as `EvalError + CapabilityDenied` — visible in the
  error panel, which is the demo-correct behaviour.
-->
<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';

import { EditorState, Compartment } from '@codemirror/state';
import { EditorView, keymap, lineNumbers, highlightActiveLine, gutter } from '@codemirror/view';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, indentOnInput, syntaxHighlighting, defaultHighlightStyle } from '@codemirror/language';
import { setDiagnostics, lintGutter, type Diagnostic } from '@codemirror/lint';
import { json as jsonLang } from '@codemirror/lang-json';

import { relonLanguage } from './playground/relon-mode';

// ---------------- types --------------------------------------------------

interface PlaygroundFile {
    path: string;
    content: string;
}

interface ErrorSpan {
    file: string | null;
    start: number;
    end: number;
    label: string | null;
}

interface ErrorReport {
    kind: 'InvalidInput' | 'ParseError' | 'AnalyzeError' | 'EvalError' | 'ProjectionError';
    message: string;
    spans: ErrorSpan[];
    help: string | null;
    code: string | null;
}

interface WasmModule {
    default: (init?: unknown) => Promise<unknown>;
    evaluate: (sources: unknown, entry: string) => unknown;
    format: (content: string) => string;
    version: () => string;
}

// ---------------- preset --------------------------------------------------

// Default content: small, immediately interesting, exercises function
// definitions, sibling refs, decorators, and f-strings. Lifted from
// `examples/demo.relon` and lightly trimmed for vertical space.
const DEFAULT_MAIN = `// Try editing me - evaluate runs automatically.
{
    currency(val, symbol): val + " " + symbol,
    multiply(a, b): a * b,
    project: {
        name: "Relon Playground",
        details: {
            base_price: 1500,
            total: multiply(&sibling.base_price, 1.2),
            @currency("GBP")
            display: &sibling.total
        }
    },
    meta: {
        tags_count: len(["rust", "config", "dsl"]),
        summary: f"Active project: \${&root.project.name}"
    }
}
`;

// ---------------- reactive state -----------------------------------------

const files = ref<PlaygroundFile[]>([
    { path: 'main.relon', content: DEFAULT_MAIN },
]);
const activeFile = ref<string>('main.relon');
const entry = ref<string>('main.relon');
const viewMode = ref<'json' | 'rendered'>('json');
const resultJson = ref<string>('');
const errors = ref<ErrorReport[]>([]);
const errorsExpanded = ref<boolean>(false);
const status = ref<string>('Loading runtime…');
const wasmVersion = ref<string>('');
const isReady = ref<boolean>(false);

// Refs to DOM mount points.
const editorHost = ref<HTMLElement | null>(null);
const jsonHost = ref<HTMLElement | null>(null);

// Editor instances (shallow: CM objects are mutable, we don't want Vue
// to reach inside them).
const editorView = shallowRef<EditorView | null>(null);
const jsonView = shallowRef<EditorView | null>(null);
const wasmRef = shallowRef<WasmModule | null>(null);
const langCompartment = new Compartment();

// ---------------- derived -------------------------------------------------

const activeFileObj = computed(() =>
    files.value.find((f) => f.path === activeFile.value) ?? files.value[0]
);

const errorCount = computed(() => errors.value.reduce((acc, r) => acc + (r.spans.length || 1), 0));

// ---------------- helpers -------------------------------------------------

function updateActiveContent(next: string) {
    const target = files.value.find((f) => f.path === activeFile.value);
    if (target) target.content = next;
}

function selectFile(path: string) {
    if (path === activeFile.value) return;
    activeFile.value = path;
    const view = editorView.value;
    const file = files.value.find((f) => f.path === path);
    if (view && file) {
        view.dispatch({
            changes: { from: 0, to: view.state.doc.length, insert: file.content },
        });
        applyDiagnosticsForActive();
    }
}

function setEntry(path: string) {
    entry.value = path;
    void runEvaluate();
}

function addFile() {
    // Minimal new-file UX: a `prompt()` is jarring but cheap. Wave 3 can
    // upgrade to an inline form. Block paths already present.
    const next = window.prompt('New file path (e.g. lib.relon):', 'lib.relon');
    if (!next) return;
    const trimmed = next.trim();
    if (!trimmed) return;
    if (files.value.some((f) => f.path === trimmed)) {
        window.alert(`A file named "${trimmed}" already exists.`);
        return;
    }
    files.value.push({ path: trimmed, content: '{\n    // ' + trimmed + '\n}\n' });
    selectFile(trimmed);
}

function removeFile(path: string) {
    if (files.value.length === 1) return;
    const idx = files.value.findIndex((f) => f.path === path);
    if (idx === -1) return;
    files.value.splice(idx, 1);
    if (entry.value === path) entry.value = files.value[0].path;
    if (activeFile.value === path) selectFile(files.value[0].path);
    void runEvaluate();
}

// ---------------- wasm boot ----------------------------------------------

async function bootWasm() {
    try {
        // VitePress `base` is `/relon/`; `BASE_URL` reflects that. We
        // intentionally avoid bundling the wasm into the Vite graph
        // (it would force a wasm-bindgen-compatible Vite plugin); the
        // file lives under `public/` and is fetched at runtime.
        const base = (import.meta as ImportMeta & { env: { BASE_URL: string } }).env.BASE_URL || '/';
        const glueUrl = new URL('wasm/relon/relon_wasm.js', window.location.origin + base);
        const wasmUrl = new URL('wasm/relon/relon_wasm_bg.wasm', window.location.origin + base);
        // `/* @vite-ignore */` suppresses Vite trying to follow the URL
        // statically; we want a plain runtime fetch.
        const mod = (await import(/* @vite-ignore */ glueUrl.href)) as WasmModule;
        await mod.default(wasmUrl);
        wasmRef.value = mod;
        wasmVersion.value = mod.version();
        isReady.value = true;
        status.value = `Ready (relon-wasm v${wasmVersion.value})`;
        await runEvaluate();
    } catch (err) {
        status.value = `Failed to load wasm runtime: ${err instanceof Error ? err.message : String(err)}`;
        console.error('[playground] wasm boot failed', err);
    }
}

// ---------------- evaluate / format --------------------------------------

let evalTimer: ReturnType<typeof setTimeout> | null = null;

function scheduleEvaluate() {
    if (evalTimer !== null) clearTimeout(evalTimer);
    evalTimer = setTimeout(() => {
        evalTimer = null;
        void runEvaluate();
    }, 200);
}

async function runEvaluate() {
    const mod = wasmRef.value;
    if (!mod) return;
    const payload = files.value.map((f) => ({ path: f.path, content: f.content }));
    try {
        const value = mod.evaluate(payload, entry.value);
        errors.value = [];
        resultJson.value = JSON.stringify(value, null, 2);
        applyDiagnosticsForActive();
    } catch (raw) {
        const report = coerceErrorReport(raw);
        errors.value = [report];
        resultJson.value = '';
        errorsExpanded.value = true;
        applyDiagnosticsForActive();
    }
    if (jsonView.value) {
        jsonView.value.dispatch({
            changes: { from: 0, to: jsonView.value.state.doc.length, insert: resultJson.value },
        });
    }
}

function runFormat() {
    const mod = wasmRef.value;
    const view = editorView.value;
    if (!mod || !view) return;
    const current = view.state.doc.toString();
    try {
        const formatted = mod.format(current);
        view.dispatch({
            changes: { from: 0, to: view.state.doc.length, insert: formatted },
        });
        // Formatter writing back will fire `updateListener` which
        // updates the model + reschedules an evaluate.
    } catch (raw) {
        // Format failures don't block evaluate; we still bubble them.
        const report = coerceErrorReport(raw);
        errors.value = [report];
        errorsExpanded.value = true;
    }
}

function coerceErrorReport(raw: unknown): ErrorReport {
    // wasm-bindgen throws our `ErrorReport` JSON as the rejection value.
    // Defensive normalisation in case something else slipped through
    // (e.g. a runtime JS error in the glue itself).
    if (raw && typeof raw === 'object') {
        const r = raw as Partial<ErrorReport>;
        if (typeof r.kind === 'string' && typeof r.message === 'string') {
            return {
                kind: r.kind as ErrorReport['kind'],
                message: r.message,
                spans: Array.isArray(r.spans) ? (r.spans as ErrorSpan[]) : [],
                help: r.help ?? null,
                code: r.code ?? null,
            };
        }
    }
    return {
        kind: 'EvalError',
        message: raw instanceof Error ? raw.message : String(raw),
        spans: [],
        help: null,
        code: null,
    };
}

// ---------------- editor lifecycle ---------------------------------------

function applyDiagnosticsForActive() {
    const view = editorView.value;
    if (!view) return;
    const docLen = view.state.doc.length;
    const diagnostics: Diagnostic[] = [];
    for (const report of errors.value) {
        for (const span of report.spans) {
            if (span.file !== activeFile.value) continue;
            // Clamp to current doc length; a recently edited buffer can
            // drift past the offsets the analyzer saw.
            const from = Math.min(span.start, docLen);
            const to = Math.min(span.end > span.start ? span.end : span.start + 1, docLen);
            diagnostics.push({
                from,
                to,
                severity: 'error',
                message: span.label ? `${span.label}: ${report.message}` : report.message,
                source: report.code ?? report.kind,
            });
        }
    }
    view.dispatch(setDiagnostics(view.state, diagnostics));
}

function jumpToSpan(report: ErrorReport, span: ErrorSpan) {
    if (!span.file) return;
    if (span.file !== activeFile.value) selectFile(span.file);
    // After selectFile dispatches a doc-replace we need to wait one tick
    // before scrolling, or the offset is stale relative to the old doc.
    queueMicrotask(() => {
        const view = editorView.value;
        if (!view) return;
        const pos = Math.min(span.start, view.state.doc.length);
        view.dispatch({
            selection: { anchor: pos, head: pos },
            scrollIntoView: true,
        });
        view.focus();
    });
    // Surface the message even when the span is on the active file —
    // helpful when the error is in a region not currently visible.
    void report;
}

onMounted(() => {
    if (!editorHost.value) return;
    const startDoc = activeFileObj.value?.content ?? '';

    const updateListener = EditorView.updateListener.of((v) => {
        if (!v.docChanged) return;
        updateActiveContent(v.state.doc.toString());
        scheduleEvaluate();
    });

    const state = EditorState.create({
        doc: startDoc,
        extensions: [
            lineNumbers(),
            highlightActiveLine(),
            history(),
            bracketMatching(),
            indentOnInput(),
            syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
            lintGutter(),
            gutter({ class: 'cm-relon-gutter' }),
            keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
            langCompartment.of(relonLanguage()),
            EditorView.lineWrapping,
            updateListener,
        ],
    });
    editorView.value = new EditorView({ state, parent: editorHost.value });

    if (jsonHost.value) {
        const jsonState = EditorState.create({
            doc: '',
            extensions: [
                lineNumbers(),
                EditorView.editable.of(false),
                EditorState.readOnly.of(true),
                jsonLang(),
                syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
                EditorView.lineWrapping,
            ],
        });
        jsonView.value = new EditorView({ state: jsonState, parent: jsonHost.value });
    }

    void bootWasm();
});

onBeforeUnmount(() => {
    if (evalTimer !== null) clearTimeout(evalTimer);
    editorView.value?.destroy();
    jsonView.value?.destroy();
});

// Keep entry highlight in sync when files mutate.
watch(files, () => {
    if (!files.value.some((f) => f.path === entry.value)) {
        entry.value = files.value[0]?.path ?? '';
    }
}, { deep: true });
</script>

<template>
  <div class="relon-playground">
    <header class="rp-status">
      <span class="rp-status-text">{{ status }}</span>
    </header>

    <div class="rp-panes">
      <section class="rp-pane rp-pane-editor">
        <div class="rp-tabs">
          <button
            v-for="f in files"
            :key="f.path"
            class="rp-tab"
            :class="{ 'is-active': f.path === activeFile }"
            :title="entry === f.path ? 'Entry file' : 'Click star to make entry'"
            @click="selectFile(f.path)"
          >
            <span class="rp-tab-label">{{ f.path }}</span>
            <span
              class="rp-tab-entry"
              :class="{ 'is-entry': entry === f.path }"
              :title="entry === f.path ? 'Entry file' : 'Set as entry'"
              @click.stop="setEntry(f.path)"
            >★</span>
            <span
              v-if="files.length > 1"
              class="rp-tab-close"
              title="Remove file"
              @click.stop="removeFile(f.path)"
            >×</span>
          </button>
          <button class="rp-tab rp-tab-add" title="Add a new file" @click="addFile">+</button>
          <span class="rp-spacer" />
          <button
            class="rp-action"
            title="Format active buffer"
            :disabled="!isReady"
            @click="runFormat"
          >Format</button>
        </div>
        <div ref="editorHost" class="rp-editor"></div>
      </section>

      <section class="rp-pane rp-pane-output">
        <div class="rp-tabs">
          <button
            class="rp-tab"
            :class="{ 'is-active': viewMode === 'json' }"
            @click="viewMode = 'json'"
          >JSON</button>
          <button
            class="rp-tab is-disabled"
            disabled
            title="Coming in next wave"
          >Rendered (coming soon)</button>
        </div>
        <div v-show="viewMode === 'json'" ref="jsonHost" class="rp-output"></div>
      </section>
    </div>

    <details class="rp-errors" :open="errorsExpanded && errors.length > 0">
      <summary @click="errorsExpanded = !errorsExpanded">
        <span class="rp-errors-title">Errors ({{ errorCount }})</span>
      </summary>
      <div v-if="errors.length === 0" class="rp-errors-empty">No errors.</div>
      <ul v-else class="rp-errors-list">
        <li v-for="(report, idx) in errors" :key="idx" class="rp-error">
          <div class="rp-error-head">
            <span class="rp-error-kind" :data-kind="report.kind">{{ report.kind }}</span>
            <span v-if="report.code" class="rp-error-code">{{ report.code }}</span>
          </div>
          <div class="rp-error-message">{{ report.message }}</div>
          <div v-if="report.help" class="rp-error-help">{{ report.help }}</div>
          <ul v-if="report.spans.length > 0" class="rp-error-spans">
            <li
              v-for="(span, sidx) in report.spans"
              :key="sidx"
              class="rp-error-span"
              :class="{ 'is-clickable': !!span.file }"
              @click="span.file && jumpToSpan(report, span)"
            >
              <code>{{ span.file ?? '<workspace>' }}:{{ span.start }}-{{ span.end }}</code>
              <span v-if="span.label" class="rp-error-span-label"> — {{ span.label }}</span>
            </li>
          </ul>
        </li>
      </ul>
    </details>
  </div>
</template>

<style scoped>
.relon-playground {
  display: flex;
  flex-direction: column;
  min-height: 600px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 6px;
  overflow: hidden;
  background: var(--vp-c-bg);
  font-family: var(--vp-font-family-base);
  font-size: 14px;
}

.rp-status {
  padding: 4px 12px;
  background: var(--vp-c-bg-soft);
  border-bottom: 1px solid var(--vp-c-divider);
  color: var(--vp-c-text-2);
  font-size: 12px;
}

.rp-panes {
  display: grid;
  grid-template-columns: 1fr 1fr;
  flex: 1 1 auto;
  min-height: 480px;
}

.rp-pane {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.rp-pane-editor {
  border-right: 1px solid var(--vp-c-divider);
}

.rp-tabs {
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 4px 6px;
  background: var(--vp-c-bg-soft);
  border-bottom: 1px solid var(--vp-c-divider);
  overflow-x: auto;
}

.rp-spacer { flex: 1 1 auto; }

.rp-tab {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 3px 10px;
  border: 1px solid transparent;
  border-radius: 4px;
  background: transparent;
  color: var(--vp-c-text-2);
  cursor: pointer;
  font-size: 12px;
  white-space: nowrap;
}

.rp-tab:hover:not(.is-disabled):not(:disabled) {
  background: var(--vp-c-default-soft);
  color: var(--vp-c-text-1);
}

.rp-tab.is-active {
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  border-color: var(--vp-c-divider);
}

.rp-tab.is-disabled,
.rp-tab:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}

.rp-tab-entry {
  color: var(--vp-c-text-3);
  cursor: pointer;
  line-height: 1;
}

.rp-tab-entry.is-entry {
  color: gold;
}

.rp-tab-close {
  color: var(--vp-c-text-3);
  cursor: pointer;
  line-height: 1;
  padding-left: 4px;
}

.rp-tab-close:hover { color: var(--vp-c-danger-1, #e0535b); }

.rp-tab-add {
  font-weight: bold;
  color: var(--vp-c-text-2);
}

.rp-action {
  padding: 3px 10px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 4px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  cursor: pointer;
  font-size: 12px;
}

.rp-action:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.rp-editor, .rp-output {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
}

.rp-editor :deep(.cm-editor),
.rp-output :deep(.cm-editor) {
  height: 100%;
  font-family: var(--vp-font-family-mono);
  font-size: 13px;
}

.rp-editor :deep(.cm-scroller),
.rp-output :deep(.cm-scroller) {
  font-family: var(--vp-font-family-mono);
}

.rp-errors {
  border-top: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg-soft);
  font-size: 12px;
}

.rp-errors summary {
  padding: 6px 12px;
  cursor: pointer;
  user-select: none;
  color: var(--vp-c-text-2);
}

.rp-errors-empty {
  padding: 6px 12px 12px;
  color: var(--vp-c-text-3);
}

.rp-errors-list {
  list-style: none;
  margin: 0;
  padding: 4px 12px 12px;
}

.rp-error {
  padding: 8px;
  margin-bottom: 6px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 4px;
  background: var(--vp-c-bg);
}

.rp-error-head {
  display: flex;
  gap: 8px;
  align-items: baseline;
  margin-bottom: 4px;
}

.rp-error-kind {
  display: inline-block;
  padding: 1px 6px;
  border-radius: 3px;
  font-weight: 600;
  font-size: 11px;
  background: var(--vp-c-danger-soft, #fbeaeb);
  color: var(--vp-c-danger-1, #b8333a);
}

.rp-error-kind[data-kind="ParseError"] { background: var(--vp-c-warning-soft, #fff4d6); color: var(--vp-c-warning-1, #8a6500); }
.rp-error-kind[data-kind="InvalidInput"] { background: var(--vp-c-default-soft); color: var(--vp-c-text-2); }

.rp-error-code {
  font-family: var(--vp-font-family-mono);
  font-size: 11px;
  color: var(--vp-c-text-3);
}

.rp-error-message {
  font-family: var(--vp-font-family-mono);
  font-size: 12px;
  margin-bottom: 4px;
  white-space: pre-wrap;
  word-break: break-word;
}

.rp-error-help {
  color: var(--vp-c-text-2);
  margin-bottom: 4px;
}

.rp-error-spans {
  list-style: none;
  margin: 0;
  padding: 0;
}

.rp-error-span {
  padding: 2px 0;
  font-family: var(--vp-font-family-mono);
  font-size: 11px;
}

.rp-error-span.is-clickable {
  cursor: pointer;
  color: var(--vp-c-brand-1, var(--vp-c-text-1));
}

.rp-error-span.is-clickable:hover {
  text-decoration: underline;
}

.rp-error-span-label {
  color: var(--vp-c-text-2);
  font-style: italic;
}

@media (max-width: 768px) {
  .rp-panes {
    grid-template-columns: 1fr;
    grid-template-rows: 1fr 1fr;
  }
  .rp-pane-editor {
    border-right: none;
    border-bottom: 1px solid var(--vp-c-divider);
  }
}
</style>
