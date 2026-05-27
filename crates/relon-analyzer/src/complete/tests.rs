use super::*;
use crate::analyze;
use relon_parser::parse_document;

fn complete_at(source: &str, line: u32, character: u32) -> Vec<CompletionItem> {
    let root = parse_document(source).expect("parse");
    let tree = analyze(&root);
    resolve(source, &root, &tree, None, line, character)
}

fn labels_with_kind(items: &[CompletionItem], kind: CompletionKind) -> Vec<String> {
    items
        .iter()
        .filter(|i| i.kind == kind)
        .map(|i| i.label.clone())
        .collect()
}

fn labels(items: &[CompletionItem]) -> Vec<String> {
    items.iter().map(|i| i.label.clone()).collect()
}

#[test]
fn suggests_sibling_pair_keys_inside_dict() {
    // Cursor sits inside `baz`'s value (the `3` literal).
    let src = "{\n    foo: 1,\n    bar: 2,\n    baz: 3\n}\n";
    let items = complete_at(src, 3, 9);
    let names = labels(&items);
    assert!(names.iter().any(|l| l == "foo"), "{names:?}");
    assert!(names.iter().any(|l| l == "bar"), "{names:?}");
    assert!(names.iter().any(|l| l == "baz"), "{names:?}");
}

#[test]
fn suggests_closure_params_inside_body() {
    // Cursor sits on the `b` token of `a + b`.
    let src = "{\n    add(a, b): a + b\n}\n";
    let items = complete_at(src, 1, 21);
    let params = labels_with_kind(&items, CompletionKind::Parameter);
    assert!(params.contains(&"a".to_string()), "{params:?}");
    assert!(params.contains(&"b".to_string()), "{params:?}");
}

#[test]
fn directive_context_suggests_directive_names() {
    // Cursor between `#` and `schema` (offset 1). classify_cursor
    // sees prev byte `#` → Directive context.
    let src = "#schema User { String name: * }\n\n{\n    x: 1\n}\n";
    let items = complete_at(src, 0, 1);
    let names = labels_with_kind(&items, CompletionKind::Directive);
    assert!(names.contains(&"schema".to_string()), "{names:?}");
    assert!(names.contains(&"main".to_string()), "{names:?}");
    let pragmas = labels_with_kind(&items, CompletionKind::Pragma);
    assert!(pragmas.contains(&"internal".to_string()), "{pragmas:?}");
    // Should NOT include unrelated stdlib names in `#` context.
    let stdlib = labels_with_kind(&items, CompletionKind::Stdlib);
    assert!(
        stdlib.is_empty(),
        "stdlib should not appear after `#`: {stdlib:?}"
    );
}

#[test]
fn reference_context_suggests_ref_vars() {
    // `&root` reference; cursor right after `&` (offset 8 on line 1).
    let src = "{\n    x: &root\n}\n";
    let items = complete_at(src, 1, 9);
    let refs = labels_with_kind(&items, CompletionKind::Reference);
    assert!(refs.contains(&"root".to_string()), "{refs:?}");
    assert!(refs.contains(&"sibling".to_string()), "{refs:?}");
    // No iteration refs outside a list.
    assert!(!refs.contains(&"prev".to_string()), "{refs:?}");
}

#[test]
fn reference_context_inside_list_includes_iteration_refs() {
    // `&this` inside a list literal; cursor right after the `&`.
    // Source layout:
    //   line 0:  `{`
    //   line 1:  `xs: [&this]`   (no leading indent)
    //   line 2:  `}`
    // Cursor at (1, 6) — byte position right after the `&`.
    let src = "{\nxs: [&this]\n}\n";
    let items = complete_at(src, 1, 6);
    let refs = labels_with_kind(&items, CompletionKind::Reference);
    assert!(refs.contains(&"prev".to_string()), "{refs:?}");
    assert!(refs.contains(&"index".to_string()), "{refs:?}");
}

#[test]
fn bare_context_includes_stdlib() {
    // Cursor on the `1` value.
    let src = "{\n    foo: 1\n}\n";
    let items = complete_at(src, 1, 10);
    let names = labels_with_kind(&items, CompletionKind::Stdlib);
    assert!(names.contains(&"len".to_string()), "{names:?}");
}

#[test]
fn bare_context_includes_schema_names() {
    let src = "#schema User { String name: * }\n\n{\n    x: 1\n}\n";
    // Cursor on `1` in the file body.
    let items = complete_at(src, 3, 7);
    let schemas = labels_with_kind(&items, CompletionKind::Schema);
    assert!(schemas.contains(&"User".to_string()), "{schemas:?}");
}

#[test]
fn bare_context_does_not_offer_directive_names() {
    let src = "{\n    foo: 1\n}\n";
    let items = complete_at(src, 1, 10);
    let dirs = labels_with_kind(&items, CompletionKind::Directive);
    assert!(
        dirs.is_empty(),
        "directives leaked into bare context: {dirs:?}"
    );
}

#[test]
fn import_alias_seeds_module_label() {
    let src = "#import lib from \"./lib.relon\"\n\n{\n    x: 1\n}\n";
    let items = complete_at(src, 3, 7);
    let modules = labels_with_kind(&items, CompletionKind::Module);
    assert!(modules.contains(&"lib".to_string()), "{modules:?}");
}

#[test]
fn destructure_import_seeds_binding_labels() {
    let src = "#import { foo, bar as baz } from \"./lib.relon\"\n\n{\n    x: 1\n}\n";
    let items = complete_at(src, 3, 7);
    let imports = labels_with_kind(&items, CompletionKind::Import);
    assert!(imports.contains(&"foo".to_string()), "{imports:?}");
    assert!(imports.contains(&"baz".to_string()), "{imports:?}");
    // Original `bar` (without alias) shouldn't show — only the
    // visible local binding.
    assert!(!imports.contains(&"bar".to_string()), "{imports:?}");
}

#[test]
fn keywords_for_cursor_directive_works_without_parse() {
    // A bare `#` on its own line doesn't parse — this is the
    // mid-edit state right after the user types `#`. The
    // parse-free fallback still emits directive names.
    let src = "// header\n\n#\n\n{ x: 1 }\n";
    // Cursor right after the `#` on line 2.
    let items = keywords_for_cursor(src, 2, 1);
    let names: Vec<String> = items
        .iter()
        .filter(|i| i.kind == CompletionKind::Directive)
        .map(|i| i.label.clone())
        .collect();
    assert!(names.contains(&"schema".to_string()), "{names:?}");
    assert!(names.contains(&"main".to_string()), "{names:?}");
    assert!(names.contains(&"import".to_string()), "{names:?}");
    let pragmas: Vec<String> = items
        .iter()
        .filter(|i| i.kind == CompletionKind::Pragma)
        .map(|i| i.label.clone())
        .collect();
    assert!(pragmas.contains(&"internal".to_string()), "{pragmas:?}");
}

#[test]
fn keywords_for_cursor_reference_works_without_parse() {
    // Mid-edit: bare `&` with no AST yet.
    let src = "&";
    let items = keywords_for_cursor(src, 0, 1);
    let refs: Vec<String> = items
        .iter()
        .filter(|i| i.kind == CompletionKind::Reference)
        .map(|i| i.label.clone())
        .collect();
    assert!(refs.contains(&"root".to_string()), "{refs:?}");
    assert!(refs.contains(&"sibling".to_string()), "{refs:?}");
    // Without AST we can't know if cursor is inside a list →
    // iteration-only refs are suppressed.
    assert!(!refs.contains(&"prev".to_string()), "{refs:?}");
}

#[test]
fn closure_body_sees_sibling_methods() {
    // Inside `multiply`'s body (`a * b`), both `currency` (sibling
    // method) and `multiply`'s own params should be in scope.
    let src = "{\n    currency(s, v): s + v,\n    multiply(a, b): a * b\n}\n";
    let items = complete_at(src, 2, 21);
    let names = labels(&items);
    assert!(names.contains(&"currency".to_string()), "{names:?}");
    assert!(names.contains(&"a".to_string()), "{names:?}");
    assert!(names.contains(&"b".to_string()), "{names:?}");
}

fn complete_recovering(source: &str, line: u32, character: u32) -> Vec<CompletionItem> {
    let parsed = relon_parser::parse_document_recovering(source);
    resolve_recovering(source, &parsed, line, character)
}

#[test]
fn recovering_at_decorator_surfaces_sibling_closures() {
    // The original user complaint: typing `@` inside a dict with
    // sibling closures should surface those closures as decorator
    // candidates, not return empty.
    let src = "{\n    fmt(v): v + 1,\n    @\n    name: \"x\"\n}\n";
    // Cursor right after the `@` on line 2 (UTF-16 character index).
    let items = complete_recovering(src, 2, 5);
    let decorators = labels_with_kind(&items, CompletionKind::Decorator);
    assert!(
        decorators.contains(&"fmt".to_string()),
        "expected `fmt` sibling closure as decorator candidate, got {decorators:?}"
    );
}

#[test]
fn recovering_at_hash_surfaces_directive_names() {
    // Standalone `#` mid-edit — should always offer the full
    // directive set even without a partial AST root.
    let src = "#";
    let items = complete_recovering(src, 0, 1);
    let dirs = labels_with_kind(&items, CompletionKind::Directive);
    assert!(dirs.contains(&"schema".to_string()), "{dirs:?}");
    assert!(dirs.contains(&"import".to_string()), "{dirs:?}");
}

#[test]
fn recovering_at_amp_surfaces_reference_bases() {
    let src = "&";
    let items = complete_recovering(src, 0, 1);
    let refs = labels_with_kind(&items, CompletionKind::Reference);
    assert!(refs.contains(&"root".to_string()), "{refs:?}");
    assert!(refs.contains(&"sibling".to_string()), "{refs:?}");
}

#[test]
fn recovering_member_dot_surfaces_dict_keys() {
    // User types `parent.│` where `parent` is a sibling dict.
    // The partial-AST member walker should surface `parent`'s
    // keys (one closure → Method, one literal → Field).
    let src = "{\n    parent: {\n        greet(): \"hi\",\n        nickname: \"jojo\"\n    },\n    child: parent.\n}\n";
    // Cursor immediately after the `parent.` on line 5 (0-indexed),
    // character 18 = end of `    child: parent.`.
    let items = complete_recovering(src, 5, 18);
    let names = labels(&items);
    assert!(
        names.contains(&"greet".to_string()),
        "expected `greet` method via member access, got {names:?}"
    );
    assert!(
        names.contains(&"nickname".to_string()),
        "expected `nickname` field via member access, got {names:?}"
    );
}

#[test]
fn recovering_fstring_interp_sees_scope() {
    // Inside `${...}`, scope candidates (siblings, closure params)
    // should be available — same as bare context.
    let src = "{\n    name: \"world\",\n    greeting: f\"hi ${\n}\n";
    // Cursor right after the `${` on line 2 (UTF-16 index 19).
    let items = complete_recovering(src, 2, 19);
    let names = labels(&items);
    assert!(
        names.contains(&"name".to_string()),
        "expected sibling `name` inside f-string interp, got {names:?}"
    );
}

#[test]
fn recovering_generic_args_surfaces_primitives_and_schemas() {
    // Cursor inside `Foo<│>` — should surface primitives + any
    // visible schema names.
    let src = "#schema Box<T> T\n\n{\n    items: Foo<\n}\n";
    // Cursor right after the `<` on line 3, character 16.
    let items = complete_recovering(src, 3, 16);
    let names = labels(&items);
    assert!(
        names.contains(&"String".to_string()),
        "expected primitive `String` in generic args, got {names:?}"
    );
    assert!(names.contains(&"Int".to_string()), "{names:?}");
}

#[test]
fn recovering_closure_return_after_arrow_surfaces_types() {
    // `(x) -> │` — closure return type position.
    let src = "{\n    f: (x) -> \n}\n";
    let items = complete_recovering(src, 1, 16);
    let names = labels(&items);
    assert!(
        names.contains(&"String".to_string()),
        "expected `String` after `->`, got {names:?}"
    );
}

#[test]
fn recovering_typed_spread_after_star_surfaces_types() {
    // `{ *│ }` — typed-spread head position.
    let src = "{\n    *\n}\n";
    let items = complete_recovering(src, 1, 5);
    let names = labels(&items);
    assert!(
        names.contains(&"Int".to_string()),
        "expected `Int` after `*`, got {names:?}"
    );
}

#[test]
fn recovering_bare_inside_dict_surfaces_siblings() {
    // Mid-edit dict with a partially typed identifier as a value.
    let src = "{\n    foo: 1,\n    bar: 2,\n    baz: f\n}\n";
    // Cursor right after the `f` on line 3.
    let items = complete_recovering(src, 3, 10);
    let names = labels(&items);
    assert!(
        names.contains(&"foo".to_string()),
        "expected sibling `foo` in bare scope, got {names:?}"
    );
    assert!(names.contains(&"bar".to_string()), "{names:?}");
}
