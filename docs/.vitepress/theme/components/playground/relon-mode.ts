// CodeMirror 6 syntax highlighting for Relon.
//
// Implemented as a `StreamLanguage` simple tokenizer rather than a Lezer
// grammar — keeps the dependency surface narrow (no `@lezer/lr` build
// step) and is plenty for read-friendly highlighting. If we later want
// folding / structural navigation we can graduate to Lezer; the
// `<RelonEditor>` consumer only sees a `LanguageSupport`.
//
// Tokens emitted (mapped to standard CodeMirror tags via StreamLanguage):
//   - `keyword`              : `#schema`, `#enum`, `#main`, `#extend`, ... .
//   - `meta`                 : `@<ident>` decorator invocations.
//   - `string`               : `"..."` and `f"..."` (interpolation
//                              handled as plain string at this layer).
//   - `number`               : integers + floats.
//   - `comment`              : `// line` and `/* block */`.
//   - `atom`                 : `true` / `false`.
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
    indentService,
    indentUnit,
    foldService,
    type StreamParser,
} from '@codemirror/language';
import { EditorState } from '@codemirror/state';
import { tags as t } from '@lezer/highlight';

// Hash-prefixed keywords. Order is irrelevant; `Set.has` lookup.
const HASH_KEYWORDS = new Set([
    '#schema',
    '#enum',
    '#main',
    '#extend',
    '#derive',
    '#no_auto_derive',
    '#native',
    '#internal',
    '#brand',
    '#import',
]);

// `@`-prefixed forms are user decorators in current Relon. Keep this set
// empty so old directive spellings are not highlighted as recommended syntax.
const AT_KEYWORDS = new Set<string>();

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
    // Paren depth inside the parameter list of a function definition
    // we have already identified via lookahead. 0 = not inside a def's
    // params. The depth counter exists so that nested parens (e.g. a
    // default-value expression, if Relon ever grows them) don't prematurely
    // close param mode.
    defParamDepth: number;
    // We've just emitted a function-definition name and are waiting for
    // the next `(` to flip into `defParamDepth = 1`. Cleared once we
    // either consume that `(` or hit any non-whitespace that isn't `(`
    // (defensive — keeps us out of param mode if the lookahead was wrong).
    awaitingDefOpenParen: boolean;
}

const parser: StreamParser<RelonState> = {
    name: 'relon',

    startState(): RelonState {
        return { inBlockComment: false, defParamDepth: 0, awaitingDefOpenParen: false };
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

        // Hash-prefixed keyword: `#schema`, `#enum`, `#main`, ...
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

        // `@<ident>` is a user decorator in current Relon. Old directive
        // spellings are not treated as keywords here.
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
            if (lower === 'true' || lower === 'false') {
                return 'atom';
            }
            if (CONTROL_KEYWORDS.has(word)) {
                return 'controlKeyword';
            }
            if (BUILTIN_TYPES.has(word)) {
                return 'typeName';
            }
            // Inside the parameter list of a function definition, bare
            // identifiers are formal parameters — give them a distinct
            // token so the highlighter can italicise them.
            if (state.defParamDepth > 0) {
                return 'variableName.local';
            }
            // Function name detection. We use a single lookahead at the
            // remainder of the current line; multi-line signatures aren't
            // a thing in Relon definitions today.
            //
            // Definition: `name(p1, p2): ...` — only triggers when the
            // paren contents are a comma-separated bareword list (or
            // empty), and a `:` follows the close paren. That keeps
            // expression-like call args (`multiply(&sibling.x, 1.2)`)
            // from getting misclassified as definitions.
            //
            // Call: `name(...` — any open-paren immediately after the
            // identifier. Calls and definitions share the same colour;
            // the parameter highlight is what's gated on a real def.
            const rest = stream.string.slice(stream.pos);
            const isDef = /^\(\s*([A-Za-z_][A-Za-z0-9_]*(\s*,\s*[A-Za-z_][A-Za-z0-9_]*)*)?\s*\)\s*:/.test(rest);
            if (isDef) {
                state.awaitingDefOpenParen = true;
                return 'variableName.function';
            }
            if (rest.startsWith('(')) {
                return 'variableName.function';
            }
            return 'variableName';
        }

        // Parens drive the function-definition param-mode counter. We
        // still emit `null` (no specific token) so the visual style of
        // brackets is unchanged — only the state machine cares.
        const ch = stream.peek();
        if (ch === '(') {
            stream.next();
            if (state.awaitingDefOpenParen) {
                state.defParamDepth = 1;
                state.awaitingDefOpenParen = false;
            } else if (state.defParamDepth > 0) {
                state.defParamDepth += 1;
            }
            return null;
        }
        if (ch === ')') {
            stream.next();
            if (state.defParamDepth > 0) {
                state.defParamDepth -= 1;
            }
            return null;
        }

        // Single-char operators / punctuation.
        if (ch && SINGLE_CHAR_OPERATORS.has(ch)) {
            stream.next();
            return 'operator';
        }

        // Everything else (braces, brackets, commas, colons, dots, ...).
        // If we were waiting for a def's `(` but hit something else
        // first, abandon the expectation — keeps stray tokens from
        // accidentally enrolling later parens into param mode.
        if (state.awaitingDefOpenParen) {
            state.awaitingDefOpenParen = false;
        }
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
    { tag: t.keyword, class: 'cm-r-keyword' },
    { tag: t.controlKeyword, class: 'cm-r-keyword' },
    { tag: t.typeName, class: 'cm-r-type' },
    // `&root`, `&sibling`, ... — emitted via `variableName.special`.
    { tag: t.special(t.variableName), class: 'cm-r-ref' },
    // Function names (definition + call site) — emitted via
    // `variableName.function`.
    { tag: t.function(t.variableName), class: 'cm-r-function' },
    // Formal parameters inside a function definition — emitted via
    // `variableName.local`.
    { tag: t.local(t.variableName), class: 'cm-r-param' },
    // `@decorator` invocations.
    { tag: t.meta, class: 'cm-r-meta' },
    // Arithmetic / comparison / logical / arrow / spread. `defaultHighlightStyle`
    // ships no `tags.operator` rule, so without this they'd be unstyled.
    { tag: t.operator, class: 'cm-r-operator' },
    // JSON-specific: object keys.
    { tag: t.propertyName, class: 'cm-r-property' },
    { tag: t.definition(t.propertyName), class: 'cm-r-property' },
]);

/// Bracket-aware indent rule. Looks at the previous non-blank line:
///   - ends with an unclosed `{` / `[` / `(` → indent one level deeper
///   - otherwise → match the previous line's indent
/// Then dedents the current line one level when it starts with
/// `}` / `]` / `)`. Mirrors what every JS / Rust / Python IDE does.
const relonIndent = indentService.of((context, pos) => {
    const doc = context.state.doc;
    const lineAtPos = doc.lineAt(pos);

    // CM6 calls indent services in two distinct modes:
    //
    //  - `simulatedBreak === pos` (the Enter path via
    //    `insertNewlineAndIndent`): `pos` sits at the end of the line
    //    the user just pressed Enter on. We're computing the indent
    //    for the *new* line that will appear below. Treat `lineAtPos`
    //    itself as the previous line, with no current-line text to
    //    consider for dedent.
    //
    //  - otherwise (Tab / indentSelection / indent-on-input): `pos`
    //    is on the line to indent. Walk back to find the real
    //    previous line, and consider `lineAtPos.text` for dedent.
    //
    // Conflating the two modes was the original bug: pressing Enter
    // after `details: {` was reading the line *above* `details: {`,
    // so the `{` was invisible and the indent never advanced.
    const isBreak = context.simulatedBreak === pos;
    let prevText: string;
    let currentLineText: string;
    if (isBreak) {
        prevText = lineAtPos.text;
        currentLineText = '';
    } else {
        if (lineAtPos.number <= 1) return 0;
        let prevLineNo = lineAtPos.number - 1;
        let walked = doc.line(prevLineNo).text;
        while (prevLineNo > 1 && walked.trim() === '') {
            prevLineNo -= 1;
            walked = doc.line(prevLineNo).text;
        }
        prevText = walked;
        currentLineText = lineAtPos.text;
    }

    const prevIndent = prevText.match(/^\s*/)?.[0].length ?? 0;
    // Strip trailing line comments + whitespace to see the line's
    // last syntactic character.
    const stripped = prevText.replace(/\/\/.*$/, '').trimEnd();
    let target = prevIndent;
    if (/[{[(]$/.test(stripped)) {
        target += context.unit;
    }

    if (/^\s*[}\])]/.test(currentLineText)) {
        target = Math.max(0, target - context.unit);
    }
    return target;
});

/// Code folding driven by bracket pairs. When a line contains an
/// unbalanced `{`, `[`, or `(`, the fold gutter offers to collapse
/// from that bracket to its match. Walks the doc one byte at a
/// time tracking the matching depth — good enough for the kinds
/// of nested dicts / lists / closures Relon emits.
const relonFold = foldService.of((state, lineStart, lineEnd) => {
    const doc = state.doc;
    const lineText = doc.sliceString(lineStart, lineEnd);
    // Find the LAST bracket opener on this line — folding starts
    // there so an opener mid-line still works.
    let openIdx = -1;
    let openChar = '';
    for (let i = lineText.length - 1; i >= 0; i--) {
        const c = lineText[i];
        if (c === '{' || c === '[' || c === '(') {
            openIdx = i;
            openChar = c;
            break;
        }
    }
    if (openIdx < 0) return null;
    const closeChar = openChar === '{' ? '}' : openChar === '[' ? ']' : ')';
    let depth = 1;
    const total = doc.length;
    let pos = lineStart + openIdx + 1;
    while (pos < total) {
        const ch = doc.sliceString(pos, pos + 1);
        if (ch === openChar) depth++;
        else if (ch === closeChar) {
            depth--;
            if (depth === 0) {
                return { from: lineStart + openIdx + 1, to: pos };
            }
        }
        pos++;
    }
    return null;
});

export function relonLanguage(): LanguageSupport {
    return new LanguageSupport(StreamLanguage.define(parser), [
        syntaxHighlighting(playgroundHighlightStyle),
        // Relon files indent with 4 spaces; CodeMirror's default is
        // 2, which clashes visually with the rest of any handwritten
        // file. Override the facet here so every consumer (the
        // indent service below, `indentWithTab`, `indentOnInput`)
        // pulls 4-space units.
        indentUnit.of('    '),
        relonIndent,
        relonFold,
        // `indentOnInput`, `commentTokens`, and `closeBrackets` are
        // language-data facets the matching extensions read off the
        // active language. Setting them here scopes the behavior to
        // Relon files instead of the whole editor.
        EditorState.languageData.of(() => [{
            indentOnInput: /^\s*[}\])]$/,
            commentTokens: { line: '//', block: { open: '/*', close: '*/' } },
            closeBrackets: { brackets: ['(', '[', '{', '"', "'"] },
        }]),
    ]);
}
