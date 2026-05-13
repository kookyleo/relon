// Editor decorations for runtime errors (EvalError / ProjectionError).
//
// Parse and analyze errors flow through `@codemirror/lint`, which paints
// the conventional red squiggle — that visual vocabulary reads "syntax
// error here", which is right for source-level problems but actively
// misleading for evaluation failures (`#main(Order order)` isn't
// malformed; we just never bound `order`). So we keep lint for
// parse/analyze and route runtime errors through this extension:
//
//   - subtle line-background highlight on the offending line
//   - the error message surfaced as the hover title
//
// (The breakpoint-style gutter dot that used to live here was removed
// when the playground gutter was tightened to a uniform 3-digit
// line-number rail — keeping a per-line marker would have forced the
// editor and JSON gutters to disagree in width. Errors remain visible
// via the line background + bottom error panel.)
//
// `setRuntimeErrors` is the only public effect; everything else is
// internal to the bundle exported as `runtimeErrorExtension`.

import {
    Decoration,
    EditorView,
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

export const runtimeErrorExtension = [
    runtimeMarksField,
    runtimeLineDecoField,
];
