// CodeMirror autocomplete bridge to the wasm `complete(...)` export.
//
// Mirrors `goto-def.ts`'s shape: the playground supplies a synchronous
// resolver (already-loaded wasm call) and we wrap it in CodeMirror's
// `autocompletion` extension. The CodeMirror layer handles filtering
// + ranking; this module just returns every candidate the analyzer
// thinks is reasonable for the cursor position.
//
// Two extras layered on top of `autocompletion(...)`:
//   1. Explicit popup trigger after the user types one of `#@&.` —
//      CodeMirror's default `activateOnTyping` only fires on word
//      characters, so without this the popup stays closed when the
//      user types e.g. `#` and waits to be told what's available.
//   2. Tab accepts the highlighted suggestion (matches VS Code /
//      JetBrains convention). Falls through to the existing
//      `indentWithTab` when no popup is open.

import {
    acceptCompletion,
    autocompletion,
    startCompletion,
    type Completion,
    type CompletionContext,
    type CompletionResult,
} from '@codemirror/autocomplete';
import { EditorView, keymap } from '@codemirror/view';
import { Prec, type Extension } from '@codemirror/state';

/// One item returned by the wasm `complete` call. Kind is the
/// lowercase tag we serialise on the Rust side (see
/// `CompletionResult` in crates/relon-wasm/src/lib.rs).
export interface RelonCompletion {
    label: string;
    kind: string;
    detail?: string | null;
}

/// Callback the playground supplies. Returns every candidate the
/// analyzer thinks is reasonable for the cursor position. CodeMirror
/// re-filters by the in-progress word.
export type CompletionResolver = (line: number, character: number) => RelonCompletion[];

const KIND_TO_TYPE: Record<string, Completion['type']> = {
    method: 'method',
    field: 'property',
    param: 'variable',
    schema: 'class',
    stdlib: 'function',
    module: 'namespace',
    import: 'function',
    reference: 'variable',
    directive: 'keyword',
    pragma: 'keyword',
    decorator: 'function',
    keyword: 'keyword',
};

/// Identifier-shaped words for the default bare context — letters,
/// digits, `_`. We also explicitly anchor on the special prefixes
/// `#`, `@`, `&` so completions trigger right after the user types
/// them.
const WORD = /[A-Za-z_][A-Za-z0-9_]*/;

/// Triggers `startCompletion` whenever the user types one of the
/// special-prefix characters. CodeMirror's default activation only
/// fires for word characters, so without this the popup never
/// opens after `#`, `@`, `&`, or `.`.
const triggerOnSpecialChars = EditorView.updateListener.of((update) => {
    if (!update.docChanged) return;
    let triggered = false;
    update.transactions.forEach((tr) => {
        if (triggered || !tr.docChanged) return;
        tr.changes.iterChanges((_fromA, _toA, _fromB, _toB, inserted) => {
            if (triggered) return;
            const text = inserted.toString();
            if (text.length === 1 && /[#@&.]/.test(text)) {
                triggered = true;
            }
        });
    });
    if (triggered) {
        // Defer to the next tick so `startCompletion` reads the
        // post-transaction doc state.
        queueMicrotask(() => startCompletion(update.view));
    }
});

/// Tab → accept the highlighted suggestion when the popup is open.
/// Returns false when the popup is closed, letting subsequent
/// keybindings (e.g. `indentWithTab`) take over. Bound at
/// `Prec.high` so it sits ahead of `indentWithTab` in the chain.
const tabAcceptsCompletion = Prec.high(
    keymap.of([{ key: 'Tab', run: acceptCompletion }]),
);

export function relonAutocomplete(resolver: CompletionResolver): Extension[] {
    const completionExt = autocompletion({
        // No icons in our HighlightStyle yet; suppress the default
        // "type" icon so the popup stays compact. The kind label is
        // surfaced via `detail` instead.
        icons: false,
        activateOnTyping: true,
        override: [
            (context: CompletionContext): CompletionResult | null => {
                const { state, pos } = context;
                const doc = state.doc;
                const lineObj = doc.lineAt(pos);
                const line = lineObj.number - 1; // CodeMirror lines are 1-based; LSP is 0-based.
                const character = pos - lineObj.from;

                // Trigger detection:
                //   - `wordMatch` covers the in-progress identifier
                //     (`pri│`, `len│`).
                //   - `triggerMatch` covers the special-prefix chars
                //     (`#│`, `@│`, `&│`, `.│`) — completion should
                //     pop up the moment the user types one of these
                //     even though there's no word yet.
                const wordMatch = context.matchBefore(WORD);
                const triggerMatch = context.matchBefore(/[#@&.]/);
                const explicit = context.explicit;

                if (!explicit && !wordMatch && !triggerMatch) {
                    return null;
                }

                // Replacement anchor:
                //   - in `pri│` → start of `pri` so typing `private`
                //     replaces what's there.
                //   - in `#│` / `&│` etc. → cursor (the trigger
                //     itself stays in source; the suggestion is what
                //     follows).
                const from = wordMatch ? wordMatch.from : pos;

                const items = resolver(line, character);
                if (items.length === 0) {
                    return null;
                }

                const completions: Completion[] = items.map((item) => ({
                    label: item.label,
                    type: KIND_TO_TYPE[item.kind] ?? undefined,
                    detail: item.detail ?? undefined,
                }));

                return {
                    from,
                    options: completions,
                    // No `validFor`: any keystroke re-invokes the
                    // resolver. Setting `validFor: WORD` here would
                    // close the popup the instant the user types a
                    // trigger char (since the empty string doesn't
                    // match WORD), which is precisely the wrong UX —
                    // the popup should stay open as the user keeps
                    // typing.
                };
            },
        ],
    });

    return [completionExt, triggerOnSpecialChars, tabAcceptsCompletion];
}
