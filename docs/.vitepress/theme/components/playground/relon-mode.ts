// CodeMirror 6 syntax highlighting for Relon.
//
// Implemented as a `StreamLanguage` simple tokenizer rather than a Lezer
// grammar — keeps the dependency surface narrow (no `@lezer/lr` build
// step) and is plenty for read-friendly highlighting. If we later want
// folding / structural navigation we can graduate to Lezer; the
// `<RelonEditor>` consumer only sees a `LanguageSupport`.
//
// Tokens emitted (mapped to standard CodeMirror tags via StreamLanguage):
//   - `keyword`              : `#schema`, `#main`, `#extend`, ... and the
//                              legacy `@fn` / `@schema` decorators that
//                              still appear in `examples/*.relon`.
//   - `meta`                 : `@<ident>` decorator invocations.
//   - `string`               : `"..."` and `f"..."` (interpolation
//                              handled as plain string at this layer).
//   - `number`               : integers + floats.
//   - `comment`              : `// line` and `/* block */`.
//   - `atom`                 : `true` / `false` / `null`.
//   - `variableName.special` : reference prefixes `&root`, `&sibling`,
//                              `&uncle`, `&prev`, `&next`, `&index`,
//                              `&this` (path tail is plain identifier).
//   - `controlKeyword`       : `where`, `match`, `for`, `in`, `if`, `as`,
//                              `with` — every keyword the parser
//                              actually consumes today.
//   - `typeName`             : canonical builtin types as enumerated by
//                              `is_builtin_type_name` in
//                              `crates/relon-parser/src/token.rs`.
//   - `operator`             : arithmetic / comparison / logical / pipe
//                              / arrow / spread tokens (see `source.rs`
//                              multi-char op table + single-char ops).
//   - `variableName`         : bare identifiers (default fallback).
//
// CodeMirror token ↔ TextMate scope (kept in sync with relon.tmLanguage.json):
//   reference  &root|&sibling|...        → variable.language.relon
//   control    where|match|for|in|...    → keyword.control.relon
//   type       Int|String|List|...       → support.type.relon
//   operator   == != && || => -> ...     → keyword.operator.relon

import {
    StreamLanguage,
    LanguageSupport,
    HighlightStyle,
    syntaxHighlighting,
    type StreamParser,
} from '@codemirror/language';
import { tags as t } from '@lezer/highlight';

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

// Reference bases that follow `&`. Source of truth:
// `crates/relon-parser/src/reference_var.rs` (`RefBase` literal table).
const REFERENCE_BASES = new Set([
    'root',
    'sibling',
    'uncle',
    'prev',
    'next',
    'index',
    'this',
]);

// Control / structural keywords actually consumed by the parser. Source
// of truth: `crates/relon-parser/src/expr.rs` (`where` / `match` heads),
// `structure/list.rs` (`for ... in ...` + comprehension guard `if`),
// `directive.rs` (`as` alias, `with` for `#extend ... with { ... }`).
// Deliberately NO `else` / `return` / `let` / `fn` — those are not
// tokens in Relon today (the few occurrences in fixtures are inside
// comments).
const CONTROL_KEYWORDS = new Set([
    'where',
    'match',
    'for',
    'in',
    'if',
    'as',
    'with',
]);

// Canonical builtin type names. Source of truth:
// `crates/relon-parser/src/token.rs::is_builtin_type_name` plus the
// names treated as builtin carriers by the analyzer (`Iter` core
// schema, `Option` / `Result` main-signature wrappers, and the
// `Bytes` / `Date` / `Time` / `DateTime` extend-allowed set).
const BUILTIN_TYPES = new Set([
    'Int',
    'Float',
    'Number',
    'String',
    'Bool',
    'Null',
    'Any',
    'List',
    'Dict',
    'Tuple',
    'Enum',
    'Closure',
    'Fn',
    'Iter',
    'Option',
    'Result',
    'Bytes',
    'Date',
    'Time',
    'DateTime',
]);

// Multi-char operators tried in order (longest first to avoid ambiguity
// between `=` / `==`, `<` / `<=`, etc.). Source of truth:
// `crates/relon-parser/src/source.rs` op table + the ternary `?`,
// optional-access `?.` / `?[`, and spread `...` consumed elsewhere.
const MULTI_CHAR_OPERATORS = [
    '...',
    '==',
    '!=',
    '<=',
    '>=',
    '&&',
    '||',
    '++',
    '=>',
    '->',
    '?.',
    '?[',
];

// Single-char operators / pipe / ternary punctuator.
const SINGLE_CHAR_OPERATORS = new Set([
    '+',
    '-',
    '*',
    '/',
    '%',
    '<',
    '>',
    '!',
    '?',
    '|',
]);

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

        // `&<base>`: reference prefix. Only the prefix is the special
        // token — any trailing `.field` walks through the identifier
        // path on subsequent token calls, which is what we want (path
        // segments highlight as plain identifiers).
        if (stream.peek() === '&') {
            const match = stream.match(/^&([A-Za-z_][A-Za-z0-9_]*)/);
            if (match) {
                const base = (match as RegExpMatchArray)[1];
                if (REFERENCE_BASES.has(base)) {
                    return 'variableName.special';
                }
                // Unknown `&xxx` — treat as bareword so a future
                // language addition shows up as identifier rather than
                // silently inheriting reference styling.
                return 'variableName';
            }
            stream.next();
            return null;
        }

        // Numbers (int / float, optional leading `-` is left to the
        // operator path so `1-2` doesn't get glued).
        if (stream.match(/^\d+(\.\d+)?([eE][+-]?\d+)?/)) {
            return 'number';
        }

        // Multi-char operators first so we don't split `==` into `=` `=`.
        for (const op of MULTI_CHAR_OPERATORS) {
            if (stream.match(op)) {
                return 'operator';
            }
        }

        // Identifiers / keywords-by-value / builtin types.
        if (stream.match(/^[A-Za-z_][A-Za-z0-9_]*/)) {
            const word = stream.current() as string;
            const lower = word.toLowerCase();
            if (lower === 'true' || lower === 'false' || lower === 'null') {
                return 'atom';
            }
            if (CONTROL_KEYWORDS.has(word)) {
                return 'controlKeyword';
            }
            if (BUILTIN_TYPES.has(word)) {
                return 'typeName';
            }
            return 'variableName';
        }

        // Single-char operators / punctuation.
        const ch = stream.peek();
        if (ch && SINGLE_CHAR_OPERATORS.has(ch)) {
            stream.next();
            return 'operator';
        }

        // Everything else (braces, brackets, commas, colons, dots, ...).
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

// Class-based highlight style. We deliberately avoid hard-coded `color`
// values here because hex literals can't react to VitePress's
// `:root.dark` theme toggle — a dark-mode visitor would see saturated
// `#aa1111` on a near-black background, which is what triggered the
// original "the colors look terrible" report. The classes resolve to
// CSS variables defined in `PlaygroundClient.vue`'s scoped style, so
// the palette swaps automatically with the documentation theme.
//
// Shared with the JSON output pane via `playgroundHighlightStyle` —
// keeping one palette across both editors avoids the eye-jarring
// contrast of two unrelated colour systems sitting side by side.
export const playgroundHighlightStyle = HighlightStyle.define([
    { tag: t.comment, class: 'cm-r-comment' },
    { tag: t.string, class: 'cm-r-string' },
    { tag: t.number, class: 'cm-r-number' },
    { tag: t.atom, class: 'cm-r-atom' },
    { tag: t.bool, class: 'cm-r-atom' },
    { tag: t.null, class: 'cm-r-atom' },
    { tag: t.keyword, class: 'cm-r-keyword' },
    { tag: t.controlKeyword, class: 'cm-r-keyword' },
    { tag: t.typeName, class: 'cm-r-type' },
    // `&root`, `&sibling`, ... — emitted via `variableName.special`.
    { tag: t.special(t.variableName), class: 'cm-r-ref' },
    // `@decorator` invocations.
    { tag: t.meta, class: 'cm-r-meta' },
    // Arithmetic / comparison / logical / arrow / spread. `defaultHighlightStyle`
    // ships no `tags.operator` rule, so without this they'd be unstyled.
    { tag: t.operator, class: 'cm-r-operator' },
    // JSON-specific: object keys.
    { tag: t.propertyName, class: 'cm-r-property' },
    { tag: t.definition(t.propertyName), class: 'cm-r-property' },
]);

export function relonLanguage(): LanguageSupport {
    return new LanguageSupport(StreamLanguage.define(parser), [
        syntaxHighlighting(playgroundHighlightStyle),
    ]);
}
