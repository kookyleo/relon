// Viewport line-number filler.
//
// CodeMirror 6's `lineNumbers()` gutter only renders markers for lines
// that exist in the document — a 1-line JSON output sitting in a 40-row
// viewport shows a single "1" with a huge blank rail underneath. To get
// the editor-style "line numbers continue to the bottom" look from VS
// Code / Monaco, we pad the document with trailing empty lines so the
// stock gutter renders them, then carefully hide those padding lines
// from every other layer:
//
//   - the saved file content (via `unpaddedContent`),
//   - the update listener that drives evaluate (via `isPaddingUpdate`),
//   - the cursor / selection (via a transaction filter that clamps any
//     selection to the unpadded range).
//
// Padding count is recomputed on every geometry / viewport / doc change.
// Non-padding user edits invalidate the padding tracker by counting the
// actual trailing `\n` run; this stops a stray insertion in the padded
// region from being misclassified as "still padding" forever.

import {
    Annotation,
    EditorSelection,
    EditorState,
    StateEffect,
    StateField,
    type Extension,
} from '@codemirror/state';
import {
    EditorView,
    ViewPlugin,
    type ViewUpdate,
} from '@codemirror/view';

// Tags a transaction we dispatched ourselves to insert / remove padding.
// Consumers use `isPaddingUpdate` to skip these in their listeners; we
// also use it to short-circuit the plugin's own re-schedule loop.
const paddingAnnotation = Annotation.define<boolean>();
const setPad = StateEffect.define<number>();

const padCountField = StateField.define<number>({
    create: () => 0,
    update(value, tr) {
        // Our own padding dispatch always carries `setPad` — apply it
        // directly. This branch must win over the doc-change recompute
        // below because the doc *also* changes inside a padding tx.
        for (const e of tr.effects) {
            if (e.is(setPad)) return e.value;
        }
        // External (non-padding) edits may have rewritten the doc tail
        // or trimmed it shorter than the previous pad. Reconcile by
        // counting the actual trailing `\n` run and clamping to that.
        if (tr.docChanged) {
            const text = tr.newDoc.toString();
            let i = text.length;
            let trailing = 0;
            while (i > 0 && text[i - 1] === '\n') { i--; trailing++; }
            return Math.min(value, trailing);
        }
        return value;
    },
});

function adjustPadding(view: EditorView) {
    const lineH = view.defaultLineHeight;
    if (!lineH || lineH <= 0) return;
    const containerH = view.dom.clientHeight;
    if (containerH <= 0) return;
    const wantLines = Math.max(1, Math.floor(containerH / lineH));
    const currentPad = view.state.field(padCountField);
    const realLines = view.state.doc.lines - currentPad;
    const wantPad = Math.max(0, wantLines - realLines);
    if (wantPad === currentPad) return;
    const docLen = view.state.doc.length;
    const padStart = docLen - currentPad;
    view.dispatch({
        changes: { from: padStart, to: docLen, insert: '\n'.repeat(wantPad) },
        effects: setPad.of(wantPad),
        annotations: paddingAnnotation.of(true),
    });
}

// Selections that fall past the unpadded tail are clamped back to the
// last real-content position. Without this, clicking in the empty
// gutter-extended area would land the cursor inside the padding, where
// typing would interleave with our trailing `\n`s.
const clampSelection = EditorState.transactionFilter.of((tr) => {
    if (!tr.selection) return tr;
    const pad = tr.startState.field(padCountField);
    if (pad <= 0) return tr;
    const maxPos = tr.newDoc.length - pad;
    const sel = tr.selection;
    let needsClamp = false;
    for (const r of sel.ranges) {
        if (r.from > maxPos || r.to > maxPos) { needsClamp = true; break; }
    }
    if (!needsClamp) return tr;
    const ranges = sel.ranges.map((r) =>
        EditorSelection.range(Math.min(r.anchor, maxPos), Math.min(r.head, maxPos))
    );
    return [tr, { selection: EditorSelection.create(ranges, sel.mainIndex) }];
});

const viewportPadPlugin = ViewPlugin.fromClass(class {
    pending = false;
    view: EditorView;
    constructor(view: EditorView) {
        this.view = view;
        this.schedule();
    }
    schedule() {
        if (this.pending) return;
        this.pending = true;
        queueMicrotask(() => {
            this.pending = false;
            adjustPadding(this.view);
        });
    }
    update(u: ViewUpdate) {
        // Don't re-schedule on our own padding dispatch — `adjustPadding`
        // is idempotent (early-returns when pad already matches), but
        // short-circuiting saves a microtask per re-pad.
        if (u.transactions.some((t) => t.annotation(paddingAnnotation))) return;
        if (u.geometryChanged || u.viewportChanged || u.docChanged) {
            this.schedule();
        }
    }
});

export const viewportPad: Extension = [padCountField, viewportPadPlugin, clampSelection];

/// Return the document text with our trailing-newline padding stripped.
/// Use this in update listeners / before sending content to wasm.
export function unpaddedContent(view: EditorView): string {
    const pad = view.state.field(padCountField);
    const text = view.state.doc.toString();
    return pad > 0 ? text.slice(0, text.length - pad) : text;
}

/// True when every transaction in this update is a padding-maintenance
/// dispatch from this extension; consumers should skip such updates so
/// they don't trigger evaluate / "file dirty" side effects.
export function isPaddingUpdate(u: ViewUpdate): boolean {
    return u.transactions.length > 0
        && u.transactions.every((t) => t.annotation(paddingAnnotation) === true);
}
