//! YuiLang: a programming language designed for AI-driven development.

pub mod ast;
pub mod checker;
pub mod diag;
pub mod interp;
pub mod lexer;
pub mod parser;

use diag::Diagnostic;

/// Parses and type-checks a source file.
pub fn load(src: &str) -> Result<ast::Program, Vec<Diagnostic>> {
    let program = parser::parse_program(src).map_err(|d| vec![d])?;
    let diags = checker::check(&program);
    if diags.is_empty() {
        Ok(program)
    } else {
        Err(diags)
    }
}

/// Checks that the program has a `main` function that `yui run` can call.
pub fn check_main(program: &ast::Program) -> Result<(), Diagnostic> {
    for item in &program.items {
        if let ast::Item::Fn(f) = item {
            if f.name == "main" {
                if !f.params.is_empty() || f.ret.is_some() {
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
    Err(Diagnostic::new("E0207", "no `main` function found", diag::Span::new(0, 0))
        .hint("add `fn main() uses io { ... }`"))
}

/// One-line-per-declaration summary of a program: signatures without bodies.
pub fn outline(program: &ast::Program, src: &str) -> String {
    use ast::Item;
    let effects = |effects: &[(String, diag::Span)]| {
        if effects.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = effects.iter().map(|(e, _)| e.as_str()).collect();
            format!(" uses {}", names.join(", "))
        }
    };
    let mut out = String::new();
    for item in &program.items {
        match item {
            Item::Struct(s) => {
                let (line, _) = diag::line_col(src, s.name_span.start);
                let fields: Vec<String> = s.fields.iter().map(|f| format!("{}: {}", f.name, f.ty)).collect();
                out.push_str(&format!("{line}: struct {} {{ {} }}\n", s.name, fields.join(", ")));
            }
            Item::Enum(e) => {
                let (line, _) = diag::line_col(src, e.name_span.start);
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
                out.push_str(&format!("{line}: enum {} {{ {} }}\n", e.name, variants.join(", ")));
            }
            Item::Fn(f) => {
                let (line, _) = diag::line_col(src, f.name_span.start);
                let params: Vec<String> = f.params.iter().map(|p| format!("{}: {}", p.name, p.ty)).collect();
                let ret = match &f.ret {
                    Some(t) => format!(" -> {t}"),
                    None => String::new(),
                };
                out.push_str(&format!(
                    "{line}: fn {}({}){ret}{}\n",
                    f.name,
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
                let (line, _) = diag::line_col(src, t.name_span.start);
                out.push_str(&format!("{line}: test {:?}{}\n", t.name, effects(&t.effects)));
            }
        }
    }
    out
}
