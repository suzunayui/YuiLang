//! `yui` command line tool.

use std::path::Path;
use yuilang::ast::Item;
use yuilang::diag::{json_str, render_human, render_json, Diagnostic, SourceMap};
use yuilang::interp::Interp;
use yuilang::SourceUnit;

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
  yui build   <file> [-o <output>]    make a stand-alone executable of the program:
                                      machine code on Windows x64 (a few KB), otherwise
                                      this interpreter with the program inside
              [--native]              fail instead of bundling the interpreter when
                                      the program cannot be compiled to machine code
              [--bundle]              always bundle the interpreter
  yui outline <file>                  print declarations without bodies
  yui --version                       print the version

Options:
  --root <dir>   directory that `import` paths are relative to
                 (default: the directory of <file>)
";

fn main() {
    // Closures called from built-in methods (`map`, `fold`, ...) recurse on the
    // native stack, so give it plenty of room.
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
    out: Option<String>,
    native: bool,
    bundle: bool,
    program_args: Vec<String>,
}

/// Marks the end of an executable made by `yui build`. Such a file is a copy
/// of `yui` followed by the program's sources, their length and this marker.
const BUNDLE_MAGIC: &[u8; 8] = b"YUIBNDL1";

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend((s.len() as u32).to_le_bytes());
    out.extend(s.as_bytes());
}

fn encode_units(units: &[SourceUnit]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((units.len() as u32).to_le_bytes());
    for unit in units {
        put_str(&mut out, &unit.module);
        put_str(&mut out, &unit.name);
        put_str(&mut out, &unit.text);
    }
    let len = out.len() as u64;
    out.extend(len.to_le_bytes());
    out.extend(BUNDLE_MAGIC);
    out
}

fn decode_units(mut bytes: &[u8]) -> Option<Vec<SourceUnit>> {
    fn take<'b>(bytes: &mut &'b [u8], n: usize) -> Option<&'b [u8]> {
        if bytes.len() < n {
            return None;
        }
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        Some(head)
    }
    fn number(bytes: &mut &[u8]) -> Option<usize> {
        Some(u32::from_le_bytes(take(bytes, 4)?.try_into().ok()?) as usize)
    }
    fn text(bytes: &mut &[u8]) -> Option<String> {
        let len = number(bytes)?;
        String::from_utf8(take(bytes, len)?.to_vec()).ok()
    }
    let count = number(&mut bytes)?;
    let mut units = Vec::new();
    for _ in 0..count {
        units.push(SourceUnit { module: text(&mut bytes)?, name: text(&mut bytes)?, text: text(&mut bytes)? });
    }
    Some(units)
}

/// The program stored at the end of this executable, if it was made by `yui build`.
fn embedded_units() -> Option<Vec<SourceUnit>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
    let size = file.metadata().ok()?.len();
    if size < 16 {
        return None;
    }
    let mut trailer = [0u8; 16];
    file.seek(SeekFrom::Start(size - 16)).ok()?;
    file.read_exact(&mut trailer).ok()?;
    if &trailer[8..] != BUNDLE_MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(trailer[..8].try_into().ok()?);
    if len > size - 16 {
        return None;
    }
    let mut payload = vec![0u8; len as usize];
    file.seek(SeekFrom::Start(size - 16 - len)).ok()?;
    file.read_exact(&mut payload).ok()?;
    decode_units(&payload)
}

/// Runs the program stored in this executable. Every command line argument
/// belongs to the program.
fn run_embedded(units: Vec<SourceUnit>) -> i32 {
    let loaded = match yuilang::load_units(&units) {
        Ok(l) => l,
        Err((diags, sources)) => {
            for d in &diags {
                eprintln!("{}", render_human(d, &sources));
            }
            return 1;
        }
    };
    let mut interp = Interp::new(&loaded.program, &loaded.sources.text);
    interp.args = std::env::args().skip(1).collect();
    match interp.run_main() {
        Ok(()) => 0,
        Err(d) => {
            eprintln!("{}", render_human(&d, &loaded.sources));
            1
        }
    }
}

/// Compiles the program to machine code and writes an executable that
/// contains no interpreter. `Err` says what in the program the native
/// compiler cannot handle yet, and where; nothing is written then.
#[cfg(all(windows, target_arch = "x86_64"))]
fn build_native(loaded: &yuilang::Loaded, opts: &Options) -> Result<i32, Option<(String, yuilang::diag::Span)>> {
    let source = Path::new(&opts.file);
    let name = source.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "main.yui".to_string());
    // Runtime errors name the files relative to the program, as in a bundled
    // build. The files are laid out as they were loaded, so that positions agree.
    let mut sources = SourceMap::default();
    for unit in &loaded.units {
        let file = if unit.module.is_empty() { name.clone() } else { format!("{}.yui", unit.module.replace('.', "/")) };
        sources.add(&file, &unit.text);
    }
    let bytes = match yuilang::jit::native::build(&loaded.program, &sources) {
        Ok(bytes) => bytes,
        Err(reason) => return Err(Some((reason.what, reason.span))),
    };
    let out = match &opts.out {
        Some(out) => out.clone(),
        None => source.with_extension(std::env::consts::EXE_EXTENSION).to_string_lossy().to_string(),
    };
    if let Err(e) = std::fs::write(&out, &bytes) {
        eprintln!("cannot write `{}`: {e}", out.replace('\\', "/"));
        return Ok(2);
    }
    println!("built: {} ({} bytes, native)", out.replace('\\', "/"), bytes.len());
    Ok(0)
}

/// No native compiler for this platform yet.
#[cfg(not(all(windows, target_arch = "x86_64")))]
fn build_native(_loaded: &yuilang::Loaded, _opts: &Options) -> Result<i32, Option<(String, yuilang::diag::Span)>> {
    Err(None)
}

/// `yui build`: makes a stand-alone executable. Machine code where that is
/// possible; otherwise (another platform, or a program that uses something
/// the native compiler does not handle yet) a copy of this executable with
/// the program appended, which behaves the same and is larger.
fn build(loaded: &yuilang::Loaded, opts: &Options) -> i32 {
    use std::io::Write;
    if !opts.bundle {
        match build_native(loaded, opts) {
            Ok(code) => return code,
            Err(Some((what, span))) if opts.native => {
                let d = Diagnostic::new("E0902", format!("the native compiler does not support {what} yet"), span)
                    .hint("build without `--native` to bundle the interpreter instead");
                eprintln!("{}", render_human(&d, &loaded.sources));
                return 1;
            }
            Err(None) if opts.native => {
                eprintln!("`--native` is only available on Windows x64 so far");
                return 1;
            }
            Err(Some((what, _))) => {
                eprintln!("note: bundling the interpreter, because the native compiler does not support {what} yet");
            }
            Err(None) => {}
        }
    }
    let source = Path::new(&opts.file);
    // Store file names relative to the program, not as paths on this machine.
    let units: Vec<SourceUnit> = loaded
        .units
        .iter()
        .map(|unit| SourceUnit {
            module: unit.module.clone(),
            name: if unit.module.is_empty() {
                source.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "main.yui".to_string())
            } else {
                format!("{}.yui", unit.module.replace('.', "/"))
            },
            text: unit.text.clone(),
        })
        .collect();
    let out = match &opts.out {
        Some(out) => out.clone(),
        None => source.with_extension(std::env::consts::EXE_EXTENSION).to_string_lossy().to_string(),
    };
    let written = std::env::current_exe()
        .and_then(|exe| std::fs::copy(exe, &out))
        .and_then(|_| std::fs::OpenOptions::new().append(true).open(&out))
        .and_then(|mut file| file.write_all(&encode_units(&units)));
    if let Err(e) = written {
        eprintln!("cannot write `{}`: {e}", out.replace('\\', "/"));
        return 2;
    }
    // Apple Silicon only runs signed code, and appending data invalidates the
    // signature the linker made. Re-sign ad hoc; ignore a missing `codesign`.
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("codesign").args(["--force", "--sign", "-", &out]).output();
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!("built: {} ({} KB)", out.replace('\\', "/"), size / 1024);
    0
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
        out: None,
        native: false,
        bundle: false,
        program_args: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--check" => opts.check = true,
            "--root" => opts.root = Some(args.next()?),
            "-o" | "--output" => opts.out = Some(args.next()?),
            "--native" => opts.native = true,
            "--bundle" => opts.bundle = true,
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
    if let Some(units) = embedded_units() {
        return run_embedded(units);
    }
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
        "check" | "run" | "test" | "outline" | "build" => {}
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
    // The native compiler works from the types of all expressions.
    let native = opts.cmd == "build" && !opts.bundle && cfg!(all(windows, target_arch = "x86_64"));
    yuilang::checker::RECORD_EXPR_TYPES.store(native, std::sync::atomic::Ordering::Relaxed);
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
        "build" => {
            if let Err(d) = yuilang::check_main(&loaded.program) {
                report(&[d], sources, true);
                return 1;
            }
            build(&loaded, &opts)
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
