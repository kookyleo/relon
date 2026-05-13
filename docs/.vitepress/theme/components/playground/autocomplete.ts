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
        // Allow completions to fire after `#`, `@`, `&`, `.` even
        // when there's no word character behind them yet — the user
        // just typed the trigger.
        activateOnTyping: true,
        override: [
            (context: CompletionContext): CompletionResult | null => {
                const { state, pos } = context;
                const doc = state.doc;
                const lineObj = doc.lineAt(pos);
                const line = lineObj.number - 1; // CodeMirror lines are 1-based; LSP is 0-based.
                const character = pos - lineObj.from;

                // Detect what the user just typed so we know how
                // wide the completion's replacement anchor should be.
                // The default `matchBefore(WORD)` returns a span like
                // `len`, but for `&|`, `#|`, `@|`, `lib.|` we want
                // the replacement to start at the cursor (the prefix
                // chars themselves stay).
                const wordMatch = context.matchBefore(WORD);
                const triggerMatch = context.matchBefore(/[#@&.]/);
                const explicit = context.explicit;

                // If the user didn't explicitly invoke completion
                // (Ctrl-Space) AND we're not on a word or trigger,
                // bail — keeps the popup from appearing inside
                // whitespace.
                if (!explicit && !wordMatch && !triggerMatch) {
                    return null;
                }

                let from: number;
                if (wordMatch) {
                    from = wordMatch.from;
                } else {
                    from = pos;
                }

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
                    // CodeMirror's default filter does prefix
                    // matching; for fuzzier behaviour set
                    // `filter: false` and supply your own.
                    validFor: WORD,
                };
            },
        ],
    });
}
