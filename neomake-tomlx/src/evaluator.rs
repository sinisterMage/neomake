//! Document-level TOMLX evaluator.
//!
//! # Pipeline
//!
//! The evaluator owns the document-level scanner. Walking the source
//! line-by-line it:
//!
//! 1. Extracts top-level `$name = <EXPR>` variable declarations,
//!    evaluates each RHS immediately (earlier variables can be
//!    referenced by later ones), and removes the line from the output.
//! 2. For every `key = <value>` line whose RHS contains a TOMLX
//!    sentinel, re-parses the RHS with [`crate::lexer`] + [`crate::parser`],
//!    evaluates it against the current scope, and re-renders the result
//!    as a TOML literal.
//! 3. Leaves every other line of the source untouched.
//!
//! The resulting buffer is a pure-TOML document that we hand off to
//! `toml::from_str` to produce the final [`toml::Value`].
//!
//! # Graceful degradation
//!
//! If the source contains *no* TOMLX sentinels (`$IDENT =`, `${`, `if `
//! in value position, or a top-level `env|glob|exec(`), [`load_str`]
//! short-circuits and parses the input directly as TOML. This
//! guarantees that any valid `.toml` file is also a valid `.tomlx`
//! file with identical semantics.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use globset::{GlobBuilder, GlobSetBuilder};
use walkdir::WalkDir;

use crate::error::TomlxError;
use crate::lexer::{tokenize, Span};
use crate::parser::{parse_expr, BinOp, Expr, StringPart, UnaryOp};
use crate::shell;

/// Evaluate a TOMLX source string into a fully-materialized [`toml::Value`].
///
/// `path` is used purely for diagnostics — pass the real path when
/// loading from disk; any synthetic name works for in-memory tests.
pub fn load_str(source: &str, path: &Path) -> Result<toml::Value, TomlxError> {
    if !has_tomlx_markers(source) {
        return parse_plain_toml(source, path);
    }
    let rendered = preprocess(source, path)?;
    parse_plain_toml(&rendered, path)
}

/// Read a file from disk and evaluate it.
pub fn load(path: &Path) -> Result<toml::Value, TomlxError> {
    let source = std::fs::read_to_string(path).map_err(|e| TomlxError {
        file: path.to_path_buf(),
        line: 0,
        col: 0,
        message: format!("cannot read `{}`: {e}", path.display()),
    })?;
    load_str(&source, path)
}

fn parse_plain_toml(source: &str, path: &Path) -> Result<toml::Value, TomlxError> {
    toml::from_str(source).map_err(|e| {
        let line = e
            .span()
            .and_then(|span| line_from_offset(source, span.start))
            .unwrap_or(0);
        TomlxError {
            file: path.to_path_buf(),
            line,
            col: 0,
            message: format!("toml parse error: {}", e.message()),
        }
    })
}

fn line_from_offset(source: &str, offset: usize) -> Option<usize> {
    if offset > source.len() {
        return None;
    }
    Some(source[..offset].bytes().filter(|&b| b == b'\n').count() + 1)
}

/// Cheap check: does the source contain anything that TOMLX cares about
/// at the line level? If not, we can skip the whole pipeline.
fn has_tomlx_markers(source: &str) -> bool {
    // Any top-level `$ident =` line is a variable declaration.
    for line in source.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('$') {
            if rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                return true;
            }
        }
    }
    // ${ appears only in TOMLX interpolation
    if source.contains("${") {
        return true;
    }
    // A value-level conditional.
    if source.contains("= if ") || source.contains(" =if ") {
        return true;
    }
    // Bare function calls or evaluator expressions
    if source.contains("git(") || source.contains("fetch(") {
        return true;
    }
    false
}

/// Scope tracking top-level variables evaluated so far.
#[derive(Debug, Default, Clone)]
struct Scope {
    vars: BTreeMap<String, toml::Value>,
    base_dir: PathBuf,
}

fn preprocess(source: &str, path: &Path) -> Result<String, TomlxError> {
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut scope = Scope {
        vars: BTreeMap::new(),
        base_dir,
    };
    let mut out = String::with_capacity(source.len());

    for (idx, line) in source.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let (body, newline) = split_trailing_newline(line);
        let trimmed = body.trim_start();
        let leading = &body[..(body.len() - trimmed.len())];

        // Top-level variable declaration: `$name = EXPR`.
        if let Some(rest) = trimmed.strip_prefix('$') {
            if rest
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_')
                .unwrap_or(false)
            {
                let (name, after_name) = take_ident(rest);
                let after_name = after_name.trim_start();
                let after_eq = after_name.strip_prefix('=').ok_or_else(|| {
                    TomlxError::new(
                        path,
                        line_no,
                        leading.len() + 1,
                        "variable declaration must use `=`",
                    )
                })?;
                let (expr_src, _inline_comment) = strip_inline_comment(after_eq.trim_start());
                let base_span = Span::new(path, line_no, column_of(body, expr_src));
                let expr = parse_source_expr(expr_src, &base_span)?;
                let value = evaluate(&expr, &scope)?;
                scope.vars.insert(name, value);
                // Emit a blank line (preserves line numbering for downstream errors).
                out.push_str(newline);
                continue;
            }
        }

        // `key = value` style line.
        if let Some((key_part, value_part)) = split_key_value(body) {
            if value_contains_tomlx_sentinel(value_part) {
                let (expr_src, _trailing_comment) = strip_inline_comment(value_part);
                let base_span = Span::new(path, line_no, column_of(body, expr_src));
                let expr = parse_source_expr(expr_src.trim(), &base_span)?;
                let value = evaluate(&expr, &scope)?;
                let rendered = render_toml_value(&value);
                out.push_str(leading);
                out.push_str(key_part.trim_end());
                out.push_str(" = ");
                out.push_str(&rendered);
                out.push_str(newline);
                continue;
            }
        }

        if trimmed.starts_with("git(") || trimmed.starts_with("fetch(") {
            let (expr_src, _trailing_comment) = strip_inline_comment(trimmed);
            let base_span = Span::new(path, line_no, column_of(body, expr_src));
            let expr = parse_source_expr(expr_src.trim(), &base_span)?;
            let _ = evaluate(&expr, &scope)?;
            out.push_str(newline);
            continue;
        }

        // Fallthrough: verbatim.
        out.push_str(body);
        out.push_str(newline);
    }

    Ok(out)
}

fn split_trailing_newline(line: &str) -> (&str, &str) {
    if let Some(stripped) = line.strip_suffix("\r\n") {
        (stripped, "\r\n")
    } else if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (line, "")
    }
}

fn take_ident(s: &str) -> (String, &str) {
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if i == 0 {
            if !(c.is_ascii_alphabetic() || c == '_') {
                break;
            }
            end = i + c.len_utf8();
        } else if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    (s[..end].to_string(), &s[end..])
}

/// Split a `key = value` line into `(key_with_leading_ws, value_after_eq)`.
///
/// Aware of string literals — a `=` inside `"..."` or `'...'` does not count.
/// Returns `None` if the line has no such split or if the LHS is a table header.
fn split_key_value(body: &str) -> Option<(&str, &str)> {
    let bytes = body.as_bytes();
    let trimmed = body.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let mut in_double = false;
    let mut in_single = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_single && b == b'"' {
            // naive double-quote toggling; we don't care about escapes
            // beyond not-the-quote for the purposes of locating `=`.
            let prev = if i > 0 { bytes[i - 1] } else { 0 };
            if prev != b'\\' {
                in_double = !in_double;
            }
        } else if !in_double && b == b'\'' {
            in_single = !in_single;
        } else if !in_double && !in_single && b == b'=' {
            return Some((&body[..i], &body[i + 1..]));
        }
        i += 1;
    }
    None
}

fn strip_inline_comment(s: &str) -> (&str, Option<&str>) {
    let bytes = s.as_bytes();
    let mut in_double = false;
    let mut in_single = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_single && b == b'"' {
            let prev = if i > 0 { bytes[i - 1] } else { 0 };
            if prev != b'\\' {
                in_double = !in_double;
            }
        } else if !in_double && b == b'\'' {
            in_single = !in_single;
        } else if !in_double && !in_single && b == b'#' {
            return (&s[..i], Some(&s[i..]));
        }
        i += 1;
    }
    (s, None)
}

fn value_contains_tomlx_sentinel(value_raw: &str) -> bool {
    // Dollar interpolation or variable reference.
    if value_raw.contains("${") {
        return true;
    }
    let trimmed = value_raw.trim_start();
    if trimmed.starts_with('$')
        && trimmed
            .chars()
            .nth(1)
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
    {
        return true;
    }
    // Value-level conditional.
    if trimmed.starts_with("if ") || trimmed.contains(" if ") {
        return true;
    }
    // Built-in function at the top level of the RHS.
    for name in ["env(", "glob(", "exec(", "git(", "fetch("] {
        if trimmed.starts_with(name) || trimmed.contains(&format!(" {name}")) {
            return true;
        }
    }
    false
}

fn parse_source_expr(src: &str, base: &Span) -> Result<Expr, TomlxError> {
    let tokens = tokenize(src, base)?;
    if tokens.is_empty() {
        return Err(TomlxError::new(
            &base.file,
            base.line,
            base.col,
            "expected expression",
        ));
    }
    parse_expr(tokens)
}

fn column_of(body: &str, slice: &str) -> usize {
    let body_ptr = body.as_ptr() as usize;
    let slice_ptr = slice.as_ptr() as usize;
    if slice_ptr >= body_ptr && slice_ptr <= body_ptr + body.len() {
        slice_ptr - body_ptr + 1
    } else {
        1
    }
}

// -----------------------------------------------------------------------
// Expression evaluation.
// -----------------------------------------------------------------------

fn evaluate(expr: &Expr, scope: &Scope) -> Result<toml::Value, TomlxError> {
    match expr {
        Expr::Int(n) => Ok(toml::Value::Integer(*n)),
        Expr::Float(f) => Ok(toml::Value::Float(*f)),
        Expr::Bool(b) => Ok(toml::Value::Boolean(*b)),
        Expr::LitStr(s) => Ok(toml::Value::String(s.clone())),
        Expr::TemplateStr(parts) => {
            let mut buf = String::new();
            for part in parts {
                match part {
                    StringPart::Lit(s) => buf.push_str(s),
                    StringPart::Interp(e) => {
                        let v = evaluate(e, scope)?;
                        buf.push_str(&stringify_value(&v));
                    }
                }
            }
            Ok(toml::Value::String(buf))
        }
        Expr::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(evaluate(item, scope)?);
            }
            Ok(toml::Value::Array(out))
        }
        Expr::Var(name) => scope.vars.get(name).cloned().ok_or_else(|| {
            TomlxError::at_unknown(
                std::path::PathBuf::new(),
                format!("undefined variable `{name}`"),
            )
        }),
        Expr::Call { name, args, span } => call_builtin(name, args, scope, span),
        Expr::Binary { op, lhs, rhs } => {
            let l = evaluate(lhs, scope)?;
            let r = evaluate(rhs, scope)?;
            apply_binop(*op, &l, &r)
        }
        Expr::Unary { op, expr } => {
            let v = evaluate(expr, scope)?;
            apply_unary(*op, &v)
        }
        Expr::KeywordArg { .. } => Err(TomlxError::at_unknown(
            std::path::PathBuf::new(),
            "keyword arguments are only valid inside function calls",
        )),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let c = evaluate(cond, scope)?;
            let b = match c {
                toml::Value::Boolean(b) => b,
                other => {
                    return Err(TomlxError::at_unknown(
                        std::path::PathBuf::new(),
                        format!("if condition must be boolean, got {}", type_of(&other)),
                    ))
                }
            };
            if b {
                evaluate(then_branch, scope)
            } else {
                evaluate(else_branch, scope)
            }
        }
    }
}

fn apply_binop(op: BinOp, l: &toml::Value, r: &toml::Value) -> Result<toml::Value, TomlxError> {
    use toml::Value as V;
    let err = |msg: String| TomlxError::at_unknown(std::path::PathBuf::new(), msg);
    match op {
        BinOp::Eq => Ok(V::Boolean(values_equal(l, r))),
        BinOp::Ne => Ok(V::Boolean(!values_equal(l, r))),
        BinOp::And => match (l, r) {
            (V::Boolean(a), V::Boolean(b)) => Ok(V::Boolean(*a && *b)),
            _ => Err(err(format!(
                "&& requires booleans, got {} and {}",
                type_of(l),
                type_of(r)
            ))),
        },
        BinOp::Or => match (l, r) {
            (V::Boolean(a), V::Boolean(b)) => Ok(V::Boolean(*a || *b)),
            _ => Err(err(format!(
                "|| requires booleans, got {} and {}",
                type_of(l),
                type_of(r)
            ))),
        },
        BinOp::Plus => match (l, r) {
            (V::Integer(a), V::Integer(b)) => Ok(V::Integer(a + b)),
            (V::Float(a), V::Float(b)) => Ok(V::Float(a + b)),
            (V::String(a), V::String(b)) => {
                let mut s = a.clone();
                s.push_str(b);
                Ok(V::String(s))
            }
            (V::Array(a), V::Array(b)) => {
                let mut out = a.clone();
                out.extend(b.iter().cloned());
                Ok(V::Array(out))
            }
            _ => Err(err(format!(
                "cannot apply `+` to {} and {}",
                type_of(l),
                type_of(r)
            ))),
        },
    }
}

fn apply_unary(op: UnaryOp, v: &toml::Value) -> Result<toml::Value, TomlxError> {
    use toml::Value as V;
    let err = |msg: String| TomlxError::at_unknown(std::path::PathBuf::new(), msg);
    match op {
        UnaryOp::Not => match v {
            V::Boolean(b) => Ok(V::Boolean(!*b)),
            _ => Err(err(format!("`!` requires bool, got {}", type_of(v)))),
        },
        UnaryOp::Neg => match v {
            V::Integer(n) => Ok(V::Integer(-*n)),
            V::Float(f) => Ok(V::Float(-*f)),
            _ => Err(err(format!(
                "unary `-` requires number, got {}",
                type_of(v)
            ))),
        },
    }
}

fn values_equal(a: &toml::Value, b: &toml::Value) -> bool {
    use toml::Value as V;
    match (a, b) {
        (V::String(x), V::String(y)) => x == y,
        (V::Integer(x), V::Integer(y)) => x == y,
        (V::Float(x), V::Float(y)) => x == y,
        (V::Boolean(x), V::Boolean(y)) => x == y,
        _ => false,
    }
}

fn stringify_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(dt) => dt.to_string(),
        toml::Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(stringify_value).collect();
            parts.join(",")
        }
        toml::Value::Table(tbl) => render_toml_value(&toml::Value::Table(tbl.clone())),
    }
}

fn type_of(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "bool",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

// -----------------------------------------------------------------------
// Built-in functions.
// -----------------------------------------------------------------------

fn call_builtin(
    name: &str,
    args: &[Expr],
    scope: &Scope,
    span: &Span,
) -> Result<toml::Value, TomlxError> {
    let mk_err = |msg: String| TomlxError::new(&span.file, span.line, span.col, msg);
    
    let mut positional = Vec::new();
    let mut kwargs = BTreeMap::new();
    for arg in args {
        match arg {
            Expr::KeywordArg { name, value } => {
                kwargs.insert(name.as_str(), evaluate(value, scope)?);
            }
            _ => {
                positional.push(evaluate(arg, scope)?);
            }
        }
    }

    match name {
        "env" => {
            if !kwargs.is_empty() {
                return Err(mk_err("env() does not accept keyword arguments".into()));
            }
            if positional.is_empty() || positional.len() > 2 {
                return Err(mk_err(format!(
                    "env() expects 1 or 2 arguments, got {}",
                    positional.len()
                )));
            }
            let key = expect_string(&positional[0], "env() first argument", &mk_err)?;
            match std::env::var(&key) {
                Ok(v) => Ok(toml::Value::String(v)),
                Err(_) => {
                    if positional.len() == 2 {
                        Ok(positional[1].clone())
                    } else {
                        Err(mk_err(format!(
                            "environment variable `{key}` not set (use env(\"{key}\", \"default\") to provide a fallback)"
                        )))
                    }
                }
            }
        }
        "glob" => {
            if !kwargs.is_empty() {
                return Err(mk_err("glob() does not accept keyword arguments".into()));
            }
            if positional.len() != 1 {
                return Err(mk_err(format!(
                    "glob() expects 1 argument, got {}",
                    positional.len()
                )));
            }
            let pat = expect_string(&positional[0], "glob() argument", &mk_err)?;
            let mut builder = GlobSetBuilder::new();
            let glob = GlobBuilder::new(&pat)
                .literal_separator(true)
                .build()
                .map_err(|e| mk_err(format!("invalid glob `{pat}`: {e}")))?;
            builder.add(glob);
            let set = builder
                .build()
                .map_err(|e| mk_err(format!("glob build failed: {e}")))?;

            let mut matches: Vec<String> = Vec::new();
            let root = &scope.base_dir;
            for entry in WalkDir::new(root).follow_links(false) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = match entry.path().strip_prefix(root) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if rel.starts_with(".neomake") {
                    continue;
                }
                // Globset uses `/`; on Windows, `WalkDir` produces `\\`.
                // Normalize so portable patterns match on every host and
                // the returned strings are also portable.
                let rel_str = if std::path::MAIN_SEPARATOR == '/' {
                    rel.to_string_lossy().into_owned()
                } else {
                    rel.to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                };
                if set.is_match(&rel_str) {
                    matches.push(rel_str);
                }
            }
            matches.sort();
            Ok(toml::Value::Array(
                matches.into_iter().map(toml::Value::String).collect(),
            ))
        }
        "exec" => {
            if !kwargs.is_empty() {
                return Err(mk_err("exec() does not accept keyword arguments".into()));
            }
            if positional.len() != 1 {
                return Err(mk_err(format!(
                    "exec() expects 1 argument, got {}",
                    positional.len()
                )));
            }
            let cmd = expect_string(&positional[0], "exec() argument", &mk_err)?;
            let invocation = shell::resolve();
            let output = Command::new(&invocation.program)
                .args(&invocation.args)
                .arg(&cmd)
                .current_dir(&scope.base_dir)
                .output()
                .map_err(|e| mk_err(format!("exec(`{cmd}`) failed to spawn: {e}")))?;
            if !output.status.success() {
                return Err(mk_err(format!(
                    "exec(`{cmd}`) exited with status {}",
                    output.status
                )));
            }
            let stdout = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
            Ok(toml::Value::String(stdout))
        }
        "git" => {
            if positional.len() != 3 {
                return Err(mk_err(format!("git() expects 3 positional arguments, got {}", positional.len())));
            }
            let repo = expect_string(&positional[0], "git() repo", &mk_err)?;
            let tag = expect_string(&positional[1], "git() tag", &mk_err)?;
            let dest_template = expect_string(&positional[2], "git() dest", &mk_err)?;

            let submodules = if let Some(v) = kwargs.get("submodules") {
                expect_bool(v, "git() submodules", &mk_err)?
            } else {
                false
            };

            let depth = if let Some(v) = kwargs.get("depth") {
                Some(expect_int(v, "git() depth", &mk_err)? as u32)
            } else {
                None
            };

            let basename = crate::fetch::extract_basename(&repo);
            let dest = crate::fetch::resolve_wildcard(&dest_template, &basename, None);

            let (path, sha) = crate::fetch::git_clone(&repo, &tag, &dest, submodules, depth)
                .map_err(|e| mk_err(format!("git() error: {e}")))?;

            let mut manifest = neomake_core::asset::AssetManifest::load(&scope.base_dir);
            manifest.add(path.clone(), neomake_core::asset::AssetRecord {
                kind: neomake_core::asset::AssetKind::Git { commit_sha: sha },
                source: repo,
            });
            manifest.save(&scope.base_dir);

            Ok(toml::Value::String(path.to_string_lossy().to_string()))
        }
        "fetch" => {
            if positional.len() != 2 {
                return Err(mk_err(format!("fetch() expects 2 positional arguments, got {}", positional.len())));
            }
            let url = expect_string(&positional[0], "fetch() url", &mk_err)?;
            let dest_template = expect_string(&positional[1], "fetch() dest", &mk_err)?;

            let sha256_expected = if let Some(v) = kwargs.get("sha256") {
                Some(expect_string(v, "fetch() sha256", &mk_err)?)
            } else {
                None
            };

            let alias = if let Some(v) = kwargs.get("alias") {
                Some(expect_string(v, "fetch() alias", &mk_err)?)
            } else {
                None
            };

            let extract = if let Some(v) = kwargs.get("extract") {
                expect_bool(v, "fetch() extract", &mk_err)?
            } else {
                false
            };

            let basename = crate::fetch::extract_basename(&url);
            let dest = crate::fetch::resolve_wildcard(&dest_template, &basename, alias.as_deref());

            let (path, sha) = crate::fetch::http_fetch(&url, &dest, sha256_expected.as_deref(), extract)
                .map_err(|e| mk_err(format!("fetch() error: {e}")))?;

            let mut manifest = neomake_core::asset::AssetManifest::load(&scope.base_dir);
            manifest.add(path.clone(), neomake_core::asset::AssetRecord {
                kind: neomake_core::asset::AssetKind::Fetch { content_sha256: sha },
                source: url,
            });
            manifest.save(&scope.base_dir);

            Ok(toml::Value::String(path.to_string_lossy().to_string()))
        }
        other => Err(mk_err(format!("unknown function `{other}`"))),
    }
}

fn expect_string(
    v: &toml::Value,
    label: &str,
    mk_err: &dyn Fn(String) -> TomlxError,
) -> Result<String, TomlxError> {
    match v {
        toml::Value::String(s) => Ok(s.clone()),
        other => Err(mk_err(format!(
            "{label} must be string, got {}",
            type_of(other)
        ))),
    }
}

fn expect_bool(
    v: &toml::Value,
    label: &str,
    mk_err: &dyn Fn(String) -> TomlxError,
) -> Result<bool, TomlxError> {
    match v {
        toml::Value::Boolean(b) => Ok(*b),
        other => Err(mk_err(format!(
            "{label} must be boolean, got {}",
            type_of(other)
        ))),
    }
}

fn expect_int(
    v: &toml::Value,
    label: &str,
    mk_err: &dyn Fn(String) -> TomlxError,
) -> Result<i64, TomlxError> {
    match v {
        toml::Value::Integer(i) => Ok(*i),
        other => Err(mk_err(format!(
            "{label} must be integer, got {}",
            type_of(other)
        ))),
    }
}

// -----------------------------------------------------------------------
// TOML value renderer (value → syntax).
// -----------------------------------------------------------------------

/// Render a [`toml::Value`] as an inline TOML literal.
///
/// Used to substitute evaluated TOMLX expressions back into the
/// pre-processed source. Not a general TOML serializer — it targets the
/// subset that can appear on the RHS of a single `key = value` line.
pub fn render_toml_value(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => render_string(s),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(dt) => dt.to_string(),
        toml::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(render_toml_value).collect();
            format!("[{}]", parts.join(", "))
        }
        toml::Value::Table(tbl) => {
            let parts: Vec<String> = tbl
                .iter()
                .map(|(k, v)| format!("{} = {}", render_key(k), render_toml_value(v)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
    }
}

fn render_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_string()
    } else {
        render_string(k)
    }
}

fn render_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn load_s(src: &str) -> Result<toml::Value, TomlxError> {
        load_str(src, &PathBuf::from("test.tomlx"))
    }

    #[test]
    fn plain_toml_degrades_cleanly() {
        let src = r#"
[tasks.build]
command = "cargo build"
deps = ["codegen"]
"#;
        let v = load_s(src).unwrap();
        let cmd = v
            .get("tasks")
            .and_then(|t| t.get("build"))
            .and_then(|b| b.get("command"))
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "cargo build");
    }

    #[test]
    fn variable_substitution() {
        let src = r#"
$profile = "release"

[tasks.build]
command = "cargo build --${profile}"
"#;
        let v = load_s(src).unwrap();
        assert_eq!(
            v.get("tasks")
                .and_then(|t| t.get("build"))
                .and_then(|b| b.get("command"))
                .and_then(|c| c.as_str())
                .unwrap(),
            "cargo build --release"
        );
    }

    #[test]
    fn conditional_value() {
        let src = r#"
$ci = true

[tasks.test]
command = if ${ci} { "make ci-test" } else { "make test" }
"#;
        let v = load_s(src).unwrap();
        assert_eq!(
            v.get("tasks")
                .and_then(|t| t.get("test"))
                .and_then(|b| b.get("command"))
                .and_then(|c| c.as_str())
                .unwrap(),
            "make ci-test"
        );
    }

    #[test]
    fn conditional_else_branch() {
        let src = r#"
$ci = false

[tasks.test]
command = if ${ci} { "make ci-test" } else { "make test" }
"#;
        let v = load_s(src).unwrap();
        assert_eq!(
            v.get("tasks")
                .and_then(|t| t.get("test"))
                .and_then(|b| b.get("command"))
                .and_then(|c| c.as_str())
                .unwrap(),
            "make test"
        );
    }

    #[test]
    fn env_function_with_default() {
        // Use a name unlikely to be in the environment.
        let src = r#"
$who = env("NEOMAKE_TEST_UNLIKELY_VAR_X9", "nobody")

[who]
name = ${who}
"#;
        let v = load_s(src).unwrap();
        assert_eq!(
            v.get("who")
                .and_then(|t| t.get("name"))
                .and_then(|s| s.as_str()),
            Some("nobody")
        );
    }

    #[test]
    fn string_concatenation() {
        let src = r#"
$suffix = "world"

[m]
msg = "hello " + ${suffix}
"#;
        let v = load_s(src).unwrap();
        assert_eq!(
            v.get("m")
                .and_then(|m| m.get("msg"))
                .and_then(|s| s.as_str()),
            Some("hello world")
        );
    }

    #[test]
    fn undefined_variable_errors() {
        let src = r#"
[x]
y = ${nope}
"#;
        // Need at least one TOMLX marker for the preprocessor to engage.
        // Include a sentinel by prefixing a var decl that's unused.
        let src = format!("$_sentinel = true\n{src}");
        let err = load_s(&src).unwrap_err();
        assert!(
            err.message.contains("undefined variable"),
            "{}",
            err.message
        );
    }

    #[test]
    fn array_with_interpolation() {
        let src = r#"
$name = "foo"

[t]
files = ["a", "${name}.rs", "b"]
"#;
        let v = load_s(src).unwrap();
        let arr = v
            .get("t")
            .and_then(|t| t.get("files"))
            .and_then(|a| a.as_array())
            .unwrap();
        assert_eq!(arr[1].as_str(), Some("foo.rs"));
    }

    #[test]
    fn render_toml_value_roundtrips() {
        let v = toml::Value::Array(vec![
            toml::Value::String("a".into()),
            toml::Value::Integer(42),
            toml::Value::Boolean(true),
        ]);
        let rendered = render_toml_value(&v);
        assert_eq!(rendered, r#"["a", 42, true]"#);
    }
}
