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
import { PRESETS, DEFAULT_PRESET_ID, type Preset } from './playground/presets';
import {
    runtimeErrorExtension,
    setRuntimeErrors,
    type RuntimeErrorMark,
} from './playground/runtime-errors';

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
    // wasm-pack `--target web` glue 0.2.x+ accepts `{ module_or_path }`;
    // the legacy positional shape still works but logs a deprecation
    // warning. We pass an object to keep the console clean.
    default: (init?: { module_or_path?: unknown }) => Promise<unknown>;
    evaluate: (sources: unknown, entry: string) => unknown;
    evaluate_main: (sources: unknown, entry: string, args: unknown) => unknown;
    format: (content: string) => string;
    version: () => string;
}

// ---------------- reactive state -----------------------------------------

// Initial state derives from the default preset so the playground boots
// with something interesting; subsequent preset selections load through
// `loadPreset()` below (which also handles the multi-file case).
const initialPreset = PRESETS.find((p) => p.id === DEFAULT_PRESET_ID) ?? PRESETS[0];
const files = ref<PlaygroundFile[]>(
    initialPreset.files.map((f) => ({ path: f.path, content: f.content }))
);
const activeFile = ref<string>(initialPreset.files[0].path);
const entry = ref<string>(initialPreset.entry);
const viewMode = ref<'json' | 'rendered'>('json');
const resultJson = ref<string>('');
const errors = ref<ErrorReport[]>([]);
const errorsExpanded = ref<boolean>(false);
const status = ref<string>('Loading runtime…');
const wasmVersion = ref<string>('');
const isReady = ref<boolean>(false);

// Currently selected preset id; drives the dropdown and the explanatory
// banner. Not persisted across reloads — keeping URL hash-routing out of
// scope for the first cut.
const presetId = ref<string>(DEFAULT_PRESET_ID);
const activePreset = computed<Preset>(() =>
    PRESETS.find((p) => p.id === presetId.value) ?? PRESETS[0]
);

// New-file modal state. We avoid `window.prompt` because it blocks the
// event loop, can't be themed, and on some browsers (notably mobile
// Safari) it's been quietly removed. Native `<dialog>` is in every
// evergreen target VitePress supports today.
const newFileDialog = ref<HTMLDialogElement | null>(null);
const newFilePath = ref<string>('');
const newFileError = ref<string>('');

// Refs to DOM mount points.
const editorHost = ref<HTMLElement | null>(null);
const jsonHost = ref<HTMLElement | null>(null);
const panesEl = ref<HTMLElement | null>(null);

// Editor / output split, in percent of `.rp-panes` width. Pointer drag
// on `.rp-resizer` mutates it; clamped to keep both panes usable.
const editorWidthPct = ref<number>(50);

// Errors panel height in CSS pixels (only honoured when the panel is
// open). Drag the top edge to resize — Chrome devtools style.
const errorsHeight = ref<number>(240);

// Args input for `#main(...)` entries. Pre-filled from the preset's
// `defaultArgs` and editable; the Run button feeds it to wasm's
// `evaluate_main`. Empty text means "no args" (auto-eval handles that).
const argsInput = ref<string>(initialPreset.defaultArgs ?? '');

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

function openNewFileDialog() {
    newFilePath.value = 'lib.relon';
    newFileError.value = '';
    const dlg = newFileDialog.value;
    if (!dlg) return;
    // `showModal` is the right call here — it grabs focus, traps the
    // Tab cycle, and closes on Esc without us wiring a keydown handler.
    if (typeof dlg.showModal === 'function') {
        dlg.showModal();
    } else {
        // Fallback for the (vanishingly small) set of browsers that
        // shipped without `<dialog>` support.
        dlg.setAttribute('open', '');
    }
    // Defer focus to after the dialog has actually rendered.
    queueMicrotask(() => {
        const input = dlg.querySelector('input');
        if (input) (input as HTMLInputElement).focus();
    });
}

function closeNewFileDialog() {
    const dlg = newFileDialog.value;
    if (!dlg) return;
    if (typeof dlg.close === 'function') {
        dlg.close();
    } else {
        dlg.removeAttribute('open');
    }
}

function confirmNewFile() {
    const raw = newFilePath.value.trim();
    if (!raw) {
        newFileError.value = 'Path cannot be empty.';
        return;
    }
    // Append `.relon` if the user dropped the extension — keeps the
    // import-by-path scenario obvious. Matching is case-sensitive
    // because module ids are case-sensitive on the analyzer side.
    const path = raw.endsWith('.relon') ? raw : `${raw}.relon`;
    if (files.value.some((f) => f.path === path)) {
        newFileError.value = `A file named "${path}" already exists.`;
        return;
    }
    files.value.push({ path, content: `{\n    // ${path}\n}\n` });
    closeNewFileDialog();
    selectFile(path);
}

function loadPreset(id: string) {
    const preset = PRESETS.find((p) => p.id === id);
    if (!preset) return;
    presetId.value = id;
    argsInput.value = preset.defaultArgs ?? '';
    // Replace the full file set; keeps things deterministic even when
    // the user has been editing — we trade auto-save for predictability.
    files.value = preset.files.map((f) => ({ path: f.path, content: f.content }));
    entry.value = preset.entry;
    const nextActive = preset.files[0].path;
    activeFile.value = nextActive;
    const view = editorView.value;
    if (view) {
        view.dispatch({
            changes: { from: 0, to: view.state.doc.length, insert: preset.files[0].content },
        });
    }
    // Evaluate against the new payload. For non-sandbox-runnable presets
    // this surfaces a genuine `EvalError` / `AnalyzeError`, which is the
    // demo-correct behaviour; the context hint inside the error panel
    // explains why and points at the CLI.
    void runEvaluate();
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
        // wasm-bindgen 0.2.93+ deprecated the positional-arg shape in
        // favour of `{ module_or_path }`. Passing the URL directly still
        // works but logs a console warning every page load; passing the
        // object keeps the console clean and is forward-compatible.
        await mod.default({ module_or_path: wasmUrl });
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

/// Run the entry through `evaluate_main`, feeding it the user-supplied
/// args JSON. Triggered by the explicit Run button — auto-eval keeps
/// using arg-less `evaluate` so `#main(...)` scripts surface their
/// missing-arg error live as you type.
async function runWithArgs() {
    const mod = wasmRef.value;
    if (!mod) return;
    const payload = files.value.map((f) => ({ path: f.path, content: f.content }));
    let parsedArgs: unknown = undefined;
    const text = argsInput.value.trim();
    if (text.length > 0) {
        try {
            parsedArgs = JSON.parse(text);
        } catch (err) {
            errors.value = [{
                kind: 'InvalidInput',
                message: `Args is not valid JSON: ${(err as Error).message}`,
                spans: [],
                help: 'The Args box accepts a JSON object keyed by `#main(...)` parameter names.',
                code: null,
            }];
            errorsExpanded.value = true;
            applyDiagnosticsForActive();
            return;
        }
    }
    try {
        const value = mod.evaluate_main(payload, entry.value, parsedArgs);
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
    const runtimeMarks: RuntimeErrorMark[] = [];
    for (const report of errors.value) {
        // Parse / analyze problems are source-level — lint's red squiggle
        // is the right vocabulary. Eval / projection problems happen at
        // runtime against valid syntax; we route them through a separate
        // decoration so the line gets a soft highlight + a gutter dot
        // instead of a "syntax error" squiggle.
        const isRuntime =
            report.kind === 'EvalError' || report.kind === 'ProjectionError';
        for (const span of report.spans) {
            if (span.file !== activeFile.value) continue;
            // Clamp to current doc length; a recently edited buffer can
            // drift past the offsets the analyzer saw.
            const from = Math.min(span.start, docLen);
            const to = Math.min(span.end > span.start ? span.end : span.start + 1, docLen);
            const message = span.label
                ? `${span.label}: ${report.message}`
                : report.message;
            if (isRuntime) {
                runtimeMarks.push({
                    line: view.state.doc.lineAt(from).number,
                    message,
                });
            } else {
                diagnostics.push({
                    from,
                    to,
                    severity: 'error',
                    message,
                    source: report.code ?? report.kind,
                });
            }
        }
    }
    view.dispatch(setDiagnostics(view.state, diagnostics));
    view.dispatch({ effects: setRuntimeErrors.of(runtimeMarks) });
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

function startResize(e: PointerEvent) {
    const panes = panesEl.value;
    if (!panes) return;
    const total = panes.clientWidth;
    if (total <= 0) return;
    e.preventDefault();
    const target = e.currentTarget as HTMLElement;
    target.setPointerCapture(e.pointerId);
    const startX = e.clientX;
    const startW = editorWidthPct.value;

    const onMove = (ev: PointerEvent) => {
        const dx = ev.clientX - startX;
        const next = startW + (dx / total) * 100;
        editorWidthPct.value = Math.min(80, Math.max(20, next));
    };
    const stop = () => {
        try { target.releasePointerCapture(e.pointerId); } catch { /* already released */ }
        target.removeEventListener('pointermove', onMove);
        target.removeEventListener('pointerup', stop);
        target.removeEventListener('pointercancel', stop);
    };
    target.addEventListener('pointermove', onMove);
    target.addEventListener('pointerup', stop);
    target.addEventListener('pointercancel', stop);
}

function startErrorsResize(e: PointerEvent) {
    // Drag is only meaningful while the panel is open. Closed → no-op,
    // so a stray pointerdown on the title bar doesn't visually do
    // anything. The +/- button has its own @pointerdown.stop.
    if (!errorsExpanded.value) return;
    e.preventDefault();
    const target = e.currentTarget as HTMLElement;
    target.setPointerCapture(e.pointerId);
    const startY = e.clientY;
    const startH = errorsHeight.value;

    const onMove = (ev: PointerEvent) => {
        // Dragging up (negative dy) grows the panel.
        const next = startH - (ev.clientY - startY);
        const ceiling = Math.max(120, window.innerHeight * 0.75);
        errorsHeight.value = Math.min(ceiling, Math.max(80, next));
    };
    const stop = () => {
        try { target.releasePointerCapture(e.pointerId); } catch { /* already released */ }
        target.removeEventListener('pointermove', onMove);
        target.removeEventListener('pointerup', stop);
        target.removeEventListener('pointercancel', stop);
    };
    target.addEventListener('pointermove', onMove);
    target.addEventListener('pointerup', stop);
    target.addEventListener('pointercancel', stop);
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
            runtimeErrorExtension,
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
      <label class="rp-preset">
        <span class="rp-preset-label">Example</span>
        <select
          class="rp-preset-select"
          :value="presetId"
          @change="loadPreset(($event.target as HTMLSelectElement).value)"
        >
          <option v-for="p in PRESETS" :key="p.id" :value="p.id">{{ p.label }}</option>
        </select>
      </label>
      <span class="rp-status-text">{{ status }}</span>
      <span class="rp-status-spacer" />
      <label v-if="!activePreset.runnableInSandbox" class="rp-args">
        <span class="rp-args-label">Args</span>
        <input
          v-model="argsInput"
          class="rp-args-input"
          type="text"
          spellcheck="false"
          autocomplete="off"
          autocorrect="off"
          autocapitalize="off"
          placeholder='{"...": ...}'
          @keydown.enter.prevent="runWithArgs"
        />
      </label>
      <button
        v-if="!activePreset.runnableInSandbox"
        class="rp-action rp-run"
        :disabled="!isReady"
        title="Evaluate with the args JSON above"
        @click="runWithArgs"
      >Run</button>
    </header>

    <div
      ref="panesEl"
      class="rp-panes"
      :style="{ '--rp-editor-w': editorWidthPct + '%' }"
    >
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
          <button class="rp-tab rp-tab-add" title="Add a new file" @click="openNewFileDialog">+</button>
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

      <div
        class="rp-resizer"
        role="separator"
        aria-orientation="vertical"
        :aria-valuenow="Math.round(editorWidthPct)"
        aria-valuemin="20"
        aria-valuemax="80"
        title="Drag to resize"
        @pointerdown="startResize"
      ></div>

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

    <section
      class="rp-errors"
      :class="{ 'is-open': errorsExpanded }"
      :style="errorsExpanded ? { height: errorsHeight + 'px' } : undefined"
    >
      <div
        class="rp-errors-head"
        :class="{ 'is-draggable': errorsExpanded }"
        :title="errorsExpanded ? 'Drag to resize' : undefined"
        @pointerdown="startErrorsResize"
      >
        <span class="rp-errors-title">Errors ({{ errorCount }})</span>
        <button
          type="button"
          class="rp-errors-toggle-btn"
          :aria-expanded="errorsExpanded"
          :aria-label="errorsExpanded ? 'Collapse errors' : 'Expand errors'"
          @click="errorsExpanded = !errorsExpanded"
          @pointerdown.stop
        >{{ errorsExpanded ? '−' : '+' }}</button>
      </div>
      <div v-if="errorsExpanded" class="rp-errors-body">
        <div
          v-if="!activePreset.runnableInSandbox && activePreset.note"
          class="rp-errors-context"
          role="note"
        >
          <span class="rp-errors-context-label">Why is this failing?</span>
          <span class="rp-errors-context-text">{{ activePreset.note }}</span>
        </div>
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
      </div>
    </section>

    <dialog ref="newFileDialog" class="rp-dialog" @close="newFileError = ''">
      <form class="rp-dialog-form" @submit.prevent="confirmNewFile">
        <h3 class="rp-dialog-title">New file</h3>
        <label class="rp-dialog-row">
          <span class="rp-dialog-label">Path</span>
          <input
            v-model="newFilePath"
            class="rp-dialog-input"
            type="text"
            placeholder="lib.relon"
            autocomplete="off"
            @keydown.esc.prevent="closeNewFileDialog"
          />
        </label>
        <p v-if="newFileError" class="rp-dialog-error">{{ newFileError }}</p>
        <p class="rp-dialog-hint">
          If you omit the extension, <code>.relon</code> is appended.
        </p>
        <div class="rp-dialog-actions">
          <button type="button" class="rp-dialog-btn" @click="closeNewFileDialog">Cancel</button>
          <button type="submit" class="rp-dialog-btn rp-dialog-btn-primary">Create</button>
        </div>
      </form>
    </dialog>
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
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 4px 12px;
  background: var(--vp-c-bg-soft);
  border-bottom: 1px solid var(--vp-c-divider);
  color: var(--vp-c-text-2);
  font-size: 12px;
}

.rp-preset {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.rp-preset-label {
  color: var(--vp-c-text-3);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}

.rp-preset-select {
  padding: 1px 6px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 3px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  font-size: 12px;
  font-family: var(--vp-font-family-mono);
}

.rp-status-text { color: var(--vp-c-text-3); }

.rp-status-spacer {
  flex: 1 1 auto;
  min-width: 8px;
}

.rp-args {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.rp-args-label {
  color: var(--vp-c-text-3);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}

.rp-args-input {
  width: 280px;
  padding: 2px 8px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 4px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  font-family: var(--vp-font-family-mono);
  font-size: 11px;
  line-height: 1.6;
  outline: none;
}

.rp-args-input:focus {
  border-color: var(--vp-c-brand-1, #6470ff);
}

.rp-run {
  /* Inherits `.rp-action`; this just nudges visual weight. */
  font-weight: 600;
}

.rp-panes {
  display: flex;
  flex-direction: row;
  flex: 1 1 auto;
  min-height: 0;
  overflow: hidden;
}

.rp-pane {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}

.rp-pane-editor {
  flex: 0 0 var(--rp-editor-w, 50%);
  min-width: 200px;
}

.rp-pane-output {
  flex: 1 1 0;
  min-width: 200px;
}

.rp-resizer {
  flex: 0 0 6px;
  align-self: stretch;
  cursor: col-resize;
  background: transparent;
  border-left: 1px solid var(--vp-c-divider);
  border-right: 1px solid transparent;
  margin: 0 -3px;
  z-index: 1;
  touch-action: none;
  transition: background-color 120ms ease;
}

.rp-resizer:hover,
.rp-resizer:active {
  background: var(--vp-c-brand-soft, rgba(100, 108, 255, 0.18));
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

/* Runtime errors: line-background + breakpoint-style gutter dot.
   Distinct from lint's red squiggle so users don't read evaluation
   failures as syntax errors. */
.rp-editor :deep(.cm-relon-runtime-line) {
  background: var(--vp-c-danger-soft, rgba(229, 83, 91, 0.08));
}

.rp-editor :deep(.cm-relon-runtime-gutter) {
  width: 12px;
}

.rp-editor :deep(.cm-relon-runtime-dot) {
  display: block;
  width: 7px;
  height: 7px;
  margin: 6px auto 0;
  border-radius: 50%;
  background: var(--vp-c-danger-1, #e0535b);
  opacity: 0.78;
  cursor: help;
}

/* Errors dock — Chrome devtools style. Collapsed: just the header.
   Open: header + draggable top edge + scrollable body sized to
   `errorsHeight`. The dock never pushes the editor off-screen because
   the body owns its own overflow. */
.rp-errors {
  border-top: 2px solid var(--vp-c-divider);
  background: var(--vp-c-bg-alt, var(--vp-c-bg-soft));
  box-shadow: 0 -1px 4px rgba(0, 0, 0, 0.05);
  font-size: 12px;
  flex: 0 0 auto;
  display: flex;
  flex-direction: column;
}

.rp-errors.is-open {
  /* Height comes from inline style bound to `errorsHeight`. */
  min-height: 0;
  overflow: hidden;
}

/* Title bar doubles as the drag handle when the panel is open. */
.rp-errors-head {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 2px 8px 2px 12px;
  min-height: 24px;
  color: var(--vp-c-text-1);
  font-size: 12px;
  line-height: 1;
  user-select: none;
  flex: 0 0 auto;
  touch-action: none;
}

.rp-errors-head.is-draggable {
  cursor: row-resize;
}

.rp-errors-head.is-draggable:hover,
.rp-errors-head.is-draggable:active {
  background: var(--vp-c-default-soft);
}

.rp-errors-title {
  font-weight: 500;
}

.rp-errors-toggle-btn {
  margin-left: auto;
  border: none;
  background: transparent;
  color: var(--vp-c-text-2);
  font-size: 20px;
  font-weight: 500;
  line-height: 1;
  padding: 0 4px;
  cursor: pointer;
}

.rp-errors-toggle-btn:hover {
  color: var(--vp-c-text-1);
}

.rp-errors-body {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  padding: 0 12px 12px;
}

.rp-errors-context {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0 0 10px;
  padding: 8px 10px;
  border: 1px solid var(--vp-c-divider);
  border-left: 3px solid var(--vp-c-warning-1, #f4b400);
  border-radius: 4px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  line-height: 1.5;
}

.rp-errors-context-label {
  font-weight: 600;
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--vp-c-text-2);
}

.rp-errors-context-text {
  white-space: pre-wrap;
  word-break: break-word;
}

.rp-errors-context-text :deep(code),
.rp-errors-context-text code {
  font-family: var(--vp-font-family-mono);
  font-size: 11px;
  background: var(--vp-c-bg-soft);
  padding: 0 4px;
  border-radius: 3px;
}

.rp-errors-list {
  list-style: none;
  margin: 0;
  padding: 4px 0 0;
}

.rp-errors-empty {
  padding: 8px 0;
  color: var(--vp-c-text-3);
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

.rp-dialog {
  border: 1px solid var(--vp-c-divider);
  border-radius: 6px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  padding: 0;
  min-width: 320px;
  max-width: 480px;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.18);
}

.rp-dialog::backdrop {
  background: rgba(0, 0, 0, 0.35);
}

.rp-dialog-form {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
  font-size: 13px;
}

.rp-dialog-title {
  margin: 0;
  font-size: 14px;
  font-weight: 600;
}

.rp-dialog-row {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.rp-dialog-label {
  color: var(--vp-c-text-2);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}

.rp-dialog-input {
  padding: 4px 8px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 4px;
  background: var(--vp-c-bg-soft);
  color: var(--vp-c-text-1);
  font-family: var(--vp-font-family-mono);
  font-size: 13px;
}

.rp-dialog-input:focus {
  outline: 2px solid var(--vp-c-brand-1, #3eaf7c);
  outline-offset: -1px;
}

.rp-dialog-error {
  margin: 0;
  color: var(--vp-c-danger-1, #b8333a);
  font-size: 12px;
}

.rp-dialog-hint {
  margin: 0;
  color: var(--vp-c-text-3);
  font-size: 11px;
}

.rp-dialog-hint code {
  font-family: var(--vp-font-family-mono);
  background: var(--vp-c-bg-soft);
  padding: 0 4px;
  border-radius: 3px;
}

.rp-dialog-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}

.rp-dialog-btn {
  padding: 4px 12px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 4px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  cursor: pointer;
  font-size: 12px;
}

.rp-dialog-btn:hover { background: var(--vp-c-default-soft); }

.rp-dialog-btn-primary {
  background: var(--vp-c-brand-1, #3eaf7c);
  border-color: var(--vp-c-brand-1, #3eaf7c);
  color: #ffffff;
}

.rp-dialog-btn-primary:hover {
  background: var(--vp-c-brand-2, #379469);
}

@media (max-width: 768px) {
  .rp-panes {
    flex-direction: column;
  }
  .rp-pane-editor {
    flex: 1 1 50%;
    min-height: 200px;
    border-bottom: 1px solid var(--vp-c-divider);
  }
  .rp-pane-output {
    flex: 1 1 50%;
    min-height: 200px;
  }
  /* Horizontal drag isn't useful in the vertical mobile layout. */
  .rp-resizer {
    display: none;
  }
}
</style>
