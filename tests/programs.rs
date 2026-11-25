//! Runs every program under `examples/` and `tests/errors/`.
//!
//! - `examples/*.yui` must type-check, pass all their tests and run `main`.
//! - `tests/errors/*.yui` start with `// expect: E0xxx` and must fail with that code,
//!   either while loading or while running their tests and `main`.

use std::path::{Path, PathBuf};
use yuilang::ast::Item;
use yuilang::interp::Interp;

fn yui_files(dir: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "yui"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .yui files in {}", root.display());
    files
}

/// The interpreter recurses on the native stack, so run it like the `yui` binary does.
fn with_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

/// Loads and runs a program. Returns the code of the first error, if any.
fn first_error(src: &str) -> Option<&'static str> {
    let program = match yuilang::load(src) {
        Ok(p) => p,
        Err(diags) => return Some(diags[0].code),
    };
    let mut interp = Interp::new(&program, src);
    interp.captured = Some(String::new());
    for item in &program.items {
        if let Item::Test(t) = item {
            if let Err(d) = interp.run_test(t) {
                return Some(d.code);
            }
        }
    }
    if yuilang::check_main(&program).is_ok() {
        if let Err(d) = interp.run_main() {
            return Some(d.code);
        }
    }
    None
}

#[test]
fn examples_pass() {
    with_big_stack(|| {
        for path in yui_files("examples") {
            let src = std::fs::read_to_string(&path).unwrap();
            assert_eq!(first_error(&src), None, "{}", path.display());
        }
    });
}

#[test]
fn error_cases_report_expected_code() {
    with_big_stack(|| {
        for path in yui_files("tests/errors") {
            let src = std::fs::read_to_string(&path).unwrap();
            let expected = src
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("// expect: "))
                .unwrap_or_else(|| panic!("{} must start with `// expect: E0xxx`", path.display()))
                .trim();
            assert_eq!(first_error(&src), Some(expected), "{}", path.display());
        }
    });
}

#[test]
fn print_output_is_captured() {
    let src = "fn main() uses io {\n  print(\"a{1 + 1}\")\n  print(\"{[1.0, 2.5]}\")\n}\n";
    let program = yuilang::load(src).expect("program loads");
    let mut interp = Interp::new(&program, src);
    interp.captured = Some(String::new());
    interp.run_main().expect("main runs");
    assert_eq!(interp.captured.as_deref(), Some("a2\n[1.0, 2.5]\n"));
}
