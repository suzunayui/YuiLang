//! `yui` command line tool.

use std::path::Path;
use yuilang::ast::Item;
use yuilang::diag::{json_str, render_human, render_json, Diagnostic, SourceMap};
use yuilang::interp::Interp;

/// Version as `YYYY.MMDD.N`: release year, month and day, and a serial number
/// for that day. Cargo does not allow leading zeros, so the month-day part is
/// stored without them (`105` for January 5th) and padded here.
fn version() -> String {
    let month_day: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0);
    format!("{}.{:04}.{}", env!("CARGO_PKG_VERSION_MAJOR"), month_day, env!("CARGO_PKG_VERSION_PATCH"))
}

fn print_usage() {
    eprint!("YuiLang {}\n\n{USAGE}", version());
}

const USAGE: &str = "\
Usage:
  yui check   <file> [--json]         type-check a file and the modules it imports
  yui run     <file> [--json] [-- args...]
                                      run `main`; arguments after `--` are returned by args()
  yui test    <file> [--json]         run the `test` blocks of a file
  yui fmt     <file> [--check]        format a file in place (--check: only report)
  yui outline <file>                  print declarations without bodies
  yui --version                       print the version

Options:
  --root <dir>   directory that `import` paths are relative to
                 (default: the directory of <file>)
";

fn main() {
    // The interpreter recurses on the native stack, so give it plenty of room.
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(real_main)
        .expect("failed to start interpreter thread");
    let code = handle.join().unwrap_or(2);
    std::process::exit(code);
}

fn diagnostics_json(diags: &[Diagnostic], sources: &SourceMap) -> String {
    let items: Vec<String> = diags.iter().map(|d| render_json(d, sources)).collect();
    format!("{{\"ok\":{},\"diagnostics\":[{}]}}", diags.is_empty(), items.join(","))
}

struct Options {
    cmd: String,
    file: String,
    json: bool,
    check: bool,
    root: Option<String>,
    program_args: Vec<String>,
}

fn parse_args() -> Option<Options> {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut opts = Options {
        cmd: String::new(),
        file: String::new(),
        json: false,
        check: false,
        root: None,
        program_args: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--check" => opts.check = true,
            "--root" => opts.root = Some(args.next()?),
            "--" => {
                opts.program_args = args.collect();
                break;
            }
            a if a.starts_with("--") => {
                eprintln!("unknown option `{a}`\n");
                return None;
            }
            _ => positional.push(arg),
        }
    }
    if positional.len() != 2 {
        return None;
    }
    opts.file = positional.pop()?;
    opts.cmd = positional.pop()?;
    Some(opts)
}

fn real_main() -> i32 {
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("yui {}", version());
        return 0;
    }
    let Some(opts) = parse_args() else {
        print_usage();
        return 2;
    };
    let json = opts.json;
    match opts.cmd.as_str() {
        "check" | "run" | "test" | "outline" => {}
        "fmt" => return format_file(&opts),
        other => {
            eprintln!("unknown command `{other}`\n");
            print_usage();
            return 2;
        }
    }

    let report = |diags: &[Diagnostic], sources: &SourceMap, json_to_stdout: bool| {
        if json {
            let text = diagnostics_json(diags, sources);
            if json_to_stdout {
                println!("{text}");
            } else {
                eprintln!("{text}");
            }
        } else {
            for d in diags {
                eprintln!("{}", render_human(d, sources));
            }
            if !diags.is_empty() {
                eprintln!("{} error(s)", diags.len());
            }
        }
    };

    // `run` keeps stdout for the program's own output.
    let json_to_stdout = opts.cmd != "run";
    let root = opts.root.as_deref().map(Path::new);
    let loaded = match yuilang::load_path(Path::new(&opts.file), root) {
        Ok(l) => l,
        Err((diags, sources)) => {
            report(&diags, &sources, json_to_stdout);
            return 1;
        }
    };
    let sources = &loaded.sources;

    match opts.cmd.as_str() {
        "check" => {
            if json {
                println!("{}", diagnostics_json(&[], sources));
            } else {
                println!("ok: {}", opts.file.replace('\\', "/"));
            }
            0
        }
        "outline" => {
            print!("{}", yuilang::outline(&loaded));
            0
        }
        "run" => {
            if let Err(d) = yuilang::check_main(&loaded.program) {
                report(&[d], sources, false);
                return 1;
            }
            let mut interp = Interp::new(&loaded.program, &sources.text);
            interp.args = opts.program_args.clone();
            match interp.run_main() {
                Ok(()) => 0,
                Err(d) => {
                    report(&[d], sources, false);
                    1
                }
            }
        }
        _ => {
            let mut interp = Interp::new(&loaded.program, &sources.text);
            let mut passed = 0;
            let mut failed = 0;
            let mut results = Vec::new();
            for item in &loaded.program.items {
                let Item::Test(t) = item else { continue };
                if !t.root {
                    continue;
                }
                match interp.run_test(t) {
                    Ok(()) => {
                        passed += 1;
                        if json {
                            results.push(format!("{{\"name\":{},\"ok\":true,\"error\":null}}", json_str(&t.name)));
                        } else {
                            println!("ok    {}", t.name);
                        }
                    }
                    Err(d) => {
                        failed += 1;
                        if json {
                            results.push(format!(
                                "{{\"name\":{},\"ok\":false,\"error\":{}}}",
                                json_str(&t.name),
                                render_json(&d, sources)
                            ));
                        } else {
                            println!("FAIL  {}", t.name);
                            println!("{}", render_human(&d, sources));
                        }
                    }
                }
            }
            if json {
                println!(
                    "{{\"ok\":{},\"passed\":{passed},\"failed\":{failed},\"tests\":[{}]}}",
                    failed == 0,
                    results.join(",")
                );
            } else {
                println!("{passed} passed, {failed} failed");
            }
            if failed == 0 {
                0
            } else {
                1
            }
        }
    }
}

fn format_file(opts: &Options) -> i32 {
    let file = opts.file.replace('\\', "/");
    let src = match std::fs::read_to_string(&opts.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read `{file}`: {e}");
            return 2;
        }
    };
    let formatted = match yuilang::fmt::format_source(&src) {
        Ok(f) => f,
        Err(d) => {
            let sources = SourceMap::single(&file, &src);
            if opts.json {
                println!("{}", diagnostics_json(&[d], &sources));
            } else {
                eprintln!("{}", render_human(&d, &sources));
            }
            return 1;
        }
    };
    let normalized = src.replace("\r\n", "\n");
    if formatted == normalized {
        println!("unchanged: {file}");
        return 0;
    }
    if opts.check {
        println!("would reformat: {file}");
        return 1;
    }
    match std::fs::write(&opts.file, &formatted) {
        Ok(()) => {
            println!("formatted: {file}");
            0
        }
        Err(e) => {
            eprintln!("cannot write `{file}`: {e}");
            2
        }
    }
}
