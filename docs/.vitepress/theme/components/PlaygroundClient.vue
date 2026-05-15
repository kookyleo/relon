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
import { useData, withBase } from 'vitepress';
// Pull the actual VitePress home-page appearance switch in so the pill
// toggle (sliding knob + sun/moon crossfade) is pixel-identical, and
// View Transitions / localStorage persistence behave the same way as
// when the user flips theme on any other docs page. Internal import,
// but the path is stable across VitePress 1.x.
// Stock VitePress nav-cluster components — same instances the home
// page renders. Importing them directly keeps the playground's right
// rail in perfect sync with the docs nav: language dropdown, theme
// pill, GitHub link all share `useData()` state and respect the same
// theme.nav / theme.socialLinks config.
import VPNavBarMenu from 'vitepress/dist/client/theme-default/components/VPNavBarMenu.vue';
import VPNavBarTranslations from 'vitepress/dist/client/theme-default/components/VPNavBarTranslations.vue';
import VPNavBarAppearance from 'vitepress/dist/client/theme-default/components/VPNavBarAppearance.vue';
import VPNavBarSocialLinks from 'vitepress/dist/client/theme-default/components/VPNavBarSocialLinks.vue';

import { EditorState, Compartment } from '@codemirror/state';
import { EditorView, keymap, lineNumbers, highlightActiveLine } from '@codemirror/view';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, indentOnInput, syntaxHighlighting, foldGutter, foldKeymap } from '@codemirror/language';
import { setDiagnostics, type Diagnostic } from '@codemirror/lint';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { searchKeymap, highlightSelectionMatches } from '@codemirror/search';
import { json as jsonLang } from '@codemirror/lang-json';

import { relonLanguage, playgroundHighlightStyle } from './playground/relon-mode';
import { viewportPad, unpaddedContent, isPaddingUpdate } from './playground/viewport-pad';
import { PRESETS, DEFAULT_PRESET_ID, type Preset } from './playground/presets';
import {
    runtimeErrorExtension,
    setRuntimeErrors,
    type RuntimeErrorMark,
} from './playground/runtime-errors';
import { gotoDefinitionExtension } from './playground/goto-def';
import { relonAutocomplete, type RelonCompletion } from './playground/autocomplete';

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
    goto_definition: (
        sources: unknown,
        entry: string,
        line: number,
        character: number
    ) => GotoDefinitionResult | null;
    complete: (
        sources: unknown,
        entry: string,
        line: number,
        character: number
    ) => RelonCompletion[];
}

export interface GotoDefinitionResult {
    path: string;
    start: { line: number; character: number };
    end: { line: number; character: number };
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

// ---------------- user workspaces (custom, persisted) -------------------
//
// In addition to the canned PRESETS, users can carve out one or more
// named scratch workspaces and have them survive a page reload. The
// data model intentionally mirrors a preset (files + entry + args) so
// switching between a preset and a workspace is a single uniform
// `files.value = …` swap; only the "should edits write back?" rule
// differs (workspaces persist via the `watch(files, …)` below;
// presets do not, since `PRESETS` is bundled constant data).

interface Workspace {
    id: string;
    name: string;
    files: PlaygroundFile[];
    entry: string;
    args: string;
}

const WORKSPACES_STORAGE_KEY = 'relon-playground-workspaces-v1';
const workspaces = ref<Workspace[]>([]);
const activeWorkspaceId = ref<string>('');
const sourceMode = ref<'preset' | 'workspace'>('preset');

const activeWorkspace = computed<Workspace | null>(() =>
    sourceMode.value === 'workspace'
        ? (workspaces.value.find((w) => w.id === activeWorkspaceId.value) ?? null)
        : null
);

// JS-implemented source picker. The native `<select>` had two problems:
// (a) styling the open menu is impossible cross-platform, so action
// rows like `New…` and per-item delete affordances couldn't share the
// item's row, and (b) it forced an artificial Examples/Workspaces
// optgroup split that read like a feature instead of just "the list
// of things you can pick". This custom flyout flattens everything to
// one list where presets and user workspaces sit together; user
// items just additionally render a `−` delete control on the right.
const sourceMenuOpen = ref<boolean>(false);
const sourceMenuRoot = ref<HTMLElement | null>(null);

const activeSourceLabel = computed<string>(() => {
    if (sourceMode.value === 'workspace' && activeWorkspace.value) return activeWorkspace.value.name;
    return activePreset.value.label;
});

function toggleSourceMenu() { sourceMenuOpen.value = !sourceMenuOpen.value; }
function closeSourceMenu() { sourceMenuOpen.value = false; }

function pickPreset(id: string) { loadPreset(id); closeSourceMenu(); }
function pickWorkspace(id: string) { selectWorkspace(id); closeSourceMenu(); }
function pickNew() { closeSourceMenu(); openNewWorkspaceDialog(); }

function onDeleteWorkspaceFromMenu(id: string, ev: MouseEvent) {
    ev.stopPropagation();
    const ws = workspaces.value.find((w) => w.id === id);
    if (!ws) return;
    if (typeof window !== 'undefined' && typeof window.confirm === 'function') {
        if (!window.confirm(`Delete workspace "${ws.name}"? This cannot be undone.`)) return;
    }
    const wasActive = activeWorkspaceId.value === id;
    workspaces.value = workspaces.value.filter((w) => w.id !== id);
    persistWorkspaces();
    if (wasActive) loadPreset(presetId.value);
}

function onDocumentClick(ev: MouseEvent) {
    if (!sourceMenuOpen.value) return;
    const root = sourceMenuRoot.value;
    if (!root) return;
    if (ev.target instanceof Node && root.contains(ev.target)) return;
    closeSourceMenu();
}

function onDocumentKey(ev: KeyboardEvent) {
    if (sourceMenuOpen.value && ev.key === 'Escape') closeSourceMenu();
}

function loadStoredWorkspaces() {
    if (typeof localStorage === 'undefined') return;
    try {
        const raw = localStorage.getItem(WORKSPACES_STORAGE_KEY);
        if (!raw) return;
        const parsed = JSON.parse(raw);
        if (!Array.isArray(parsed)) return;
        // Light validation — drop obviously malformed entries instead
        // of crashing the playground when the schema rolls forward.
        workspaces.value = parsed.filter((w) =>
            w && typeof w.id === 'string' && typeof w.name === 'string' && Array.isArray(w.files)
        );
    } catch { /* corrupted storage — fall back to empty list */ }
}

function persistWorkspaces() {
    if (typeof localStorage === 'undefined') return;
    try {
        localStorage.setItem(WORKSPACES_STORAGE_KEY, JSON.stringify(workspaces.value));
    } catch { /* quota or private-mode browser — silently ignore */ }
}

// New-workspace modal state.
const newWorkspaceDialog = ref<HTMLDialogElement | null>(null);
const newWorkspaceName = ref<string>('');
const newWorkspaceError = ref<string>('');

// New-file modal state. We avoid `window.prompt` because it blocks the
// event loop, can't be themed, and on some browsers (notably mobile
// Safari) it's been quietly removed. Native `<dialog>` is in every
// evergreen target VitePress supports today.
const newFileDialog = ref<HTMLDialogElement | null>(null);
const newFilePath = ref<string>('');
const newFileError = ref<string>('');

// Args modal state. Authoring multi-line JSON inside a single-row
// `<input>` is hostile; clicking the inline Args field pops a modal
// with a pretty-printed textarea, and the inline field shows the
// compacted (one-line, no whitespace) projection of whatever the user
// committed. Invalid JSON survives round-trip — we don't lose what the
// user typed — but it stays surfaced verbatim in the inline field so
// the broken state is visible at a glance.
const argsDialog = ref<HTMLDialogElement | null>(null);
const argsDraft = ref<string>('');
const argsDraftError = ref<string>('');
// The args modal's editing surface is a real CodeMirror instance —
// the textarea it replaced couldn't render JSON syntax colours, line
// numbers, or proper bracket matching, all of which matter when
// users are pasting realistic 30-line argument payloads.
const argsEditorHost = ref<HTMLElement | null>(null);
const argsEditorView = shallowRef<EditorView | null>(null);

// Top-bar toolbox.
//   - `autoRun` gates the `scheduleEvaluate` call inside the editor's
//     update listener; flipping it off freezes the JSON pane until the
//     user hits Run (or toggles it back on).
//   - Guide link derives from the current locale segment so the
//     standalone playground hands the user back into the right docs
//     tree they came from.
//   - Theme toggle is the actual `VPSwitchAppearance` component pulled
//     from the default theme — nothing to wire here; it shares
//     `isDark` + the `toggle-appearance` inject the docs pages use.
const autoRun = ref<boolean>(true);
// `useData()` is consumed by the stock VitePress nav-cluster
// components below; we just need to ensure the call happens inside
// `setup()` of this component so the locale + theme.nav / socialLinks
// reactive data is available when they mount.
useData();

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

// Compact (single-line, no whitespace) projection of the args JSON for
// the inline field. Empty inputs stay empty. Parse failures fall back
// to the raw text so the broken state is still visible — silently
// hiding malformed JSON behind a sanitised display would let the user
// click Run on garbage with no warning until evaluate barfs.
const argsCompact = computed<string>(() => {
    const raw = argsInput.value.trim();
    if (!raw) return '';
    try {
        return JSON.stringify(JSON.parse(raw));
    } catch {
        return raw;
    }
});

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

function openArgsDialog() {
    // Pretty-print whatever the user committed so multi-line editing
    // feels natural. If the stored value won't parse we still let them
    // edit it raw — fixing malformed JSON is half the reason this
    // modal exists.
    const raw = argsInput.value.trim();
    if (raw) {
        try {
            argsDraft.value = JSON.stringify(JSON.parse(raw), null, 2);
        } catch {
            argsDraft.value = raw;
        }
    } else {
        argsDraft.value = '';
    }
    argsDraftError.value = '';
    const dlg = argsDialog.value;
    if (!dlg) return;
    if (typeof dlg.showModal === 'function') {
        dlg.showModal();
    } else {
        dlg.setAttribute('open', '');
    }
    // Wait for the dialog's content layer to mount before constructing
    // the editor — `<dialog>` defers child rendering until showModal()
    // resolves, so synchronous CM mount races the host being null.
    queueMicrotask(() => {
        const host = argsEditorHost.value;
        if (!host) return;
        argsEditorView.value?.destroy();
        const state = EditorState.create({
            doc: argsDraft.value,
            extensions: [
                lineNumbers(),
                highlightActiveLine(),
                history(),
                bracketMatching(),
                indentOnInput(),
                jsonLang(),
                syntaxHighlighting(playgroundHighlightStyle, { fallback: true }),
                EditorView.lineWrapping,
                keymap.of([
                    ...defaultKeymap,
                    ...historyKeymap,
                    indentWithTab,
                    // Save shortcut. `Mod` is Cmd on Mac, Ctrl elsewhere.
                    { key: 'Mod-Enter', run: () => { confirmArgs(); return true; } },
                ]),
                // Sync edits back into the model so `confirmArgs` can
                // read from `argsDraft` directly (matches the textarea
                // contract the v-model used to provide).
                EditorView.updateListener.of((u) => {
                    if (u.docChanged) argsDraft.value = u.state.doc.toString();
                }),
            ],
        });
        argsEditorView.value = new EditorView({ state, parent: host });
        argsEditorView.value.focus();
    });
}

function closeArgsDialog() {
    argsEditorView.value?.destroy();
    argsEditorView.value = null;
    const dlg = argsDialog.value;
    if (!dlg) return;
    if (typeof dlg.close === 'function') {
        dlg.close();
    } else {
        dlg.removeAttribute('open');
    }
}

// Unified Run entrypoint behind the toolbar button. Any non-empty
// `argsInput` routes through `runWithArgs` so the committed JSON is
// honoured — regardless of whether the source is a sandbox preset or
// a user workspace. Empty args fall back to the no-arg `runEvaluate`
// path (which is also what auto-run uses on every edit).
function runActive() {
    if (argsInput.value.trim()) {
        void runWithArgs();
    } else {
        void runEvaluate();
    }
}

function confirmArgs() {
    // Validate parse-ability so the inline field can confidently show
    // the compact projection. Empty draft means "no args" and is fine.
    const raw = argsDraft.value.trim();
    if (raw) {
        try {
            JSON.parse(raw);
        } catch (err) {
            argsDraftError.value = `Invalid JSON: ${(err as Error).message}`;
            return;
        }
    }
    argsInput.value = raw;
    argsDraftError.value = '';
    closeArgsDialog();
}

function loadPreset(id: string) {
    const preset = PRESETS.find((p) => p.id === id);
    if (!preset) return;
    sourceMode.value = 'preset';
    activeWorkspaceId.value = '';
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

function selectWorkspace(id: string) {
    const ws = workspaces.value.find((w) => w.id === id);
    if (!ws) return;
    sourceMode.value = 'workspace';
    activeWorkspaceId.value = id;
    argsInput.value = ws.args ?? '';
    files.value = ws.files.map((f) => ({ path: f.path, content: f.content }));
    entry.value = ws.entry || ws.files[0]?.path || 'main.relon';
    activeFile.value = files.value[0]?.path ?? 'main.relon';
    const view = editorView.value;
    if (view && files.value[0]) {
        view.dispatch({
            changes: { from: 0, to: view.state.doc.length, insert: files.value[0].content },
        });
    }
    void runEvaluate();
}

function openNewWorkspaceDialog() {
    newWorkspaceName.value = `workspace ${workspaces.value.length + 1}`;
    newWorkspaceError.value = '';
    const dlg = newWorkspaceDialog.value;
    if (!dlg) return;
    if (typeof dlg.showModal === 'function') dlg.showModal();
    else dlg.setAttribute('open', '');
    queueMicrotask(() => {
        const input = dlg.querySelector('input');
        if (input) {
            (input as HTMLInputElement).focus();
            (input as HTMLInputElement).select();
        }
    });
}

function closeNewWorkspaceDialog() {
    const dlg = newWorkspaceDialog.value;
    if (!dlg) return;
    if (typeof dlg.close === 'function') dlg.close();
    else dlg.removeAttribute('open');
}

function confirmNewWorkspace() {
    const name = newWorkspaceName.value.trim();
    if (!name) {
        newWorkspaceError.value = 'Name cannot be empty.';
        return;
    }
    if (workspaces.value.some((w) => w.name === name)) {
        newWorkspaceError.value = `A workspace named "${name}" already exists.`;
        return;
    }
    const id = `ws-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
    workspaces.value.push({
        id,
        name,
        files: [{ path: 'main.relon', content: '{\n    \n}\n' }],
        entry: 'main.relon',
        args: '',
    });
    persistWorkspaces();
    closeNewWorkspaceDialog();
    selectWorkspace(id);
}

function deleteActiveWorkspace() {
    const ws = activeWorkspace.value;
    if (!ws) return;
    if (typeof window !== 'undefined' && typeof window.confirm === 'function') {
        if (!window.confirm(`Delete workspace "${ws.name}"? This cannot be undone.`)) return;
    }
    workspaces.value = workspaces.value.filter((w) => w.id !== ws.id);
    persistWorkspaces();
    // Fall back to the current (or default) preset so the editor never
    // sits on an orphaned source.
    loadPreset(presetId.value);
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
    if (!mod) return;
    // Format the entire workspace, not just the active tab — every
    // file's text gets normalised in one pass. We use the editor's
    // live buffer for the active file (so uncommitted edits round-
    // trip cleanly) and `files.value` for the rest.
    const view = editorView.value;
    const liveActive = view ? view.state.doc.toString() : null;
    const formattedByPath: Record<string, string> = {};
    const failures: ErrorReport[] = [];
    for (const f of files.value) {
        const source = f.path === activeFile.value && liveActive != null
            ? liveActive
            : f.content;
        try {
            formattedByPath[f.path] = mod.format(source);
        } catch (raw) {
            failures.push(coerceErrorReport(raw));
        }
    }
    if (failures.length > 0) {
        // Don't apply a partial format — surface the first failure
        // and leave every file as-is. Clearer than mixed state.
        errors.value = failures;
        errorsExpanded.value = true;
        return;
    }
    // Apply: update in-memory file contents first, then swap the
    // editor doc for the active file (whose change fires the
    // updateListener → model write-back path).
    for (const f of files.value) {
        const next = formattedByPath[f.path];
        if (next != null) f.content = next;
    }
    if (view) {
        const activeText = formattedByPath[activeFile.value];
        if (activeText != null) {
            view.dispatch({
                changes: { from: 0, to: view.state.doc.length, insert: activeText },
            });
        }
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

// Dialog dragging state: we track translation offsets per-dialog
// instance so they "remember" where they were last dragged during a
// single session. Map keys are the dialog elements themselves.
const dialogOffsets = new Map<HTMLElement, { x: number; y: number }>();

function startDialogDrag(e: PointerEvent) {
    // The handle is the h3 title, but we move the parent <dialog>.
    const dialog = (e.currentTarget as HTMLElement).closest('dialog');
    if (!dialog) return;

    e.preventDefault();
    const handle = e.currentTarget as HTMLElement;
    handle.setPointerCapture(e.pointerId);

    if (!dialogOffsets.has(dialog)) {
        dialogOffsets.set(dialog, { x: 0, y: 0 });
    }
    const offset = dialogOffsets.get(dialog)!;
    const startX = e.clientX - offset.x;
    const startY = e.clientY - offset.y;

    const onMove = (ev: PointerEvent) => {
        offset.x = ev.clientX - startX;
        offset.y = ev.clientY - startY;
        dialog.style.transform = `translate(${offset.x}px, ${offset.y}px)`;
    };
    const stop = () => {
        try { handle.releasePointerCapture(e.pointerId); } catch { /* already released */ }
        handle.removeEventListener('pointermove', onMove);
        handle.removeEventListener('pointerup', stop);
        handle.removeEventListener('pointercancel', stop);
    };
    handle.addEventListener('pointermove', onMove);
    handle.addEventListener('pointerup', stop);
    handle.addEventListener('pointercancel', stop);
}

/// Run the workspace-aware goto-definition lookup at the cursor's
/// (line, character) and return the target the wasm layer found, or
/// null. Called by the CodeMirror gotoDef extension on Mod-hover and
/// Mod-click — must be cheap (synchronous, ≤ a few ms).
function resolveGotoDef(line: number, character: number): GotoDefinitionResult | null {
    const mod = wasmRef.value;
    if (!mod) return null;
    const sources = files.value.map((f) => ({ path: f.path, content: f.content }));
    try {
        return mod.goto_definition(sources, activeFile.value, line, character);
    } catch {
        // Defensive: a transient parse error or workspace cycle is a
        // perfectly normal state for an editing session — don't crash
        // the editor over it.
        return null;
    }
}

/// Run the wasm-side completion resolver at the cursor's (line,
/// character) and return the candidates. Called by the CodeMirror
/// autocomplete extension on every keystroke; must be synchronous.
/// Bails to an empty list on workspace errors so the editor keeps
/// behaving sanely while the user is mid-edit.
function resolveCompletions(line: number, character: number): RelonCompletion[] {
    const mod = wasmRef.value;
    if (!mod) return [];
    const sources = files.value.map((f) => ({ path: f.path, content: f.content }));
    try {
        return mod.complete(sources, activeFile.value, line, character);
    } catch {
        return [];
    }
}

/// Apply a goto-definition target: switch to the right tab (for
/// cross-file jumps) and move the editor's selection to the target's
/// range. The range covers the *key* of the resolved field (VS Code
/// convention — clicking lib.shout jumps to the destination and
/// highlights "shout"). Collapsed ranges (start == end) — e.g. cursor
/// on `&root` — degrade to a plain caret.
function applyGotoDef(target: GotoDefinitionResult) {
    if (target.path !== activeFile.value) {
        selectFile(target.path);
    }
    const view = editorView.value;
    if (!view) return;
    const startLineNum = target.start.line + 1;
    if (startLineNum < 1 || startLineNum > view.state.doc.lines) return;
    const startLine = view.state.doc.line(startLineNum);
    const startOffset = Math.min(startLine.from + target.start.character, startLine.to);
    const endLineNum = target.end.line + 1;
    const endLine =
        endLineNum >= 1 && endLineNum <= view.state.doc.lines
            ? view.state.doc.line(endLineNum)
            : startLine;
    const endOffset = Math.min(endLine.from + target.end.character, endLine.to);
    view.dispatch({
        selection: { anchor: startOffset, head: endOffset },
        scrollIntoView: true,
    });
    view.focus();
}

onMounted(() => {
    if (!editorHost.value) return;
    const startDoc = activeFileObj.value?.content ?? '';

    const updateListener = EditorView.updateListener.of((v) => {
        if (!v.docChanged) return;
        // Skip our own viewport-pad maintenance: those transactions only
        // append/remove trailing `\n`s for gutter-fill, and surfacing
        // them as "user edits" would trigger redundant evaluates.
        if (isPaddingUpdate(v)) return;
        updateActiveContent(unpaddedContent(v.view));
        if (autoRun.value) scheduleEvaluate();
    });

    const state = EditorState.create({
        doc: startDoc,
        extensions: [
            lineNumbers(),
            foldGutter(),
            highlightActiveLine(),
            highlightSelectionMatches(),
            history(),
            bracketMatching(),
            closeBrackets(),
            indentOnInput(),
            runtimeErrorExtension,
            keymap.of([
                ...closeBracketsKeymap,
                ...defaultKeymap,
                ...historyKeymap,
                ...foldKeymap,
                ...searchKeymap,
                indentWithTab,
                // Format the current buffer with relon-fmt. Mod-S keeps
                // Cmd-S from doing nothing in this no-server playground
                // and matches the "save = canonicalise" convention.
                {
                    key: 'Mod-s',
                    preventDefault: true,
                    run: () => { runFormat(); return true; },
                },
                {
                    key: 'Mod-Shift-f',
                    preventDefault: true,
                    run: () => { runFormat(); return true; },
                },
            ]),
            langCompartment.of(relonLanguage()),
            gotoDefinitionExtension({
                resolve: resolveGotoDef,
                jump: applyGotoDef,
            }),
            relonAutocomplete(resolveCompletions),
            EditorView.lineWrapping,
            viewportPad,
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
                // Share the relon palette so the JSON pane recolours with the
                // VitePress theme; `defaultHighlightStyle` is a fixed light
                // palette and looks bad in dark mode.
                syntaxHighlighting(playgroundHighlightStyle, { fallback: true }),
                EditorView.lineWrapping,
                viewportPad,
            ],
        });
        jsonView.value = new EditorView({ state: jsonState, parent: jsonHost.value });
    }

    loadStoredWorkspaces();
    if (typeof window !== 'undefined') {
        window.addEventListener('mousedown', onDocumentClick);
        window.addEventListener('keydown', onDocumentKey);
    }
    void bootWasm();
});

onBeforeUnmount(() => {
    if (evalTimer !== null) clearTimeout(evalTimer);
    editorView.value?.destroy();
    jsonView.value?.destroy();
    if (typeof window !== 'undefined') {
        window.removeEventListener('mousedown', onDocumentClick);
        window.removeEventListener('keydown', onDocumentKey);
    }
});

// Keep entry highlight in sync when files mutate.
watch(files, () => {
    if (!files.value.some((f) => f.path === entry.value)) {
        entry.value = files.value[0]?.path ?? '';
    }
}, { deep: true });

// Persist workspace edits whenever the file set or entry pointer
// changes. Presets are read-only by design, so we gate on
// `sourceMode === 'workspace'` here.
watch([files, entry, argsInput], () => {
    if (sourceMode.value !== 'workspace') return;
    const ws = workspaces.value.find((w) => w.id === activeWorkspaceId.value);
    if (!ws) return;
    ws.files = files.value.map((f) => ({ path: f.path, content: f.content }));
    ws.entry = entry.value;
    ws.args = argsInput.value;
    persistWorkspaces();
}, { deep: true });
</script>

<template>
  <div class="relon-playground">
    <header class="rp-status">
      <a class="rp-brand" href="../" aria-label="Relon home">
        <img class="rp-brand-logo" :src="withBase('/logo-mini.svg')" alt="" />
        <span class="rp-brand-name">Relon</span>
      </a>
      <div ref="sourceMenuRoot" class="rp-source-wrap">
        <button
          type="button"
          class="rp-source"
          :aria-expanded="sourceMenuOpen"
          aria-haspopup="listbox"
          @click="toggleSourceMenu"
        >
          <span class="rp-source-current">{{ activeSourceLabel }}</span>
          <span class="rp-source-caret" aria-hidden="true">▾</span>
        </button>
        <ul v-if="sourceMenuOpen" class="rp-source-menu" role="listbox">
          <li
            v-for="p in PRESETS"
            :key="`p:${p.id}`"
            role="option"
            :aria-selected="sourceMode === 'preset' && presetId === p.id"
            class="rp-source-item"
            :class="{ 'is-active': sourceMode === 'preset' && presetId === p.id }"
            @click="pickPreset(p.id)"
          >
            <span class="rp-source-item-label">{{ p.label }}</span>
          </li>
          <li
            v-for="w in workspaces"
            :key="`w:${w.id}`"
            role="option"
            :aria-selected="sourceMode === 'workspace' && activeWorkspaceId === w.id"
            class="rp-source-item rp-source-item-ws"
            :class="{ 'is-active': sourceMode === 'workspace' && activeWorkspaceId === w.id }"
            @click="pickWorkspace(w.id)"
          >
            <span class="rp-source-item-label">{{ w.name }}</span>
            <button
              type="button"
              class="rp-source-item-del"
              :title="`Delete workspace &quot;${w.name}&quot;`"
              aria-label="Delete workspace"
              @click="onDeleteWorkspaceFromMenu(w.id, $event)"
            >−</button>
          </li>
          <li
            role="option"
            class="rp-source-item rp-source-item-new"
            @click="pickNew"
          >
            <span class="rp-source-item-label">New</span>
          </li>
        </ul>
      </div>
      <div class="rp-args-cluster">
        <span class="rp-args-bracket">main(</span>
        <button
          type="button"
          class="rp-args-input rp-args-trigger"
          :class="{ 'is-empty': !argsCompact }"
          :title="argsCompact ? 'Click to edit args (JSON)' : 'Click to enter args (JSON)'"
          @click="openArgsDialog"
        >
          <span v-if="argsCompact" class="rp-args-text">{{ argsCompact }}</span>
          <span v-else class="rp-args-placeholder">{}</span>
        </button>
        <span class="rp-args-bracket">)</span>
      </div>
      <span class="rp-run-cluster" :class="{ 'is-auto': autoRun }">
        <button
          class="rp-action rp-run"
          :disabled="!isReady"
          :title="activePreset.runnableInSandbox ? 'Evaluate' : 'Evaluate with the args JSON above'"
          aria-label="Run"
          @click="runActive"
        >
          <svg viewBox="0 0 10 10" width="12" height="12" aria-hidden="true" class="rp-run-icon">
            <path
              d="M2.5 1.5L8.5 5L2.5 8.5Z"
              :fill="autoRun ? 'currentColor' : 'none'"
              :stroke="autoRun ? 'none' : 'currentColor'"
              stroke-width="1.2"
              stroke-linejoin="round"
            />
          </svg>
        </button>
        <label
          class="rp-autorun"
          :title="autoRun ? 'Auto-run on every edit (click to disable)' : 'Auto-run is off — use the Run button or re-enable'"
        >
          <input v-model="autoRun" type="checkbox" class="rp-autorun-box" />
          <span class="rp-autorun-label">auto-run {{ autoRun ? 'on' : 'off' }}</span>
        </label>
      </span>
      <span class="rp-status-spacer" />
      <!-- Only surface the wasm boot status while it's load-in-progress
           or failure-state; once `isReady` flips, the success line just
           clutters the chrome. -->
      <span v-if="!isReady" class="rp-status-text">{{ status }}</span>

      <!--
        Right-aligned cluster: a direct transplant of the VitePress
        home-page nav right side. We mount the stock components
        (VPNavBarMenu / VPNavBarTranslations / VPNavBarAppearance /
        VPNavBarSocialLinks) so locale, theme and social links all
        share state with every other docs page. The container reuses
        the upstream `.content-body` separator pattern so the 1px
        vertical pipes between groups land in the exact same spots
        as on the home page.
      -->
      <div class="rp-navbar content-body" role="toolbar" aria-label="Playground navigation">
        <VPNavBarMenu class="menu" />
        <VPNavBarTranslations class="translations" />
        <VPNavBarAppearance class="appearance" />
        <VPNavBarSocialLinks class="social-links" />
      </div>
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
            :title="entry === f.path ? 'Entry file (evaluate runs from here)' : 'Click ▸ to make this the entry file'"
            @click="selectFile(f.path)"
          >
            <span class="rp-tab-label">{{ f.path }}</span>
            <span
              class="rp-tab-entry"
              :class="{ 'is-entry': entry === f.path }"
              :title="entry === f.path ? 'Entry file' : 'Set as entry'"
              :aria-label="entry === f.path ? 'Entry file' : 'Set as entry'"
              @click.stop="setEntry(f.path)"
            >
              <!-- Right-pointing triangle: reads as "execution starts
                   here", which is the literal semantics of `entry`. -->
              <svg viewBox="0 0 10 10" width="9" height="9" aria-hidden="true">
                <path d="M2 1L9 5L2 9Z" fill="currentColor" />
              </svg>
            </span>
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
            class="rp-action rp-action-format"
            title="Format active buffer"
            aria-label="Format"
            :disabled="!isReady"
            @click="runFormat"
          >
            <svg
              class="rp-action-icon-svg"
              viewBox="0 0 24 24"
              width="13"
              height="13"
              fill="currentColor"
              aria-hidden="true"
            >
              <!-- Heroicons-style four-point sparkles: one big, two small -->
              <path d="M14 3 L15.1 6.9 L19 8 L15.1 9.1 L14 13 L12.9 9.1 L9 8 L12.9 6.9 Z" />
              <path d="M6.5 11.5 L7.1 13.4 L9 14 L7.1 14.6 L6.5 16.5 L5.9 14.6 L4 14 L5.9 13.4 Z" />
              <path d="M16.5 15 L17 16.5 L18.5 17 L17 17.5 L16.5 19 L16 17.5 L14.5 17 L16 16.5 Z" />
            </svg>
            <span class="rp-action-label">Format</span>
          </button>
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
        >
          <!-- Closed → up chevron (clicking lifts the panel up).
               Open → down chevron (clicking pushes the panel down).
               Mirrors devtools / VS Code bottom-panel conventions. -->
          <span
            :class="errorsExpanded ? 'vpi-chevron-down' : 'vpi-chevron-up'"
            aria-hidden="true"
          />
        </button>
        <span v-if="isReady && wasmVersion" class="rp-errors-version">
          relon-wasm v{{ wasmVersion }}
        </span>
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
        <h3 class="rp-dialog-title" @pointerdown="startDialogDrag">New file</h3>
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

    <dialog ref="newWorkspaceDialog" class="rp-dialog" @close="newWorkspaceError = ''">
      <form class="rp-dialog-form" @submit.prevent="confirmNewWorkspace">
        <h3 class="rp-dialog-title" @pointerdown="startDialogDrag">New workspace</h3>
        <label class="rp-dialog-row">
          <span class="rp-dialog-label">Name</span>
          <input
            v-model="newWorkspaceName"
            class="rp-dialog-input"
            type="text"
            placeholder="my workspace"
            autocomplete="off"
            @keydown.esc.prevent="closeNewWorkspaceDialog"
          />
        </label>
        <p v-if="newWorkspaceError" class="rp-dialog-error">{{ newWorkspaceError }}</p>
        <p class="rp-dialog-hint">
          A workspace starts with an empty <code>main.relon</code>. Files
          and arg JSON are saved to <code>localStorage</code> as you edit.
        </p>
        <div class="rp-dialog-actions">
          <button type="button" class="rp-dialog-btn" @click="closeNewWorkspaceDialog">Cancel</button>
          <button type="submit" class="rp-dialog-btn rp-dialog-btn-primary">Create</button>
        </div>
      </form>
    </dialog>

    <dialog ref="argsDialog" class="rp-dialog rp-args-dialog" @close="argsDraftError = ''">
      <form class="rp-dialog-form" @submit.prevent="confirmArgs">
        <h3 class="rp-dialog-title" @pointerdown="startDialogDrag">Args</h3>
        <div ref="argsEditorHost" class="rp-dialog-editor" @keydown.esc.prevent="closeArgsDialog"></div>
        <p v-if="argsDraftError" class="rp-dialog-error">{{ argsDraftError }}</p>
        <p class="rp-dialog-hint">
          The inline field shows a compact (single-line) projection.
          <kbd>⌘/Ctrl</kbd>+<kbd>Enter</kbd> to save, <kbd>Esc</kbd> to cancel.
        </p>
        <div class="rp-dialog-actions">
          <button type="button" class="rp-dialog-btn" @click="closeArgsDialog">Cancel</button>
          <button type="submit" class="rp-dialog-btn rp-dialog-btn-primary">Save</button>
        </div>
      </form>
    </dialog>
  </div>
</template>

<style scoped>
/* Syntax-highlight palette. CodeMirror tokens are tagged with the
   `cm-r-*` classes by `playgroundHighlightStyle`; we map them to CSS
   variables so a single declaration block flips both panes together
   when VitePress toggles `:root.dark`. Hex values picked to keep
   AA contrast (≥4.5:1) against `--vp-c-bg` in each mode. */
.relon-playground {
  --rp-c-comment: #8a6f47;
  --rp-c-string:  #a31515;
  --rp-c-number:  #117a4f;
  --rp-c-atom:    #117a4f;
  --rp-c-keyword: #7a1f97;
  --rp-c-type:    #006d6a;
  --rp-c-ref:     #1a5f80;
  --rp-c-meta:    #a300a3;
  --rp-c-operator:#5a6675;
  --rp-c-property:#0f4c91;
  --rp-c-function:#8c5a00;
  --rp-c-param:   #a14d00;

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

:root.dark .relon-playground {
  --rp-c-comment: #9aa18b;
  --rp-c-string:  #f5a097;
  --rp-c-number:  #94d3a8;
  --rp-c-atom:    #94d3a8;
  --rp-c-keyword: #d9a3ff;
  --rp-c-type:    #7fd4c5;
  --rp-c-ref:     #92cdf0;
  --rp-c-meta:    #f486f4;
  --rp-c-operator:#9aa6b5;
  --rp-c-property:#a4c8ff;
  --rp-c-function:#dcdcaa;
  --rp-c-param:   #e8b97a;
}

.rp-editor :deep(.cm-r-comment),
.rp-output :deep(.cm-r-comment)  { color: var(--rp-c-comment); font-style: italic; }
.rp-editor :deep(.cm-r-string),
.rp-output :deep(.cm-r-string)   { color: var(--rp-c-string); }
.rp-editor :deep(.cm-r-number),
.rp-output :deep(.cm-r-number)   { color: var(--rp-c-number); }
.rp-editor :deep(.cm-r-atom),
.rp-output :deep(.cm-r-atom)     { color: var(--rp-c-atom); }
.rp-editor :deep(.cm-r-keyword),
.rp-output :deep(.cm-r-keyword)  { color: var(--rp-c-keyword); font-weight: 600; }
.rp-editor :deep(.cm-r-type),
.rp-output :deep(.cm-r-type)     { color: var(--rp-c-type); }
.rp-editor :deep(.cm-r-ref),
.rp-output :deep(.cm-r-ref)      { color: var(--rp-c-ref); font-weight: 600; }
.rp-editor :deep(.cm-r-meta),
.rp-output :deep(.cm-r-meta)     { color: var(--rp-c-meta); }
.rp-editor :deep(.cm-r-operator),
.rp-output :deep(.cm-r-operator) { color: var(--rp-c-operator); }
.rp-editor :deep(.cm-r-property),
.rp-output :deep(.cm-r-property) { color: var(--rp-c-property); }
.rp-editor :deep(.cm-r-function),
.rp-output :deep(.cm-r-function) { color: var(--rp-c-function); font-weight: 600; }
.rp-editor :deep(.cm-r-param),
.rp-output :deep(.cm-r-param)    { color: var(--rp-c-param); font-style: italic; }

.rp-status {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 4px 12px;
  /* Unified font-size across the header — left controls and right
     nav items both inherit from here (13px), bridging the previous
     12 vs 14 jump. Specific elements (brand title 14px, autorun
     caption 11px) override below to keep visual hierarchy. */
  font-size: 13px;
  /* Mirror the Errors dock styling at the top of the playground —
     same `--vp-c-bg-alt` tone, same 2px divider, same subtle shadow —
     so the top status bar and bottom errors bar read as a matched
     pair of "chrome" rails bracketing the editor panes. */
  background: var(--vp-c-bg-alt, var(--vp-c-bg-soft));
  border-bottom: 2px solid var(--vp-c-divider);
  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.05);
  color: var(--vp-c-text-2);
  z-index: 1;
  /* Locally shrink the VitePress nav-height variable. The transplanted
     VPNavBar* components use it for line-height + intrinsic heights
     (`VPNavBarMenuLink` is the biggest offender at line-height: 64px),
     which would otherwise blow this header up to docs-nav size. */
  --vp-nav-height: 34px;
}

/* Brand lockup in the top-left of the playground header. Two-tone
   horizontal pill mirroring the file-icon brand mark: {R} on the
   neutral background (left segment) + white "Relon" wordmark on a
   filled green segment (right). The standalone layout has no
   VitePress navbar, so this is the only path back to the docs root —
   the pill makes the click target obvious. */
/* Brand palette pinned to the logo.svg artwork (not VitePress brand
   tokens) so the playground header reads as the same lockup as the
   home-page hero — cream {R} half + green "Relon" half. */
.rp-brand {
  display: inline-flex;
  align-items: stretch;
  height: 28px;
  border-radius: 6px;
  border: 1px solid #3E8A50;
  overflow: hidden;
  text-decoration: none;
  line-height: 1;
  background: #F3F1ED;
  transition: filter 0.15s ease;
}

.rp-brand:hover {
  filter: brightness(0.97);
}

.rp-brand-logo {
  width: 20px;
  height: 20px;
  margin: 0 8px;
  align-self: center;
  display: block;
}

.rp-brand-name {
  display: inline-flex;
  align-items: center;
  padding: 0 10px;
  background: #3E8A50;
  color: #F3F1ED;
  font-family: var(--vp-font-family-base);
  font-weight: 700;
  font-size: 14px;
  letter-spacing: 0.01em;
}

/* Top-bar controls share a single height + radius so the row reads as
   one tidy strip. Each control still owns its own font-size /
   padding-x — only the vertical sizing is unified via `height` +
   `box-sizing: border-box`. */
.rp-args-input,
.rp-status .rp-action {
  height: 26px;
  box-sizing: border-box;
  border-radius: 4px;
  border: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  font-size: 13px;
  line-height: 1;
  vertical-align: middle;
}

/* Custom source picker.
   - The chip is a `<button>` so we can theme it cross-platform (native
     `<select>` open-menus are unstyleable beyond OS defaults).
   - Menu is a `<ul role="listbox">` that flattens presets + user
     workspaces into one continuous list, with a `New…` row pinned at
     the bottom. User workspaces additionally render a small `−`
     delete control that swallows the click so the row-click still
     selects the workspace.
   - Open/close is driven by `sourceMenuOpen`; outside clicks and
     Escape are wired up at the window level in onMounted. */
.rp-source-wrap {
  position: relative;
  display: inline-block;
}

.rp-source {
  height: 26px;
  box-sizing: border-box;
  display: inline-flex;
  align-items: center;
  justify-content: space-between;
  gap: 6px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 4px;
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  font-family: var(--vp-font-family-mono);
  font-size: 13px;
  line-height: 1;
  padding: 0 8px;
  cursor: pointer;
  /* Lock the chip width so switching between short and long names
     (e.g. "demo" vs "feature_flag") doesn't bounce the surrounding
     controls. 13ch covers the longest builtin preset; +28px accounts
     for left/right padding (16) and the caret (10) + gap. */
  min-width: calc(13ch + 28px);
}

.rp-source-current {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.rp-source:hover,
.rp-source[aria-expanded='true'] {
  border-color: var(--vp-c-text-3);
}

.rp-source-caret {
  font-size: 10px;
  color: var(--vp-c-text-3);
}

.rp-source-menu {
  position: absolute;
  top: calc(100% + 4px);
  left: 0;
  z-index: 20;
  margin: 0;
  padding: 4px 0;
  list-style: none;
  min-width: 100%;
  max-width: 240px;
  background: var(--vp-c-bg-elv, var(--vp-c-bg));
  border: 1px solid var(--vp-c-divider);
  border-radius: 6px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.18);
  font-family: var(--vp-font-family-mono);
  font-size: 13px;
  line-height: 1;
}

.rp-source-item {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 10px;
  color: var(--vp-c-text-1);
  cursor: pointer;
  user-select: none;
  font-size: 13px;
}

.rp-source-item:hover {
  background: var(--vp-c-default-soft);
}

.rp-source-item.is-active {
  color: var(--vp-c-green-3);
  font-weight: 600;
}

.rp-source-item-label {
  flex: 1 1 auto;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.rp-source-item-del {
  width: 18px;
  height: 18px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border: none;
  border-radius: 3px;
  background: transparent;
  color: var(--vp-c-text-3);
  font-size: 14px;
  line-height: 1;
  cursor: pointer;
  padding: 0;
  opacity: 0;
  transition: opacity 80ms ease;
}

.rp-source-item-ws:hover .rp-source-item-del {
  opacity: 1;
}

.rp-source-item-del:hover {
  background: var(--vp-c-danger-soft, rgba(229, 83, 91, 0.16));
  color: var(--vp-c-danger-1, #e0535b);
}

/* `New…` sits visually distinct (top divider) but doesn't get the
   active-state colour since it isn't a selectable workspace. */
.rp-source-item-new {
  border-top: 1px solid var(--vp-c-divider);
  margin-top: 2px;
  padding-top: 8px;
  color: var(--vp-c-text-2);
}

.rp-status-text { color: var(--vp-c-text-3); }

.rp-status-spacer {
  flex: 1 1 auto;
  min-width: 8px;
}

/* Right-aligned nav cluster: a transplant of VitePress's home nav
   right column (`.content-body` from VPNavBar). The stock components
   we mount inside (`VPNavBarMenu`, `VPNavBarTranslations`,
   `VPNavBarAppearance`, `VPNavBarSocialLinks`) ship with their own
   `display: none` gates at the 768 / 1280 viewport breakpoints —
   appropriate inside the docs nav, but our playground is its own
   focused UI that has room for them at every width. Force them all
   back to flex display. The `::before` separator rules at the bottom
   mirror the upstream `.menu + .translations::before` / etc. pattern
   so the 1×24 dividers land in the same spots as the home page. */
.rp-navbar {
  display: inline-flex;
  align-items: center;
  margin-left: 12px;
}

.rp-navbar :deep(.VPNavBarMenu),
.rp-navbar :deep(.VPNavBarTranslations),
.rp-navbar :deep(.VPNavBarAppearance),
.rp-navbar :deep(.VPNavBarSocialLinks) {
  display: flex;
  align-items: center;
}

/* The default VPNavBarMenuLink renders at `font-weight: 500` and
   `font-size: 14px`, both of which look bigger than the 13px control
   strip on the left. Bring weight + size down so the nav recedes
   into the header rail. */
.rp-navbar :deep(.VPNavBarMenuLink) {
  font-weight: 400;
  font-size: 13px;
}

.rp-navbar :deep(.menu + .translations::before),
.rp-navbar :deep(.menu + .appearance::before),
.rp-navbar :deep(.menu + .social-links::before),
.rp-navbar :deep(.translations + .appearance::before),
.rp-navbar :deep(.appearance + .social-links::before) {
  margin-right: 8px;
  margin-left: 8px;
  width: 1px;
  height: 24px;
  background-color: var(--vp-c-divider);
  content: '';
}

.rp-navbar :deep(.menu + .appearance::before),
.rp-navbar :deep(.translations + .appearance::before) {
  margin-right: 16px;
}

.rp-navbar :deep(.appearance + .social-links::before) {
  margin-left: 16px;
}

.rp-navbar :deep(.social-links) {
  margin-right: -8px;
}

/* Run + auto-run share a horizontal cluster. Originally bottom-
   aligned for a "hanging tag" effect; centred now to stay on the
   same horizontal baseline as every other header control — the
   caption competing with the row's baseline was breaking the
   overall rhythm of the strip. */
.rp-run-cluster {
  display: inline-flex;
  flex-direction: row;
  align-items: center;
  gap: 6px;
}

.rp-autorun {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  /* Balanced with the surrounding controls — same color and size as
     the args brackets to maintain a clean horizontal rhythm. */
  color: var(--vp-c-text-3);
  font-size: 12px;
  line-height: 1;
  cursor: pointer;
  user-select: none;
  opacity: 0.85;
  transition: opacity 120ms ease, color 120ms ease;
}

.rp-autorun-box {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
}

.rp-autorun-label {
  white-space: nowrap;
  letter-spacing: 0.02em;
}

.rp-autorun:hover {
  /* Hover reads "this is a link / clickable" — brand link colour
     plus underline matches the docs-page convention for tappable
     text, so users understand the label itself is the toggle target
     (not just the dot/ring decoration). */
  opacity: 1;
  color: var(--vp-c-brand-1);
}

.rp-autorun:hover .rp-autorun-label {
  text-decoration: underline;
}

.rp-autorun-box:focus-visible + .rp-autorun-label {
  outline: 2px solid var(--vp-c-green-3);
  outline-offset: 2px;
  border-radius: 2px;
}


.rp-args-cluster {
  display: inline-flex;
  align-items: center;
  gap: 2px;
}

.rp-args-bracket {
  font-family: var(--vp-font-family-mono);
  font-size: 12px;
  font-weight: 500;
  color: var(--vp-c-text-3);
  user-select: none;
  /* Slight nudge to align with the mono text inside the box */
  margin-top: -1px;
}

.rp-args-input {
  /* Halved from the original 280px — 140px is enough to scan the
     compact JSON preview at a glance without crowding out the rest
     of the header on narrow screens. */
  width: 140px;
  padding: 0 8px;
  font-family: var(--vp-font-family-mono);
  font-size: 13px;
  outline: none;
}

.rp-args-input:focus {
  border-color: var(--vp-c-brand-1, #6470ff);
}

/* The inline Args field is now a click-to-open button shaped like an
   input. The text inside is whatever fits on one line of the compact
   JSON projection; longer payloads truncate with an ellipsis. */
.rp-args-trigger {
  display: inline-flex;
  align-items: center;
  cursor: pointer;
  text-align: left;
  overflow: hidden;
}

.rp-args-trigger:hover {
  border-color: var(--vp-c-brand-1, #3eaf7c);
}

.rp-args-trigger .rp-args-text,
.rp-args-trigger .rp-args-placeholder {
  width: 100%;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  font-family: var(--vp-font-family-mono);
}

.rp-args-trigger .rp-args-placeholder {
  color: var(--vp-c-text-3);
}

/* Play-button styling: rounded rectangle footprint, green fill, white triangle.
   Fill / hover / active steps are pinned to the logo.svg green
   palette (#3E8A50 base, with adjacent shades present in the artwork)
   so the Run button reads as the same green as the {R}/Relon brand
   pill on the left of the header. Selector is doubled
   (`.rp-status .rp-action.rp-run`) to beat the unified-control rule
   above on specificity. */
.rp-status .rp-action.rp-run {
  /* Rounded rectangle (slightly smaller 24px to match the args box
     visual weight). The ring and triangle are now integrated via an
     SVG child and a simple border toggle on this parent. */
  position: relative;
  width: 24px;
  height: 24px;
  padding: 0;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: 4px;
  background: #3E8A50;
  border: 1px solid #3E8A50;
  color: #ffffff;
  outline: none;
  transition: background 120ms ease, border-color 120ms ease,
              color 120ms ease, transform 80ms ease;
}

/* Auto state: the solid fill evaporates into a ring (the "hollow
   outer circle") and the triangle flips to solid green. */
.rp-run-cluster.is-auto .rp-action.rp-run {
  background: transparent;
  color: #3E8A50;
}

.rp-status .rp-action.rp-run:hover:not(:disabled) {
  background: #479759;
  border-color: #479759;
}

.rp-run-cluster.is-auto .rp-action.rp-run:hover:not(:disabled) {
  background: rgba(62, 138, 80, 0.08);
}

/* Click feedback: brief press-down. */
.rp-status .rp-action.rp-run:active:not(:disabled) {
  transform: scale(0.9);
}

.rp-status .rp-action.rp-run:disabled {
  background: var(--vp-c-default-soft);
  border-color: var(--vp-c-divider);
  color: var(--vp-c-text-3);
  cursor: not-allowed;
}

.rp-run-icon {
  /* Optical centring — the glyph's visual mass sits ~1px left of its
     bounding box, so nudge right. */
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
  flex: 0 0 4px;
  align-self: stretch;
  cursor: col-resize;
  /* Default `--vp-c-divider` washes out against the playground's
     darker editor background — bump to `--vp-c-divider-dark` (or the
     same colour the docs use for code-block borders) so the seam is
     legible at a glance instead of fading into the panes. */
  background: var(--vp-c-divider-dark, var(--vp-c-divider));
  border-left: none;
  border-right: none;
  margin: 0;
  z-index: 1;
  touch-action: none;
  transition: background-color 120ms ease;
}

.rp-resizer:hover,
.rp-resizer:active {
  background: var(--vp-c-brand-1, #3eaf7c);
}

/* Underline-tab style (VS Code / Chrome devtools): the row sits on
   the editor's own background, no bordered ribbon, and active items
   carry a 2px coloured underline instead of a filled chip. Inactive
   tabs are bare text; hover gets a soft tone but no border. */
.rp-tabs {
  display: flex;
  align-items: stretch;
  gap: 0;
  padding: 0 8px;
  background: transparent;
  border-bottom: 1px solid var(--vp-c-divider);
  overflow-x: auto;
}

.rp-spacer { flex: 1 1 auto; }

.rp-tab {
  position: relative;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 6px 10px;
  border: none;
  border-radius: 0;
  background: transparent;
  color: var(--vp-c-text-2);
  cursor: pointer;
  font-size: 12px;
  white-space: nowrap;
}

/* Active underline is rendered with a ::after that overlays the
   shared bottom border — sits 1px below so it visually consumes the
   row's divider line at the active position. */
.rp-tab::after {
  content: '';
  position: absolute;
  left: 6px;
  right: 6px;
  bottom: -1px;
  height: 2px;
  background: transparent;
  border-radius: 1px;
  pointer-events: none;
}

.rp-tab:hover:not(.is-disabled):not(:disabled) {
  color: var(--vp-c-text-1);
}

.rp-tab.is-active {
  color: var(--vp-c-text-1);
}

.rp-tab.is-active::after {
  background: var(--vp-c-brand-1);
}

.rp-tab.is-disabled,
.rp-tab:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}

/* Entry marker: a small right-pointing triangle (echoes "this is
   where evaluate starts running") rather than the previous star.
   For non-entry tabs we hide it by default and surface only on
   `:hover` of the tab itself — same affordance, way less noise in
   the row. The active entry tab keeps the marker always-on so the
   workspace-level "entry" state is visible at a glance. */
.rp-tab-entry {
  display: inline-flex;
  align-items: center;
  color: var(--vp-c-text-3);
  cursor: pointer;
  line-height: 1;
  opacity: 0;
  transition: opacity 80ms ease, color 80ms ease;
}

.rp-tab:hover .rp-tab-entry {
  opacity: 0.7;
}

.rp-tab-entry:hover {
  opacity: 1 !important;
  color: var(--vp-c-text-1);
}

.rp-tab-entry.is-entry {
  opacity: 1;
  color: #3E8A50;
}

.rp-tab-close {
  color: var(--vp-c-text-3);
  cursor: pointer;
  line-height: 1;
  padding-left: 4px;
}

.rp-tab-close:hover { color: var(--vp-c-danger-1, #e0535b); }

.rp-tab-add {
  font-size: 16px;
  font-weight: 500;
  line-height: 1;
  color: var(--vp-c-text-2);
  padding: 6px 12px;
}

.rp-tab-add:hover {
  color: var(--vp-c-brand-1);
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

/* Format / row-level actions inside `.rp-tabs` ride the new
   underline-tab style: no border, transparent background, hover
   shifts text colour only. They live in the tab row but aren't
   themselves "selected" — no underline. */
.rp-tabs .rp-action {
  border: none;
  background: transparent;
  color: var(--vp-c-text-2);
  padding: 6px 10px;
  display: inline-flex;
  align-items: center;
}

.rp-tabs .rp-action:hover:not(:disabled) {
  color: var(--vp-c-text-1);
}

/* Format action: framed button — visually distinct from the underline
   tabs in the same row. Icon + label sit side-by-side; SVG strokes
   pick up `currentColor` so disabled/hover tweaks propagate. */
.rp-tabs .rp-action.rp-action-format {
  display: inline-flex;
  align-items: center;
  align-self: center;
  gap: 5px;
  padding: 2px 10px;
  margin: 0 6px 0 0;
  height: 22px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 999px;
  background: var(--vp-c-bg-soft);
  color: var(--vp-c-text-2);
  font-size: 11.5px;
  line-height: 1;
  letter-spacing: 0.02em;
  transition: background-color 0.15s ease, border-color 0.15s ease, color 0.15s ease;
}

.rp-tabs .rp-action.rp-action-format:hover:not(:disabled) {
  border-color: var(--vp-c-brand-1);
  color: var(--vp-c-brand-1);
  background: var(--vp-c-bg);
}

.rp-tabs .rp-action.rp-action-format:active:not(:disabled) {
  background: var(--vp-c-bg-mute);
}

.rp-action-format .rp-action-icon-svg {
  display: block;
  flex: 0 0 auto;
}

.rp-action-format .rp-action-label {
  font-size: 12px;
  line-height: 1;
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

/* CodeMirror's stock gutter is `#f5f5f5` with a hard border — fine on
   white, jarring against the VitePress dark canvas. Flatten the gutter
   into the editor background and switch the divider to the shared
   divider colour so it reads as a tonal step, not a separate panel.
   The min-width pins both panes to the same gutter width: with
   viewport-pad on both sides the lineNumbers gutter naturally widens
   to ≥2 digits, so a tight reserve is enough — left editor also
   stacks lint + runtime-error gutters but those keep their own
   intrinsic widths. */
.rp-editor :deep(.cm-gutters),
.rp-output :deep(.cm-gutters) {
  background: transparent;
  border-right: 1px solid var(--vp-c-divider);
  color: var(--vp-c-text-3);
  min-width: 2.5rem;
  /* `.cm-gutters` is `display: flex`. With only the lineNumbers child
     left, the default `flex-start` leaves dead space between the
     digits and the right-hand divider — looks like the numbers are
     floating in a column instead of hugging the rail. Pin to the end
     so 1- and 2-digit values stay flush against the divider. */
  justify-content: flex-end;
}

.rp-editor :deep(.cm-lineNumbers .cm-gutterElement),
.rp-output :deep(.cm-lineNumbers .cm-gutterElement) {
  color: var(--vp-c-text-3);
  /* Reserve room for ≥3 digits so the rail doesn't reflow once the
     buffer crosses line 100 — and so both panes line up before the
     JSON output has anywhere near as many lines as the editor. */
  min-width: 3ch;
  padding: 0 6px;
  text-align: right;
}

/* Conventional IDE layout puts the line-number rail flush against the
   editor content, with diagnostic / breakpoint markers on the *outside*
   (left). CodeMirror renders gutters in registration order, which puts
   `lineNumbers()` leftmost — the opposite of what every developer
   expects. `.cm-gutters` is `display: flex`, so a high `order` on the
   lineNumbers child pushes it to the right end of the gutter strip
   without touching extension order or the diagnostic gutters. */
.rp-editor :deep(.cm-lineNumbers),
.rp-output :deep(.cm-lineNumbers) {
  order: 99;
}

/* Active-line gutter highlight (default `#e2f2ff`) is loud in dark mode. */
.rp-editor :deep(.cm-activeLineGutter) {
  background: var(--vp-c-default-soft);
  color: var(--vp-c-text-2);
}

.rp-editor :deep(.cm-activeLine) {
  background: var(--vp-c-default-soft);
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
  /* Sits inline after "Errors (N)" on the left of the header,
     letting the wasm version chip below claim the right edge. */
  border: none;
  background: transparent;
  color: var(--vp-c-text-2);
  line-height: 1;
  padding: 0 4px;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
}

.rp-errors-toggle-btn:hover {
  color: var(--vp-c-text-1);
}

.rp-errors-toggle-btn [class^='vpi-'] {
  width: 14px;
  height: 14px;
}

/* `relon-wasm v…` floats on the right of the Errors header — the
   playground's "this runtime is up and at version X" indicator now
   that the top-bar status text auto-hides on success. */
.rp-errors-version {
  margin-left: auto;
  color: var(--vp-c-text-3);
  font-family: var(--vp-font-family-mono);
  font-size: 11px;
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
  cursor: grab;
  user-select: none;
  /* Visual handle affordance: the header feels like a draggable bar. */
  padding: 4px 0;
  margin-top: -4px;
}

.rp-dialog-title:active {
  cursor: grabbing;
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

/* Args modal: wider than the new-file modal because users routinely
   paste sizeable JSON payloads. The textarea grows with the dialog
   and uses the mono font so brace nesting stays legible. */
.rp-args-dialog {
  min-width: min(640px, 90vw);
  max-width: 90vw;
}

.rp-dialog-editor {
  width: 100%;
  height: 360px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 4px;
  background: var(--vp-c-bg-soft);
  overflow: hidden;
  box-sizing: border-box;
  /* Focus indicator lives on this rounded wrapper so it follows the
     border-radius. The default CodeMirror focus outline sits on the
     inner `.cm-editor` rectangle, which would draw sharp corners
     over our 4px-rounded shell. */
  transition: border-color 120ms ease, box-shadow 120ms ease;
}

.rp-dialog-editor:focus-within {
  border-color: var(--vp-c-brand-1);
  box-shadow: 0 0 0 1px var(--vp-c-brand-1);
}

.rp-dialog-editor :deep(.cm-editor) {
  height: 100%;
  font-family: var(--vp-font-family-mono);
  font-size: 12px;
  background: transparent;
}

/* Suppress CodeMirror's default 1px focus outline on `.cm-editor`
   since we're handling focus on the wrapper. */
.rp-dialog-editor :deep(.cm-editor.cm-focused) {
  outline: none;
}

.rp-dialog-editor :deep(.cm-scroller) {
  font-family: var(--vp-font-family-mono);
}

.rp-dialog-editor :deep(.cm-gutters) {
  background: transparent;
  border-right: 1px solid var(--vp-c-divider);
  color: var(--vp-c-text-3);
}

.rp-dialog-editor :deep(.cm-lineNumbers .cm-gutterElement) {
  color: var(--vp-c-text-3);
  padding: 0 6px;
  text-align: right;
}

.rp-dialog-editor :deep(.cm-activeLine) {
  background: var(--vp-c-default-soft);
}

.rp-dialog-editor :deep(.cm-activeLineGutter) {
  background: var(--vp-c-default-soft);
  color: var(--vp-c-text-2);
}

.rp-dialog-hint kbd {
  font-family: var(--vp-font-family-mono);
  font-size: 10px;
  padding: 1px 4px;
  border: 1px solid var(--vp-c-divider);
  border-radius: 3px;
  background: var(--vp-c-bg-soft);
  color: var(--vp-c-text-2);
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
