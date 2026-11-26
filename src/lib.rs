//! YuiLang: a programming language designed for AI-driven development.

pub mod ast;
pub mod bytecode;
pub mod checker;
pub mod diag;
pub mod fmt;
pub mod interp;
#[cfg(target_arch = "x86_64")]
pub mod jit;
pub mod lexer;
pub mod parser;
pub mod window;

use diag::{Diagnostic, SourceMap, Span};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A type-checked program together with the sources it was loaded from.
pub struct Loaded {
    pub program: ast::Program,
    pub sources: SourceMap,
    /// Every source file of the program, the entry file first.
    pub units: Vec<SourceUnit>,
}

/// One source file of a program.
#[derive(Clone)]
pub struct SourceUnit {
    /// Dotted module path; empty for the entry file.
    pub module: String,
    /// File name shown in diagnostics.
    pub name: String,
    pub text: String,
}

pub type LoadError = (Vec<Diagnostic>, SourceMap);

/// Loads the entry file and every module it imports, then type-checks them.
/// Imports are resolved relative to `root` (default: the entry file's directory).
pub fn load_path(path: &Path, root: Option<&Path>) -> Result<Loaded, LoadError> {
    let name = path.to_string_lossy().to_string();
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            let d = Diagnostic::new("E0208", format!("cannot read `{name}`: {e}"), Span::new(0, 0));
            return Err((vec![d], SourceMap::single(&name, "")));
        }
    };
    let root: PathBuf = match root {
        Some(r) => r.to_path_buf(),
        None => path.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    let entry = path.canonicalize().ok();
    load(&name, &src, &|module, span| read_module(&root, entry.as_deref(), module, span))
}

/// Loads a program from a string. Imports are resolved relative to the current directory.
pub fn load_str(src: &str) -> Result<Loaded, LoadError> {
    load("main.yui", src, &|module, span| read_module(Path::new("."), None, module, span))
}

/// Loads a program from source files held in memory, as stored in an
/// executable made by `yui build`. The first unit is the entry file.
pub fn load_units(units: &[SourceUnit]) -> Result<Loaded, LoadError> {
    let Some(entry) = units.first() else {
        let d = Diagnostic::new("E0208", "the program contains no source files", Span::new(0, 0));
        return Err((vec![d], SourceMap::default()));
    };
    load(&entry.name, &entry.text, &|module, span| {
        units
            .iter()
            .find(|u| u.module == module)
            .map(|u| (u.name.clone(), u.text.clone()))
            .ok_or_else(|| Diagnostic::new("E0208", format!("cannot find module `{module}`"), span))
    })
}

/// Reads the file of a module below `root`. Returns its display name and text.
fn read_module(root: &Path, entry: Option<&Path>, module: &str, span: Span) -> Result<(String, String), Diagnostic> {
    let relative: PathBuf = module.split('.').collect::<PathBuf>().with_extension("yui");
    let file = root.join(&relative);
    let display = file.to_string_lossy().to_string();
    if entry.is_some() && file.canonicalize().ok().as_deref() == entry {
        return Err(Diagnostic::new(
            "E0208",
            format!("module `{module}` is the entry file and cannot be imported"),
            span,
        ));
    }
    match std::fs::read_to_string(&file) {
        Ok(text) => Ok((display, text)),
        Err(_) => Err(Diagnostic::new("E0208", format!("cannot find module `{module}`"), span)
            .hint(format!("expected the file `{}`", display.replace('\\', "/")))),
    }
}

/// Parses the entry file and, through `read`, every module it imports
/// (directly or not), then type-checks the whole program.
fn load(
    name: &str,
    src: &str,
    read: &dyn Fn(&str, Span) -> Result<(String, String), Diagnostic>,
) -> Result<Loaded, LoadError> {
    let mut sources = SourceMap::default();
    let base = sources.add(name, src);
    let mut units = vec![SourceUnit { module: String::new(), name: name.to_string(), text: src.to_string() }];
    let mut program = match parser::parse_file(src, base, "", true) {
        Ok(p) => p,
        Err(d) => return Err((vec![d], sources)),
    };

    let mut loaded: HashSet<String> = HashSet::new();
    let mut queue: Vec<(String, Span)> = program.imports.iter().map(|i| (i.path.clone(), i.span)).collect();
    while let Some((module, span)) = queue.pop() {
        if !loaded.insert(module.clone()) {
            continue;
        }
        let (display, text) = match read(&module, span) {
            Ok(found) => found,
            Err(d) => return Err((vec![d], sources)),
        };
        let base = sources.add(&display, &text);
        match parser::parse_file(&text, base, &module, true) {
            Ok(parsed) => {
                queue.extend(parsed.imports.iter().map(|i| (i.path.clone(), i.span)));
                program.items.extend(parsed.items);
            }
            Err(d) => return Err((vec![d], sources)),
        }
        units.push(SourceUnit { module, name: display, text });
    }

    let (diags, types) = checker::check(&program);
    program.types = types;
    if diags.is_empty() {
        Ok(Loaded { program, sources, units })
    } else {
        Err((diags, sources))
    }
}
/// Checks that the program has a `main` function that `yui run` can call.
pub fn check_main(program: &ast::Program) -> Result<(), Diagnostic> {
    for item in &program.items {
        if let ast::Item::Fn(f) = item {
            if f.name == "main" {
                if !f.params.is_empty() || f.ret.is_some() || !f.type_params.is_empty() {
                    return Err(Diagnostic::new(
                        "E0207",
                        "`main` must take no parameters and return nothing",
                        f.name_span,
                    )
                    .hint("write `fn main() uses io { ... }`"));
                }
                return Ok(());
            }
        }
    }
    Err(Diagnostic::new("E0207", "no `main` function found", Span::new(0, 0))
        .hint("add `fn main() uses io { ... }`"))
}

/// One-line-per-declaration summary of the entry file: signatures without bodies.
pub fn outline(loaded: &Loaded) -> String {
    use ast::Item;
    let sources = &loaded.sources;
    let src = &sources.text;
    let entry_end = sources.files.first().map_or(0, |f| f.end);
    let generics = |params: &[String]| {
        if params.is_empty() {
            String::new()
        } else {
            format!("<{}>", params.join(", "))
        }
    };
    let effects = |effects: &[(String, Span)]| {
        if effects.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = effects.iter().map(|(e, _)| e.as_str()).collect();
            format!(" uses {}", names.join(", "))
        }
    };
    let mut out = String::new();
    for import in &loaded.program.imports {
        let (_, line, _) = sources.locate(import.span.start);
        out.push_str(&format!("{line}: import {}\n", import.path));
    }
    for item in &loaded.program.items {
        if item.span().start > entry_end {
            continue;
        }
        let (_, line, _) = sources.locate(item.span().start);
        match item {
            Item::Struct(s) => {
                let fields: Vec<String> = s.fields.iter().map(|f| format!("{}: {}", f.name, f.ty)).collect();
                out.push_str(&format!(
                    "{line}: struct {}{} {{ {} }}\n",
                    s.name,
                    generics(&s.type_params),
                    fields.join(", ")
                ));
            }
            Item::Enum(e) => {
                let variants: Vec<String> = e
                    .variants
                    .iter()
                    .map(|v| {
                        if v.fields.is_empty() {
                            v.name.clone()
                        } else {
                            let tys: Vec<String> = v.fields.iter().map(|t| t.to_string()).collect();
                            format!("{}({})", v.name, tys.join(", "))
                        }
                    })
                    .collect();
                out.push_str(&format!(
                    "{line}: enum {}{} {{ {} }}\n",
                    e.name,
                    generics(&e.type_params),
                    variants.join(", ")
                ));
            }
            Item::Fn(f) => {
                let params: Vec<String> = f.params.iter().map(|p| format!("{}: {}", p.name, p.ty)).collect();
                let ret = match &f.ret {
                    Some(t) if matches!(t.kind, ast::TypeKind::Fn(..)) => format!(" -> ({t})"),
                    Some(t) => format!(" -> {t}"),
                    None => String::new(),
                };
                out.push_str(&format!(
                    "{line}: fn {}{}({}){ret}{}\n",
                    f.name,
                    generics(&f.type_params),
                    params.join(", "),
                    effects(&f.effects)
                ));
                for r in &f.requires {
                    out.push_str(&format!("    requires {}\n", &src[r.span.start..r.span.end]));
                }
                for e in &f.ensures {
                    out.push_str(&format!("    ensures {}\n", &src[e.span.start..e.span.end]));
                }
            }
            Item::Test(t) => {
                out.push_str(&format!("{line}: test {:?}{}\n", t.name, effects(&t.effects)));
            }
        }
    }
    out
}
