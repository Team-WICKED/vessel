//! TypeScript declarations of the wire, for the web UI (ADR 0017).
//!
//! [Decision Log]
//! - Purpose: the browser builds `Call`s and reads `Reply`s and events in
//!   the shapes this crate defines. Hand-written TypeScript would drift
//!   from them silently; declarations generated from these types cannot.
//! - Constraint: one wire contract, defined here. The generator changes
//!   nothing on the wire: `ts_rs::TS` derives sit behind this crate's `ts`
//!   feature, which no shipped binary enables.
//! - Alternatives: ts-rs's own export (one file per type, written by
//!   generated tests to a directory set by an environment variable, with
//!   64-bit integers as `bigint`, which `JSON.parse` never produces);
//!   JSON Schema plus a schema-to-TypeScript tool (a second toolchain, and
//!   serde's tagged enums lose their discriminants on the way).
//! - Choice: ts-rs's derives for the declarations, gathered here into one
//!   file from a few roots (everything reachable from `Call`, `Reply`, the
//!   event log and errors), with 64-bit integers as `number`. A test
//!   compares the file with what the types declare now and fails on any
//!   difference; `VESSEL_WRITE_TS=1` rewrites it.
//! - Generic types (protocol 7's `lsp::Page<T>`) are declared once, with
//!   their parameter; two Rust types with one TypeScript name clash only
//!   when their declarations differ.
//! - Cost: `u64` values above 2^53 lose precision in a browser. Counters
//!   (`EventSeq`, request ids) and byte counts stay far below it. A type
//!   with a hand-written serde impl, or a field with `serde(with)`, needs a
//!   `ts(type = "...")` override beside it, since ts-rs cannot see what the
//!   impl writes; the generated file's diff shows such a change in review.

use std::any::TypeId;
use std::collections::BTreeMap;
use std::fmt;

use ts_rs::{Config, TS, TypeVisitor};

use crate::{Call, EventRecord, Method, PairingOffer, Reply, SubscriptionEnd, WireError};

/// Where the web UI keeps the declarations, from the workspace's root.
pub const PROTO_FILE: &str = "web/src/wire/proto.ts";

/// The note each generated file starts with.
pub const NOTE: &str = "\
// Generated from the Rust wire types (ADR 0017). Do not edit: running
// `VESSEL_WRITE_TS=1 cargo test --features ts typescript` in the crate that
// generates this file rewrites it, and CI fails while it differs.
";

/// TypeScript declarations gathered from Rust types and everything they
/// refer to, by name.
pub struct Declarations {
    config: Config,
    types: BTreeMap<String, Declared>,
}

/// One declaration and the type it came from.
struct Declared {
    id: TypeId,
    text: String,
}

/// Two different Rust types declare the same TypeScript name; one file
/// cannot hold both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameClash {
    /// The name both declare.
    pub name: String,
}

impl fmt::Display for NameClash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "two wire types are both named `{}` in TypeScript; rename one with `#[ts(rename)]`",
            self.name
        )
    }
}

impl std::error::Error for NameClash {}

impl fmt::Debug for Declarations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Declarations")
            .field("types", &self.types.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Default for Declarations {
    fn default() -> Self {
        Self::new()
    }
}

impl Declarations {
    /// None yet; 64-bit integers are declared as `number`, which is what
    /// `JSON.parse` gives a browser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: Config::new().with_large_int("number"),
            types: BTreeMap::new(),
        }
    }

    /// The wire: everything a client sends to a node or reads from one,
    /// pairing offers included.
    ///
    /// # Errors
    /// Two wire types share a TypeScript name.
    pub fn wire() -> Result<Self, NameClash> {
        let mut declarations = Self::new();
        declarations.add::<Call>()?;
        declarations.add::<Reply>()?;
        declarations.add::<Method>()?;
        declarations.add::<WireError>()?;
        declarations.add::<EventRecord>()?;
        declarations.add::<SubscriptionEnd>()?;
        declarations.add::<PairingOffer>()?;
        Ok(declarations)
    }

    /// Declare `T` and every type it refers to.
    ///
    /// # Errors
    /// One of them shares its TypeScript name with a different type
    /// already declared.
    pub fn add<T: TS + 'static + ?Sized>(&mut self) -> Result<(), NameClash> {
        let mut visit = Visit {
            declarations: self,
            clash: None,
        };
        visit.visit::<T>();
        visit.clash.map_or(Ok(()), Err)
    }

    /// Whether `name` is declared.
    #[must_use]
    pub fn declares(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    /// The declared names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.types.keys().map(String::as_str)
    }

    /// A TypeScript module: `NOTE`, then `import type { .. } from "<from>"`
    /// for the names `base` declares and these declarations use, then each
    /// declaration not already in `base`, sorted by name.
    #[must_use]
    pub fn render(&self, base: Option<(&Self, &str)>) -> String {
        let mut text = String::from(NOTE);
        if let Some((base, from)) = base {
            let ours: Vec<&Declared> = self
                .types
                .iter()
                .filter(|(name, _declared)| !base.declares(name))
                .map(|(_name, declared)| declared)
                .collect();
            let imported: Vec<&str> = base
                .names()
                .filter(|name| ours.iter().any(|declared| mentions(&declared.text, name)))
                .collect();
            if !imported.is_empty() {
                text.push('\n');
                text.push_str("import type { ");
                text.push_str(&imported.join(", "));
                text.push_str(" } from \"");
                text.push_str(from);
                text.push_str("\";\n");
            }
        }
        for (name, declared) in &self.types {
            if base.is_some_and(|(base, _from)| base.declares(name)) {
                continue;
            }
            text.push('\n');
            text.push_str(&declared.text);
            text.push('\n');
        }
        text
    }
}

/// `code` with its double-quoted string literals emptied.
fn outside_strings(code: &str) -> String {
    let mut kept = String::with_capacity(code.len());
    let mut quoted = false;
    let mut escaped = false;
    for character in code.chars() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
                kept.push(character);
            }
        } else {
            if character == '"' {
                quoted = true;
            }
            kept.push(character);
        }
    }
    kept
}

/// Whether `text` uses the identifier `name` (not as part of a longer one).
fn mentions(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _found)| {
        let before = text.get(..at).and_then(|head| head.chars().next_back());
        let after = at
            .checked_add(name.len())
            .and_then(|end| text.get(end..))
            .and_then(|tail| tail.chars().next());
        !before.is_some_and(identifier) && !after.is_some_and(identifier)
    })
}

fn identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/// Walks a type's dependencies, declaring each declarable one once.
struct Visit<'a> {
    declarations: &'a mut Declarations,
    clash: Option<NameClash>,
}

impl TypeVisitor for Visit<'_> {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        // Primitives and containers have no file of their own in ts-rs,
        // and nothing to declare.
        if self.clash.is_some() || T::output_path().is_none() {
            return;
        }
        let name = T::ident(&self.declarations.config);
        let id = TypeId::of::<T>();
        let mut text = T::docs().unwrap_or_default();
        text.push_str("export ");
        text.push_str(&T::decl(&self.declarations.config));
        if let Some(declared) = self.declarations.types.get(&name) {
            // Instantiations of one generic type (`Page<Symbol>`,
            // `Page<Location>`) are different Rust types with one
            // declaration, `Page<T>`; only their arguments are new.
            if declared.id != id && declared.text != text {
                self.clash = Some(NameClash { name });
            } else if declared.id != id {
                T::visit_generics(self);
            }
            return;
        }
        let _first = self.declarations.types.insert(name, Declared { id, text });
        T::visit_dependencies(self);
        T::visit_generics(self);
    }
}

/// Checks a generated module against the file it is kept in, the way both
/// crates' tests do: equal, or rewritten when `VESSEL_WRITE_TS` is set.
///
/// # Errors
/// The file differs (or cannot be read) and `VESSEL_WRITE_TS` is not set,
/// or writing it failed. The message says how to regenerate it.
pub fn check_file(path: &std::path::Path, generated: &str) -> Result<(), String> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current == generated {
        return Ok(());
    }
    if std::env::var_os("VESSEL_WRITE_TS").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        return std::fs::write(path, generated).map_err(|error| error.to_string());
    }
    Err(format!(
        "{} is not what the Rust types declare; rerun with VESSEL_WRITE_TS=1 and commit the result",
        path.display()
    ))
}

/// What the declarations must not contain: `any` anywhere, or `unknown`
/// outside `RawJson` (ADR 0017). Comments and string literal types (a
/// `"unknown"` freshness is a value, not the type) are not checked.
///
/// # Errors
/// The first offending line.
pub fn check_strict(generated: &str) -> Result<(), String> {
    let mut in_comment = false;
    for line in generated.lines() {
        let code = line.trim_start();
        if code.starts_with("/**") || code.starts_with("/*") {
            in_comment = !code.contains("*/");
            continue;
        }
        if in_comment {
            in_comment = !code.contains("*/");
            continue;
        }
        if code.starts_with("//") {
            continue;
        }
        let code = &outside_strings(code);
        if mentions(code, "any") || mentions(code, "bigint") {
            return Err(format!("`any` or `bigint` in a declaration: {line}"));
        }
        if mentions(code, "unknown") && code != "export type RawJson = unknown;" {
            return Err(format!("`unknown` outside `RawJson`: {line}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn typescript_declarations_are_current() {
        let generated = Declarations::wire().unwrap().render(None);
        check_strict(&generated).unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(PROTO_FILE);
        check_file(&path, &generated).unwrap();
    }

    #[test]
    fn identifiers_are_matched_whole() {
        assert!(mentions("{ id: NodeId }", "NodeId"));
        assert!(!mentions("{ id: NodeIdentity }", "NodeId"));
        assert!(!mentions("{ id: MyNodeId }", "NodeId"));
        assert!(check_strict("export type A = { a: any };").is_err());
        assert!(check_strict("export type B = { b: unknown };").is_err());
        assert!(check_strict("/** any node */\nexport type C = string;").is_ok());
        assert!(check_strict("export type RawJson = unknown;").is_ok());
        // A string literal type is a value, not `any` or `unknown`.
        assert!(check_strict(r#"export type F = "saved" | "unknown";"#).is_ok());
        assert!(check_strict(r#"export type G = { "any": string };"#).is_ok());
        assert!(check_strict(r#"export type H = { a: "x\"" | any };"#).is_err());
    }
}
