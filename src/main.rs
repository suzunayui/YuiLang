//! `yui` command line tool.

use yuilang::ast::Item;
use yuilang::diag::{json_str, render_human, render_json, Diagnostic};
use yuilang::interp::Interp;

const USAGE: &str = "\
YuiLang 0.1.0

Usage:
  yui check   <file> [--json]   type-check a file
  yui run     <file> [--json]   run `main`
  yui test    <file> [--json]   run `test` blocks
  yui outline <file>            print declarations without bodies
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

fn diagnostics_json(diags: &[Diagnostic], file: &str, src: &str) -> String {
    let items: Vec<String> = diags.iter().map(|d| render_json(d, file, src)).collect();
    format!("{{\"ok\":{},\"diagnostics\":[{}]}}", diags.is_empty(), items.join(","))
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (cmd, file) = match positional.as_slice() {
        [cmd, file] => (cmd.as_str(), file.as_str()),
        _ => {
            eprint!("{USAGE}");
            return 2;
        }
    };
    if !matches!(cmd, "check" | "run" | "test" | "outline") {
        eprintln!("unknown command `{cmd}`\n");
        eprint!("{USAGE}");
        return 2;
    }

    let src = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read `{file}`: {e}");
            return 2;
        }
    };
    // Normalize the path so diagnostics look the same on every platform.
    let file = file.replace('\\', "/");
    let file = file.as_str();

    // Reports diagnostics found before the program runs.
    let report = |diags: &[Diagnostic], json_to_stdout: bool| {
        if json {
            let text = diagnostics_json(diags, file, &src);
            if json_to_stdout {
                println!("{text}");
            } else {
                eprintln!("{text}");
            }
        } else {
            for d in diags {
                eprintln!("{}", render_human(d, file, &src));
            }
            if !diags.is_empty() {
                eprintln!("{} error(s)", diags.len());
            }
        }
    };

    // `run` keeps stdout for the program's own output.
    let json_to_stdout = cmd != "run";
    let program = match yuilang::load(&src) {
        Ok(p) => p,
        Err(diags) => {
            report(&diags, json_to_stdout);
            return 1;
        }
    };

    match cmd {
        "check" => {
            if json {
                println!("{}", diagnostics_json(&[], file, &src));
            } else {
                println!("ok: {file}");
            }
            0
        }
        "outline" => {
            print!("{}", yuilang::outline(&program, &src));
            0
        }
        "run" => {
            if let Err(d) = yuilang::check_main(&program) {
                report(&[d], false);
                return 1;
            }
            let mut interp = Interp::new(&program, &src);
            match interp.run_main() {
                Ok(()) => 0,
                Err(d) => {
                    report(&[d], false);
                    1
                }
            }
        }
        _ => {
            let mut interp = Interp::new(&program, &src);
            let mut passed = 0;
            let mut failed = 0;
            let mut results = Vec::new();
            for item in &program.items {
                let Item::Test(t) = item else { continue };
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
                                render_json(&d, file, &src)
                            ));
                        } else {
                            println!("FAIL  {}", t.name);
                            println!("{}", render_human(&d, file, &src));
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
