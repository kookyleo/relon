// Editor decorations for runtime errors (EvalError / ProjectionError).
//
// Parse and analyze errors flow through `@codemirror/lint`, which paints
// the conventional red squiggle + gutter cross — that visual vocabulary
// reads "syntax error here", which is right for source-level problems
// but actively misleading for evaluation failures (`#main(Order order)`
// isn't malformed; we just never bound `order`). So we keep lint for
// parse/analyze and route runtime errors through this extension:
//
//   - subtle line-background highlight on the offending line
//   - a small breakpoint-style dot in a dedicated left gutter
//   - the error message surfaced as the hover title
//
// `setRuntimeErrors` is the only public effect; everything else is
// internal to the bundle exported as `runtimeErrorExtension`.

import {
    Decoration,
    EditorView,
    GutterMarker,
    gutter,
    type DecorationSet,
} from '@codemirror/view';
import { StateEffect, StateField } from '@codemirror/state';

export interface RuntimeErrorMark {
    /** 1-based line number in the active document. */
    line: number;
    message: string;
}

export const setRuntimeErrors = StateEffect.define<RuntimeErrorMark[]>();

const runtimeMarksField = StateField.define<RuntimeErrorMark[]>({
    create: () => [],
    update(value, tr) {
        for (const effect of tr.effects) {
            if (effect.is(setRuntimeErrors)) return effect.value;
        }
        return value;
    },
});

const runtimeLineDecoField = StateField.define<DecorationSet>({
    create: () => Decoration.none,
    update(_value, tr) {
        const marks = tr.state.field(runtimeMarksField);
        if (marks.length === 0) return Decoration.none;
        const docLines = tr.state.doc.lines;
        const ranges = [];
        for (const m of marks) {
            if (m.line < 1 || m.line > docLines) continue;
            const lineInfo = tr.state.doc.line(m.line);
            ranges.push(
                Decoration.line({
                    class: 'cm-relon-runtime-line',
                    attributes: { title: m.message },
                }).range(lineInfo.from)
            );
        }
        return Decoration.set(ranges);
    },
    provide: (f) => EditorView.decorations.from(f),
});

class RuntimeDotMarker extends GutterMarker {
    constructor(private message: string) {
        super();
    }
    override toDOM() {
        const el = document.createElement('span');
        el.className = 'cm-relon-runtime-dot';
        el.title = this.message;
        return el;
    }
}

const runtimeGutter = gutter({
    class: 'cm-relon-runtime-gutter',
    lineMarker(view, line) {
        const marks = view.state.field(runtimeMarksField, false);
        if (!marks || marks.length === 0) return null;
        const lineNo = view.state.doc.lineAt(line.from).number;
        const match = marks.find((m) => m.line === lineNo);
        return match ? new RuntimeDotMarker(match.message) : null;
    },
    lineMarkerChange: (update) =>
        update.transactions.some((tr) =>
            tr.effects.some((effect) => effect.is(setRuntimeErrors))
        ),
});

export const runtimeErrorExtension = [
    runtimeMarksField,
    runtimeLineDecoField,
    runtimeGutter,
];
