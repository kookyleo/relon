// Mod-hover underline + Mod-click "go to definition" for the Relon
// playground editor. Wraps the WASM `goto_definition` lookup in a
// CodeMirror 6 ViewPlugin so the interaction matches what users expect
// from VS Code / IntelliJ — hold Cmd/Ctrl, identifiers under the
// cursor that have a known definition pick up an underline and a
// pointer cursor; click jumps to the definition (switching tabs for
// cross-file targets).
//
// The wasm call is synchronous (already loaded by PlaygroundClient
// before this extension is wired in) so we can resolve on every
// mousemove without scheduling async work. To keep the per-move cost
// low, we cache the last (line, character) we resolved and only
// re-call wasm when the position changes.

import { EditorView, Decoration, type DecorationSet, ViewPlugin, type ViewUpdate } from '@codemirror/view';
import { StateField, StateEffect, type Extension } from '@codemirror/state';

export interface GotoDefinitionTarget {
    path: string;
    start: { line: number; character: number };
    end: { line: number; character: number };
}

/// Callback contract the playground supplies. Returns the target the
/// cursor at (line, character) of the active editor's source resolves
/// to — or null when the cursor isn't on a recognised reference. Must
/// be cheap (synchronous, ≤ a few ms): we call it on every Mod-held
/// mousemove that changes the cursor's character index.
export type Resolver = (line: number, character: number) => GotoDefinitionTarget | null;

/// Callback the playground supplies for cross-file jumps. The
/// extension calls this when the resolved target's `path` differs
/// from the editor's current entry. The playground switches to the
/// right tab, then the extension re-fires the selection dispatch on
/// the next animation frame (the editor view is recreated when the
/// active file changes, so we have to wait for the new view).
export type JumpHandler = (target: GotoDefinitionTarget) => void;

/// Decoration toggled when the Mod-key + cursor sits on a linkable
/// identifier. Uses an underline + pointer cursor to mirror IDE
/// affordances.
const linkMark = Decoration.mark({
    class: 'cm-gotodef-link',
    inclusive: false,
});

interface HighlightState {
    decorations: DecorationSet;
    /// (line, character) of the cursor position whose resolve hit.
    /// Cached so a mousemove that doesn't change the char index
    /// doesn't trigger another wasm call.
    cachedLine: number;
    cachedCharacter: number;
    /// `true` when the cached target was a hit (decoration present).
    cachedHit: boolean;
}

const setHighlight = StateEffect.define<DecorationSet>();

const highlightField = StateField.define<DecorationSet>({
    create() {
        return Decoration.none;
    },
    update(deco, tr) {
        let next = deco.map(tr.changes);
        for (const e of tr.effects) {
            if (e.is(setHighlight)) next = e.value;
        }
        return next;
    },
    provide: (f) => EditorView.decorations.from(f),
});

interface ExtensionOptions {
    /// Called on every Mod-held mousemove (after position dedup). The
    /// implementation should look the position up against wasm's
    /// `goto_definition` over the current sources map.
    resolve: Resolver;
    /// Called on Mod-click. The handler is responsible for switching
    /// tabs (when path != current entry) and dispatching the editor
    /// selection. We hand it the full target so it can compute byte
    /// offsets itself; the extension doesn't know the target file's
    /// source text.
    jump: JumpHandler;
}

/// Build the CodeMirror extension. Wire it into the editor's state
/// alongside `langCompartment.of(relonLanguage())` etc.
export function gotoDefinitionExtension(options: ExtensionOptions): Extension {
    return [
        highlightField,
        ViewPlugin.fromClass(
            class {
                private modHeld = false;
                private state: HighlightState = {
                    decorations: Decoration.none,
                    cachedLine: -1,
                    cachedCharacter: -1,
                    cachedHit: false,
                };
                private readonly view: EditorView;
                private readonly cleanup: Array<() => void> = [];

                constructor(view: EditorView) {
                    this.view = view;
                    const dom = view.dom;
                    const onKeyDown = (e: KeyboardEvent) => {
                        if (isModKey(e)) {
                            this.modHeld = true;
                        }
                    };
                    const onKeyUp = (e: KeyboardEvent) => {
                        if (isModKey(e)) {
                            this.modHeld = false;
                            this.clear();
                        }
                    };
                    const onBlur = () => {
                        // Lose the key state when focus leaves; a Mod that
                        // was held when the user alt-tabbed away may have
                        // been released without a keyup landing.
                        this.modHeld = false;
                        this.clear();
                    };
                    const onMouseMove = (e: MouseEvent) => {
                        // Use the *event's* modifier state rather than
                        // our tracked flag: covers the case where keydown
                        // arrived in a different DOM tree (focus shifts
                        // can swallow the editor's keydown).
                        const held = e.metaKey || e.ctrlKey;
                        this.modHeld = held;
                        if (!held) {
                            this.clear();
                            return;
                        }
                        const pos = view.posAtCoords({ x: e.clientX, y: e.clientY });
                        if (pos == null) {
                            this.clear();
                            return;
                        }
                        this.maybeResolve(pos);
                    };
                    const onMouseDown = (e: MouseEvent) => {
                        if (!(e.metaKey || e.ctrlKey)) return;
                        if (e.button !== 0) return;
                        const pos = view.posAtCoords({ x: e.clientX, y: e.clientY });
                        if (pos == null) return;
                        const { line, character } = posToLineChar(view, pos);
                        const target = options.resolve(line, character);
                        if (!target) return;
                        // Suppress the regular selection / caret update —
                        // we want a "jump" not a "move cursor here".
                        e.preventDefault();
                        e.stopPropagation();
                        options.jump(target);
                    };
                    dom.addEventListener('keydown', onKeyDown);
                    dom.addEventListener('keyup', onKeyUp);
                    dom.addEventListener('blur', onBlur, true);
                    dom.addEventListener('mousemove', onMouseMove);
                    dom.addEventListener('mousedown', onMouseDown, true);
                    this.cleanup.push(
                        () => dom.removeEventListener('keydown', onKeyDown),
                        () => dom.removeEventListener('keyup', onKeyUp),
                        () => dom.removeEventListener('blur', onBlur, true),
                        () => dom.removeEventListener('mousemove', onMouseMove),
                        () => dom.removeEventListener('mousedown', onMouseDown, true)
                    );
                }

                update(_u: ViewUpdate) {
                    // Decorations live in the StateField — nothing to do
                    // here, but keeping the hook around documents that
                    // we're aware of view updates and chose not to
                    // recompute on doc changes (the next mousemove will).
                }

                destroy() {
                    for (const fn of this.cleanup) fn();
                }

                private maybeResolve(docPos: number) {
                    const { line, character } = posToLineChar(this.view, docPos);
                    if (line === this.state.cachedLine && character === this.state.cachedCharacter) {
                        return;
                    }
                    this.state.cachedLine = line;
                    this.state.cachedCharacter = character;
                    const target = options.resolve(line, character);
                    if (!target) {
                        if (this.state.cachedHit) {
                            this.clear();
                        }
                        return;
                    }
                    // Underline the identifier the cursor is on. We
                    // approximate the identifier span by walking out
                    // from `docPos` in both directions until we hit a
                    // non-identifier char. This is local DOM math; it
                    // doesn't need to match the analyzer's token
                    // exactly, just look right.
                    const doc = this.view.state.doc;
                    const lineText = doc.lineAt(docPos);
                    const offsetInLine = docPos - lineText.from;
                    const text = lineText.text;
                    let start = offsetInLine;
                    let end = offsetInLine;
                    while (start > 0 && isIdentChar(text.charCodeAt(start - 1))) start--;
                    while (end < text.length && isIdentChar(text.charCodeAt(end))) end++;
                    if (end === start) {
                        // Cursor on a non-ident char (e.g. the dot in
                        // `lib.x`). Don't underline — but the click
                        // would still jump if the user committed.
                        this.clear();
                        return;
                    }
                    const decoration = Decoration.set([
                        linkMark.range(lineText.from + start, lineText.from + end),
                    ]);
                    this.state.cachedHit = true;
                    this.view.dispatch({ effects: setHighlight.of(decoration) });
                }

                private clear() {
                    if (!this.state.cachedHit) return;
                    this.state.cachedHit = false;
                    this.state.cachedLine = -1;
                    this.state.cachedCharacter = -1;
                    this.view.dispatch({ effects: setHighlight.of(Decoration.none) });
                }
            }
        ),
        EditorView.theme({
            '.cm-gotodef-link': {
                textDecoration: 'underline',
                cursor: 'pointer',
            },
        }),
    ];
}

function isModKey(e: KeyboardEvent): boolean {
    // Both Mac (Meta) and PC (Ctrl) qualify — matches the convention
    // CodeMirror's `Mod-` keybinding alias uses internally.
    return e.key === 'Meta' || e.key === 'Control';
}

function isIdentChar(code: number): boolean {
    // ASCII identifier characters: A–Z, a–z, 0–9, _. Conservative —
    // matches the Relon tokenizer's `[A-Za-z_][A-Za-z0-9_]*` rule.
    return (
        (code >= 48 && code <= 57) ||
        (code >= 65 && code <= 90) ||
        (code >= 97 && code <= 122) ||
        code === 95
    );
}

function posToLineChar(view: EditorView, pos: number): { line: number; character: number } {
    const lineObj = view.state.doc.lineAt(pos);
    // CodeMirror line numbers are 1-based; LSP is 0-based.
    const line = lineObj.number - 1;
    // `pos - lineObj.from` is a UTF-16 code-unit offset because
    // CodeMirror's Text doc is JS-string-indexed. That matches the
    // wasm function's character parameter.
    const character = pos - lineObj.from;
    return { line, character };
}
