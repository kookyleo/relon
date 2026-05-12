// CodeMirror 6 syntax highlighting for Relon.
//
// Implemented as a `StreamLanguage` simple tokenizer rather than a Lezer
// grammar — keeps the dependency surface narrow (no `@lezer/lr` build
// step) and is plenty for read-friendly highlighting. If we later want
// folding / structural navigation we can graduate to Lezer; the
// `<RelonEditor>` consumer only sees a `LanguageSupport`.
//
// Tokens emitted (mapped to standard CodeMirror tags via StreamLanguage):
//   - `keyword`        : `#schema`, `#main`, `#extend`, ... and the
//                        legacy `@fn` / `@schema` decorators that still
//                        appear in `examples/*.relon`.
//   - `meta`           : `@<ident>` decorator invocations.
//   - `string`         : `"..."` and `f"..."` (interpolation handled as
//                        plain string at this layer; good enough).
//   - `number`         : integers + floats.
//   - `comment`        : `// line` and `/* block */`.
//   - `variableName`   : bare identifiers (default fallback).

import { StreamLanguage, LanguageSupport, type StreamParser } from '@codemirror/language';

// Hash-prefixed keywords. Order is irrelevant; `Set.has` lookup.
const HASH_KEYWORDS = new Set([
    '#schema',
    '#main',
    '#extend',
    '#derive',
    '#no_auto_derive',
    '#native',
    '#private',
    '#brand',
    '#import',
]);

// `@`-prefixed forms that the language treats as keywords (not user
// decorators). Older examples still use these; keeping them highlighted
// distinctly avoids visual noise.
const AT_KEYWORDS = new Set(['@schema', '@fn']);

interface RelonState {
    inBlockComment: boolean;
}

const parser: StreamParser<RelonState> = {
    name: 'relon',

    startState(): RelonState {
        return { inBlockComment: false };
    },

    token(stream, state) {
        // Resume inside a block comment carried over from the previous
        // line. Bail to `null` (no token) once we walk past `*/`.
        if (state.inBlockComment) {
            while (!stream.eol()) {
                if (stream.match('*/')) {
                    state.inBlockComment = false;
                    return 'comment';
                }
                stream.next();
            }
            return 'comment';
        }

        // Skip leading whitespace; CodeMirror does this for us but
        // documenting the contract here keeps the tokenizer obvious.
        if (stream.eatSpace()) {
            return null;
        }

        // Line comment.
        if (stream.match('//')) {
            stream.skipToEnd();
            return 'comment';
        }
        // Block comment open.
        if (stream.match('/*')) {
            state.inBlockComment = true;
            // Same-line close.
            while (!stream.eol()) {
                if (stream.match('*/')) {
                    state.inBlockComment = false;
                    return 'comment';
                }
                stream.next();
            }
            return 'comment';
        }

        // f-string prefix: consume `f` then fall through to string.
        if (stream.match(/^f"/)) {
            consumeString(stream);
            return 'string';
        }
        if (stream.peek() === '"') {
            stream.next();
            consumeString(stream);
            return 'string';
        }

        // Hash-prefixed keyword: `#schema`, `#main`, ...
        if (stream.peek() === '#') {
            const match = stream.match(/^#[A-Za-z_][A-Za-z0-9_]*/);
            if (match) {
                const tok = (match as RegExpMatchArray)[0];
                return HASH_KEYWORDS.has(tok) ? 'keyword' : 'meta';
            }
            // Lone `#` — let it pass as punctuation.
            stream.next();
            return null;
        }

        // `@<ident>`: either a built-in keyword form (`@fn`, `@schema`)
        // or a user decorator. Both highlight as `meta`/`keyword`; the
        // visual difference is intentional but subtle.
        if (stream.peek() === '@') {
            const match = stream.match(/^@[A-Za-z_][A-Za-z0-9_]*/);
            if (match) {
                const tok = (match as RegExpMatchArray)[0];
                return AT_KEYWORDS.has(tok) ? 'keyword' : 'meta';
            }
            stream.next();
            return null;
        }

        // Numbers (int / float, optional leading `-` is left to the
        // operator path so `1-2` doesn't get glued).
        if (stream.match(/^\d+(\.\d+)?([eE][+-]?\d+)?/)) {
            return 'number';
        }

        // Identifiers / keywords-by-value. `true`/`false`/`null` are
        // values; surface them as `atom` so themes pick the literal
        // colour.
        if (stream.match(/^[A-Za-z_][A-Za-z0-9_]*/)) {
            const word = (stream.current() as string).toLowerCase();
            if (word === 'true' || word === 'false' || word === 'null') {
                return 'atom';
            }
            return 'variableName';
        }

        // Operators / punctuation — eat one char, no token (default colour).
        stream.next();
        return null;
    },
};

function consumeString(stream: { eol: () => boolean; next: () => string | undefined }) {
    // We were called positioned just past the opening `"`. Walk to the
    // matching `"`, respecting backslash escapes. Unterminated strings
    // (eol reached without close) fall through to be re-entered next
    // line as a fresh string — acceptable for highlight-only mode.
    while (!stream.eol()) {
        const ch = stream.next();
        if (ch === '\\') {
            stream.next();
            continue;
        }
        if (ch === '"') {
            return;
        }
    }
}

export function relonLanguage(): LanguageSupport {
    return new LanguageSupport(StreamLanguage.define(parser));
}
