// CodeMirror autocomplete bridge to the wasm `complete(...)` export.
//
// Mirrors `goto-def.ts`'s shape: the playground supplies a synchronous
// resolver (already-loaded wasm call) and we wrap it in CodeMirror's
// `autocompletion` extension. The CodeMirror layer handles filtering
// + ranking; this module just returns every candidate the analyzer
// thinks is reasonable for the cursor position.

import { autocompletion, type Completion, type CompletionContext, type CompletionResult } from '@codemirror/autocomplete';
import type { Extension } from '@codemirror/state';

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

export function relonAutocomplete(resolver: CompletionResolver): Extension {
    return autocompletion({
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
}
