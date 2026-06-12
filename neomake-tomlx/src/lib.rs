//! TOMLX: a progressively enhanced superset of TOML for `neomake`.
//!
//! TOMLX adds three capabilities on top of TOML:
//!
//! 1. Top-level variable declarations: `$name = <expr>`.
//! 2. String interpolation and bare-value references: `${name}`.
//! 3. Value-level conditionals (`if cond { a } else { b }`) and a small
//!    library of built-in functions (`env`, `glob`, `exec`).
//!
//! # Graceful degradation
//!
//! Any valid `.toml` file is a valid `.tomlx` file with identical
//! semantics: the [`load_str`] entry point short-circuits to
//! `toml::from_str` when it detects no TOMLX sentinels in the source.
//!
//! # Pipeline
//!
//! Loading a TOMLX file is a strict three-stage pipeline so that the
//! TOMLX extensions never bleed into the underlying TOML parser:
//!
//! ```text
//! source ──► [lexer] ──► tokens ──► [parser] ──► AST ──► [evaluator] ──► toml::Value
//! ```
//!
//! The evaluator owns a document-level scanner that runs between the
//! TOMLX and TOML phases; it extracts variable declarations, substitutes
//! evaluated expressions, and hands a clean TOML string to
//! `toml::from_str`.

#![warn(missing_docs)]
#![deny(clippy::all)]

pub mod error;
pub mod evaluator;
pub mod fetch;
pub mod lexer;
pub mod parser;
mod shell;

pub use error::TomlxError;
pub use evaluator::{load, load_str, render_toml_value};
pub use lexer::{Span, SpannedToken, StrSegment, Token};
pub use parser::{parse_expr, BinOp, Expr, StringPart, UnaryOp};
