//! Runs every program under `examples/` and `tests/errors/`, and checks the formatter.
//!
//! - `examples/*.yui` and `examples/modules/main.yui` must type-check, pass
//!   all their tests and run `main`.
//! - `tests/errors/*.yui` start with `// expect: E0xxx` and must fail with that code,
//!   either while loading or while running their tests and `main`.
//! - Every example is already in canonical format, and formatting is idempotent.

use std::path::{Path, PathBuf};
use yuilang::ast::Item;
use yuilang::interp::Interp;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn yui_files(dir: &str) -> Vec<PathBuf> {
    let dir = root().join(dir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "yui"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .yui files in {}", dir.display());
    files
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

/// Closures called from built-in methods recurse on the native stack, so run
/// programs like the `yui` binary does.
fn with_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

/// Loads and runs a program. Returns the code of the first error, if any.
/// `jit` false forces the bytecode interpreter; true uses machine code where
/// this platform supports it.
fn first_error(path: &Path, jit: bool) -> Option<&'static str> {
    let loaded = match yuilang::load_path(path, None) {
        Ok(l) => l,
        Err((diags, _)) => return Some(diags[0].code),
    };
    let mut interp = Interp::new(&loaded.program, &loaded.sources.text);
    interp.jit = interp.jit && jit;
    interp.captured = Some(String::new());
    for item in &loaded.program.items {
        if let Item::Test(t) = item {
            if t.root {
                if let Err(d) = interp.run_test(t) {
                    return Some(d.code);
                }
            }
        }
    }
    if yuilang::check_main(&loaded.program).is_ok() {
        if let Err(d) = interp.run_main() {
            return Some(d.code);
        }
    }
    None
}

fn example_files() -> Vec<PathBuf> {
    let mut files = yui_files("examples");
    files.push(root().join("examples/modules/main.yui"));
    files
}

#[test]
fn examples_pass() {
    with_big_stack(|| {
        for path in example_files() {
            for jit in [true, false] {
                assert_eq!(first_error(&path, jit), None, "{} (jit: {jit})", path.display());
            }
        }
    });
}

#[test]
fn error_cases_report_expected_code() {
    with_big_stack(|| {
        for path in yui_files("tests/errors") {
            let src = read(&path);
            let expected = src
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("// expect: "))
                .unwrap_or_else(|| panic!("{} must start with `// expect: E0xxx`", path.display()))
                .trim()
                .to_string();
            for jit in [true, false] {
                assert_eq!(first_error(&path, jit), Some(expected.as_str()), "{} (jit: {jit})", path.display());
            }
        }
    });
}

#[test]
fn print_output_is_captured() {
    let src = "fn main() uses io {\n  print(\"a{1 + 1}\")\n  print(\"{[1.0, 2.5]}\")\n}\n";
    let loaded = yuilang::load_str(src).map_err(|(d, _)| d).expect("program loads");
    let mut interp = Interp::new(&loaded.program, &loaded.sources.text);
    interp.captured = Some(String::new());
    interp.run_main().expect("main runs");
    assert_eq!(interp.captured.as_deref(), Some("a2\n[1.0, 2.5]\n"));
}

#[test]
fn module_output() {
    let loaded = yuilang::load_path(&root().join("examples/modules/main.yui"), None)
        .map_err(|(d, _)| d)
        .expect("program loads");
    let mut interp = Interp::new(&loaded.program, &loaded.sources.text);
    interp.captured = Some(String::new());
    interp.run_main().expect("main runs");
    let out = interp.captured.unwrap();
    assert!(out.contains("Shape.Rect(2.0, 3.0)"), "{out}");
}

#[test]
fn examples_are_formatted() {
    let mut files = yui_files("examples");
    files.extend(yui_files("examples/modules"));
    files.extend(yui_files("examples/modules/geometry"));
    for path in files {
        let src = read(&path);
        let formatted = yuilang::fmt::format_source(&src).unwrap_or_else(|d| panic!("{}: {}", path.display(), d.message));
        assert_eq!(formatted, src, "{} is not in canonical format; run `yui fmt`", path.display());
    }
}

#[test]
fn formatter_fixes_layout() {
    let src = read(&root().join("tests/fmt/messy.yui"));
    let expected = read(&root().join("tests/fmt/messy.expected.yui"));
    let formatted = yuilang::fmt::format_source(&src).map_err(|d| d.message).expect("formats");
    assert_eq!(formatted, expected);
}

#[test]
fn formatting_is_idempotent() {
    let mut files = yui_files("tests/errors");
    files.extend(yui_files("tests/fmt"));
    for path in files {
        let src = read(&path);
        // Files that do not parse cannot be formatted.
        let Ok(once) = yuilang::fmt::format_source(&src) else { continue };
        let twice = yuilang::fmt::format_source(&once).map_err(|d| d.message).expect("formats again");
        assert_eq!(twice, once, "{}", path.display());
    }
}

#[test]
fn file_io_uses_fs_effect() {
    let path = std::env::temp_dir().join(format!("yuilang_test_{}.txt", std::process::id()));
    let p = path.to_string_lossy().replace('\\', "/");
    let src = format!(
        "fn main() uses io, fs {{\n  match write_file(\"{p}\", \"hello\") {{\n    Ok(_) => print(\"written\")\n    Err(e) => print(e)\n  }}\n  print(read_file(\"{p}\").unwrap_or(\"missing\"))\n  print(read_file(\"{p}.nope\").is_err().to_string())\n}}\n"
    );
    let loaded = yuilang::load_str(&src).map_err(|(d, _)| d).expect("program loads");
    let mut interp = Interp::new(&loaded.program, &loaded.sources.text);
    interp.captured = Some(String::new());
    interp.run_main().expect("main runs");
    let _ = std::fs::remove_file(&path);
    assert_eq!(interp.captured.as_deref(), Some("written\nhello\ntrue\n"));

    // The same program without `fs` in its signature must be rejected.
    let pure = src.replace("uses io, fs", "uses io");
    let codes: Vec<&str> = match yuilang::load_str(&pure) {
        Ok(_) => Vec::new(),
        Err((diags, _)) => diags.iter().map(|d| d.code).collect(),
    };
    assert!(codes.contains(&"E0401"), "{codes:?}");
}