use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_relon-cli");

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root above crates/relon-cli")
}

fn stdlib_manifest_names(root: &Path, marker: &str) -> BTreeSet<String> {
    let path = root.join("docs/en/guide/stdlib.md");
    let doc =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let start = format!("<!-- {marker}:start -->");
    let end = format!("<!-- {marker}:end -->");
    let section = doc
        .split_once(&start)
        .and_then(|(_, rest)| rest.split_once(&end).map(|(section, _)| section))
        .unwrap_or_else(|| panic!("stdlib.md must contain {marker} markers"));

    let names: BTreeSet<String> = section
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("| `")?;
            let (name, _) = rest.split_once('`')?;
            Some(name.to_string())
        })
        .collect();
    assert!(!names.is_empty(), "{marker} listed no names");
    names
}

fn visit_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let entry = entry.expect("read_dir entry");
        let path = entry.path();
        if path.is_dir() {
            visit_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn diagnostic_codes_in_source(root: &Path) -> BTreeSet<String> {
    let mut files = Vec::new();
    visit_rs_files(&root.join("crates"), &mut files);

    let mut codes = BTreeSet::new();
    const MARKER: &str = concat!("code(", "relon::");
    for path in files {
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut rest = src.as_str();
        while let Some(pos) = rest.find(MARKER) {
            let code_tail = &rest[pos + "code(".len()..];
            let Some(end) = code_tail.find(')') else {
                panic!("unterminated diagnostic code marker in {}", path.display());
            };
            let code: String = code_tail[..end].split_whitespace().collect();
            codes.insert(code);
            rest = &code_tail[end + 1..];
        }
    }

    codes
}

fn namespace_wildcard(code: &str) -> String {
    let mut parts = code.split("::");
    assert_eq!(
        parts.next(),
        Some("relon"),
        "diagnostic code must start with relon::"
    );
    let subsystem = parts
        .next()
        .unwrap_or_else(|| panic!("diagnostic code has no subsystem: {code}"));
    format!("relon::{subsystem}::*")
}

#[test]
fn stable_user_stdlib_manifest_matches_first_release_surface() {
    let root = workspace_root();
    let actual = stdlib_manifest_names(&root, "relon-stdlib-user-manifest");
    let expected: BTreeSet<String> = [
        "len",
        "range",
        "type",
        "std/list.map",
        "std/list.filter",
        "std/list.reduce",
        "std/list.contains",
        "std/list.sum",
        "std/list.avg",
        "std/list.len",
        "std/list.first",
        "std/list.last",
        "std/list.compact",
        "std/list.flatten",
        "std/dict.merge",
        "std/dict.keys",
        "std/dict.values",
        "std/dict.has_key",
        "std/string.split",
        "std/string.join",
        "std/string.replace",
        "std/string.upper",
        "std/string.lower",
        "std/string.contains",
        "std/math.abs",
        "std/math.max",
        "std/math.min",
        "std/math.clamp",
        "std/is.int",
        "std/is.string",
        "std/is.bool",
        "std/is.float",
        "std/is.list",
        "std/is.dict",
        "std/is.number",
        "std/is.empty",
        "std/value.default",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(
        actual, expected,
        "stable user stdlib manifest must match the first-release API surface"
    );
    assert!(
        actual.iter().all(|name| !name.starts_with('_')),
        "stable user stdlib manifest must not expose implementation intrinsics"
    );
    assert!(
        !actual.contains("std/string.glob_match"),
        "std/string.glob_match remains legacy/runtime-only until promoted"
    );
}

#[test]
fn spec_std_string_catalog_matches_stable_manifest() {
    let root = workspace_root();
    for path in ["docs/en/guide/spec.md", "docs/zh/guide/spec.md"] {
        let full_path = root.join(path);
        let text = std::fs::read_to_string(&full_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", full_path.display()));
        let row = text
            .lines()
            .find(|line| line.starts_with("| `std/string` |"))
            .unwrap_or_else(|| panic!("{path} must list a std/string catalog row"));
        assert!(
            !row.contains("glob_match"),
            "{path} must not list std/string.glob_match as stable stdlib"
        );
        assert!(
            !text.contains("stable-callable-manifest"),
            "{path} must not link to the retired stable-callable-manifest anchor"
        );
        assert!(
            text.contains("legacy/runtime-only"),
            "{path} must state that glob_match remains legacy/runtime-only"
        );
    }
}

#[test]
fn auto_trusted_first_release_limit_is_documented() {
    let root = workspace_root();
    for path in [
        "README.md",
        "CHANGELOG.md",
        "docs/en/guide/host-integration.md",
        "docs/en/guide/release-tiers.md",
        "docs/zh/guide/host-integration.md",
        "docs/zh/guide/release-tiers.md",
        ".github/ISSUE_TEMPLATE/release-checklist.md",
    ] {
        let full_path = root.join(path);
        let text = std::fs::read_to_string(&full_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", full_path.display()));
        assert!(
            text.contains("Backend::Auto + TrustLevel::Trusted"),
            "{path} must document the first-release Auto + Trusted limitation"
        );
        assert!(
            text.contains("Backend::TreeWalk"),
            "{path} must point trusted imports / staged host fns to Backend::TreeWalk"
        );
    }
}

#[test]
fn public_release_language_is_0_1_not_1_0_or_2_0() {
    let root = workspace_root();

    let docs_package_path = root.join("docs/package.json");
    let docs_package = std::fs::read_to_string(&docs_package_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", docs_package_path.display()));
    assert!(
        docs_package.contains(r#""version": "0.1.0-rc2""#),
        "docs/package.json must not imply a 1.0 docs package for the first public release"
    );
    assert!(
        docs_package.contains(r#""description": "Documentation for Relon 0.1 RC2""#),
        "docs/package.json description must match the 0.1 RC release posture"
    );
    assert!(
        !docs_package.contains("Relon 2.0"),
        "docs/package.json must not describe the first public release as Relon 2.0"
    );

    let docs_lock_path = root.join("docs/package-lock.json");
    let docs_lock = std::fs::read_to_string(&docs_lock_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", docs_lock_path.display()));
    assert!(
        docs_lock.starts_with("{\n  \"name\": \"relon-docs\",\n  \"version\": \"0.1.0-rc2\""),
        "docs/package-lock.json top-level relon-docs version must mirror docs/package.json"
    );
    assert!(
        docs_lock
            .contains("\"\": {\n      \"name\": \"relon-docs\",\n      \"version\": \"0.1.0-rc2\""),
        "docs/package-lock.json root package version must mirror docs/package.json"
    );

    for path in [
        "README.md",
        "docs/en/index.md",
        "docs/zh/index.md",
        "docs/en/guide/introduction.md",
        "docs/zh/guide/introduction.md",
        "docs/en/guide/release-tiers.md",
        "docs/zh/guide/release-tiers.md",
    ] {
        let full_path = root.join(path);
        let text = std::fs::read_to_string(&full_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", full_path.display()));
        for phrase in [
            "Relon 1.0",
            "Relon 2.0",
            "1.0 release",
            "1.0 stable",
            "version 1.0",
        ] {
            assert!(
                !text.contains(phrase),
                "{path} must not imply a 1.0/2.0 release posture via `{phrase}`"
            );
        }
    }
}

#[test]
fn diagnostics_doc_covers_miette_code_namespaces() {
    let root = workspace_root();
    let doc_path = root.join("docs/en/guide/diagnostics.md");
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", doc_path.display()));
    let codes = diagnostic_codes_in_source(&root);
    assert!(
        !codes.is_empty(),
        "source scan found no relon:: diagnostic codes"
    );

    let missing: Vec<String> = codes
        .iter()
        .filter(|code| {
            let namespace = namespace_wildcard(code);
            !doc.contains(code.as_str()) && !doc.contains(namespace.as_str())
        })
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "diagnostics.md does not document these diagnostic code namespaces or exact codes: \
         {missing:?}"
    );
}

#[test]
fn front_door_security_docs_link_to_threat_model() {
    let root = workspace_root();
    for path in [
        "README.md",
        "docs/en/index.md",
        "docs/en/guide/introduction.md",
        "docs/en/guide/use-cases.md",
        "docs/en/guide/playground.md",
        "docs/zh/index.md",
        "docs/zh/guide/introduction.md",
        "docs/zh/guide/use-cases.md",
        "docs/zh/guide/playground.md",
    ] {
        let full_path = root.join(path);
        let text = std::fs::read_to_string(&full_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", full_path.display()));
        assert!(
            text.contains("threat-model"),
            "{path} must link or point to the Threat Model"
        );
    }
}

#[test]
fn user_docs_avoid_overbroad_sandbox_claims() {
    let root = workspace_root();
    let forbidden = [
        "sandboxed runtime",
        "in-browser sandboxed runtime",
        "沙箱安全",
        "零风险运行",
        "完整沙箱",
    ];
    for path in ["README.md", "docs/en", "docs/zh"] {
        let full_path = root.join(path);
        let mut files = Vec::new();
        if full_path.is_dir() {
            collect_markdown_files(&full_path, &mut files);
        } else {
            files.push(full_path);
        }
        for file in files {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            for phrase in forbidden {
                assert!(
                    !text.contains(phrase),
                    "{} contains overbroad sandbox claim `{phrase}`",
                    file.display()
                );
            }
        }
    }
}

#[test]
fn diagnostic_output_examples_match_golden_text() {
    let cases = [
        DiagnosticCase {
            file_name: "parse.relon",
            source: "{ a: }",
            args: &["check", "{file}"],
            expected: r#"Error:   × Analyzer reported errors:
  │   - module parse error in `<TMP>/parse.relon`: parse error:
  │ expected expression"#,
        },
        DiagnosticCase {
            file_name: "type.relon",
            source: r#"{ Int port: "oops" }"#,
            args: &["check", "{file}"],
            expected: r#"Error:   × Analyzer reported errors:
  │   - [<TMP>/type.relon] static type mismatch in schema field
  │ `port`: expected Int, value is String"#,
        },
        DiagnosticCase {
            file_name: "remote_import.relon",
            source: "#import x from \"https://example.com/a.relon\"\n{ y: 1 }\n",
            args: &["check", "{file}"],
            expected: r#"Error:   × Analyzer reported errors:
  │   - module not found: https://example.com/a.relon (remote `#import`
  │ requires --trust (or Capabilities::network))"#,
        },
        DiagnosticCase {
            file_name: "missing_arg.relon",
            source: "#main(Int x) -> Int\nx\n",
            args: &["run", "{file}"],
            expected: r#"Error:   × File declares `#main(...)`; pass --args '<json>' (or --args -) to provide
  │ host arguments"#,
        },
        DiagnosticCase {
            file_name: "backend_unsupported.relon",
            source: "{ x: 1 }\n",
            args: &["check", "--backend", "cranelift-aot", "{file}"],
            expected: r#"ok: analyzer
Error:   × cranelift-aot: incompatible (cranelift-aot requires `#main(...)`)"#,
        },
        DiagnosticCase {
            file_name: "steps.relon",
            source: "#relaxed\n{ loop(): loop(), x: loop() }\n",
            args: &["run", "--backend", "tree-walk", "--max-steps", "10", "{file}"],
            expected: r#"Error: relon::eval::step_limit_exceeded

  × Step limit exceeded
   ╭─[<TMP>/steps.relon:2:11]
 1 │ #relaxed
 2 │ { loop(): loop(), x: loop() }
   ·           ───┬──
   ·              ╰── budget exhausted here
   ╰────
  help: The script ran longer than the configured `max_steps` / deadline
        budget. Raise `Capabilities::max_steps` or refactor recursive /
        iterative work."#,
        },
        DiagnosticCase {
            file_name: "schema.relon",
            source: "#schema C { #expect \"n positive\" Int n: (Int n) -> Bool => n > 0 }\n#main(C c) -> C\nc\n",
            args: &[
                "run",
                "--backend",
                "tree-walk",
                "{file}",
                "--args",
                r#"{"c":{"n":0}}"#,
            ],
            expected: r#"Error: relon::eval::main_arg_type_mismatch

  × type mismatch for `#main` arg `c`: expected n positive, found 0
   ╭─[<TMP>/schema.relon:2:9]
 1 │ #schema C { #expect "n positive" Int n: (Int n) -> Bool => n > 0 }
 2 │ #main(C c) -> C
   ·         ┬
   ·         ╰── type mismatch
 3 │ c
   ╰────"#,
        },
    ];

    for case in cases {
        let dir = tempfile::tempdir().expect("create diagnostic fixture tempdir");
        let path = dir.path().join(case.file_name);
        std::fs::write(&path, case.source)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));

        let mut command = Command::new(BINARY);
        strip_color_env(&mut command);
        for arg in case.args {
            if *arg == "{file}" {
                command.arg(&path);
            } else {
                command.arg(arg);
            }
        }

        let output = command
            .output()
            .unwrap_or_else(|e| panic!("spawn relon CLI for {}: {e}", case.file_name));
        assert!(
            !output.status.success(),
            "{} unexpectedly succeeded: stdout={}",
            case.file_name,
            String::from_utf8_lossy(&output.stdout)
        );

        let actual = normalize_diagnostic_output(&output.stdout, &output.stderr, dir.path());
        assert_eq!(
            actual,
            case.expected.trim_end(),
            "{} diagnostic output drifted",
            case.file_name
        );
    }
}

struct DiagnosticCase<'a> {
    file_name: &'a str,
    source: &'a str,
    args: &'a [&'a str],
    expected: &'a str,
}

/// Golden comparisons assume the CLI's default "no TTY, no color" output.
/// Color-forcing variables inherited from the test runner's shell
/// (`FORCE_COLOR=3` is common in CI and agent harnesses) would make miette
/// emit ANSI codes into the captured pipes and fail the byte comparison,
/// so strip every color knob before spawning.
fn strip_color_env(command: &mut Command) {
    for var in ["FORCE_COLOR", "NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
        command.env_remove(var);
    }
}

fn normalize_diagnostic_output(stdout: &[u8], stderr: &[u8], temp_dir: &Path) -> String {
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(stdout));
    text.push_str(&String::from_utf8_lossy(stderr));

    let mut text = text.replace("\r\n", "\n").replace('\r', "\n");
    let temp = temp_dir.to_string_lossy();
    text = text.replace(temp.as_ref(), "<TMP>");
    let temp_slash = temp.replace('\\', "/");
    text = text.replace(&temp_slash, "<TMP>");
    text.trim_end().to_string()
}

fn collect_markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let entry = entry.expect("read_dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_markdown_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

#[test]
fn example_headers_advertise_command_backend_and_golden() {
    let root = workspace_root();
    let examples_dir = root.join("examples");
    for entry in std::fs::read_dir(&examples_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", examples_dir.display()))
    {
        let path = entry.expect("read_dir entry").path();
        if path.extension().is_none_or(|ext| ext != "relon") {
            continue;
        }

        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            source.contains("Try:"),
            "{} missing `Try:` header",
            path.display()
        );
        assert!(
            source.contains("Recommended backend:"),
            "{} missing `Recommended backend:` header",
            path.display()
        );
        let expected = source
            .lines()
            .find_map(|line| line.trim().strip_prefix("Expected output:").map(str::trim))
            .unwrap_or_else(|| panic!("{} missing `Expected output:` header", path.display()));
        assert!(
            root.join(expected).exists(),
            "{} points to missing expected output path `{expected}`",
            path.display()
        );
    }
}

#[test]
fn example_commands_match_golden_outputs() {
    let root = workspace_root();
    let cases = [
        (
            "examples/demo.relon",
            None,
            "fixtures/golden/success/examples/demo.json",
        ),
        (
            "examples/validation.relon",
            None,
            "fixtures/golden/success/examples/validation.json",
        ),
        (
            "examples/feature_flag.relon",
            Some(r#"{"user":{"id":"alice-42","region":"eu","plan":"pro","rollout_bucket":17}}"#),
            "fixtures/golden/examples_main/feature_flag.json",
        ),
        (
            "examples/pricing.relon",
            Some(
                r#"{"order":{"tier":"gold","items":[{"sku":"BOOK-01","qty":3,"unit_cents":10000},{"sku":"PEN-09","qty":4,"unit_cents":5000},{"sku":"DESK-22","qty":1,"unit_cents":30000}]}}"#,
            ),
            "fixtures/golden/examples_main/pricing.json",
        ),
        (
            "examples/workflow.relon",
            Some(r#"{"input":{"state":"placed","event":"pay"}}"#),
            "fixtures/golden/examples_main/workflow.json",
        ),
    ];

    for (example, args, golden) in cases {
        let mut command = Command::new(BINARY);
        strip_color_env(&mut command);
        command
            .arg("run")
            .arg(root.join(example))
            .arg("--backend")
            .arg("tree-walk");
        if let Some(args) = args {
            command.arg("--args").arg(args);
        }

        let output = command
            .output()
            .unwrap_or_else(|e| panic!("spawn relon CLI for {example}: {e}"));
        assert!(
            output.status.success(),
            "{example} exited non-zero: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );

        let actual: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{example} stdout must be JSON: {e}"));
        let expected_raw = std::fs::read_to_string(root.join(golden))
            .unwrap_or_else(|e| panic!("read golden {golden}: {e}"));
        let expected: serde_json::Value = serde_json::from_str(&expected_raw)
            .unwrap_or_else(|e| panic!("golden {golden} must be JSON: {e}"));
        assert_eq!(actual, expected, "{example} output drifted from {golden}");
    }
}
