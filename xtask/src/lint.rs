//! `lint`: source rules that rustc and clippy cannot express.
//!
//! - **No `dyn`.** plx uses static dispatch only:
//!   generics and `impl Trait`, never trait objects. Every `.rs` file in the
//!   repository is checked.
//! - **Panic sites are inventoried.** Every `bug!`, `invariant!`,
//!   `breach!`, `contract!`, `.or_bug(…)` and `.or_contract(…)` in production
//!   code must
//!   state its invariant as a string literal, and the set of sites must match
//!   `invariants.lock` exactly. New sites show up in review as a lock diff;
//!   removed ones ratchet it down. `lint --bless` rewrites the lock.
//! - **No runtime asserts in production code**: `assert!` and
//!   `debug_assert!` (and their `_eq`/`_ne` forms) either vanish in release
//!   or bypass the inventory. `const _: () = assert!(…);` is compile-time and
//!   allowed.
//! - **Domain fields say what they mean**: in the domain crates
//!   ([`DOMAIN_CRATES`]), no struct or enum-variant field is a bare `bool`,
//!   `String` or `str`: those become enums and newtypes. A single-field
//!   tuple struct is that newtype, so it may wrap one.
//! - **xtask's lints mirror the workspace's**: xtask is a standalone
//!   workspace, so it cannot inherit `[workspace.lints]`; its copy must match,
//!   except for the few lints a command-line tool needs ([`XTASK_ALLOWS`]).
//!
//! Production code is `crates/*/src`, minus generated bindings, plx-bug
//! itself (which defines the macros) and `#[cfg(test)]` items. Tokenizing
//! (rather than grepping) ignores comments and string literals but still sees
//! inside macro bodies, where a syntax-tree visitor would not look.
//!
//! There is no escape hatch for `dyn` or asserts: if one is ever needed,
//! revisit the rule, don't route around it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

use crate::Failure;

/// Directories that hold plx's Rust sources.
const ROOTS: [&str; 3] = ["crates", "xtask", "research"];

/// The panic-site inventory, relative to the repository root.
const LOCK: &str = "invariants.lock";

/// Lints xtask sets to `allow` although the workspace denies them: a
/// command-line tool reports on stderr.
const XTASK_ALLOWS: [&str; 2] = ["print_stderr", "print_stdout"];

/// Crates whose types model the domain, where fields must be typed.
const DOMAIN_CRATES: [&str; 5] = [
    "plx-core",
    "plx-emulator",
    "plx-protocol",
    "plx-config",
    "plx-client-core",
];

/// Types a domain field must not be (or contain).
const RAW_FIELD_TYPES: [&str; 3] = ["bool", "String", "str"];

/// Runtime assert macros, banned in production code.
const ASSERTS: [&str; 6] = [
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
];

/// Whether `lint` checks `invariants.lock` or rewrites it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
    Check,
    Bless,
}

pub(crate) fn run(root: &Path, lock: Lock) -> Result<(), Failure> {
    let mut files = Vec::new();
    for dir in ROOTS {
        collect_rust_files(root, &root.join(dir), &mut files)?;
    }
    files.sort();

    let mut findings = Vec::new();
    let mut sites = Vec::new();
    for file in &files {
        let source = fs::read_to_string(root.join(file))
            .map_err(|e| Failure(format!("cannot read {}: {e}", file.display())))?;
        let shown = file.display();
        for (line, column) in dyn_positions(tokenize(file, &source)?) {
            findings.push(format!(
                "{shown}:{line}:{}: trait object (`dyn`) is not allowed; use generics or `impl Trait`",
                column + 1
            ));
        }
        if is_production(file) {
            // Tokenized again rather than copied: the copy ban covers xtask.
            let scan = scan_production(tokenize(file, &source)?);
            for (line, name) in scan.asserts {
                findings.push(format!(
                    "{shown}:{line}: `{name}!` in production code; state the invariant with plx_bug::invariant!"
                ));
            }
            for line in scan.direct_failures {
                findings.push(format!(
                    "{shown}:{line}: call plx-bug's macros or OrBug, not `__fail` (it bypasses the inventory)"
                ));
            }
            for (line, kind) in scan.unlabelled {
                findings.push(format!(
                    "{shown}:{line}: `{kind}` needs a string literal stating the invariant"
                ));
            }
            for site in scan.sites {
                sites.push(format!("{shown} | {} | {}", site.kind, site.message));
            }
            if is_domain(file) {
                for (line, ty) in raw_fields(tokenize(file, &source)?) {
                    findings.push(format!(
                        "{shown}:{line}: a domain field of type `{ty}`; give it a type that says what it means (an enum or a newtype)"
                    ));
                }
            }
        }
    }
    sites.sort();
    findings.extend(lint_mirror(root)?);

    let lock_path = root.join(LOCK);
    match lock {
        Lock::Bless => {
            let mut text = String::from(LOCK_HEADER);
            for site in &sites {
                text.push_str(site);
                text.push('\n');
            }
            fs::write(&lock_path, text)?;
            eprintln!("lint: wrote {LOCK} ({} panic sites)", sites.len());
        }
        Lock::Check => findings.extend(compare_lock(&lock_path, &sites)?),
    }

    if !findings.is_empty() {
        for finding in &findings {
            eprintln!("{finding}");
        }
        return Err(Failure(format!(
            "{} lint finding(s) in {} files",
            findings.len(),
            files.len()
        )));
    }
    eprintln!(
        "lint: {} files clean, {} panic sites match {LOCK}",
        files.len(),
        sites.len()
    );
    Ok(())
}

const LOCK_HEADER: &str = "\
# Every panic site in plx's production code:
# file | kind | the invariant it states. Generated by
# `cargo run --manifest-path xtask/Cargo.toml -- lint --bless`; review the
# diff: each new line is an invariant the types could not express.
";

/// Differences between the sites found and the lock, both ways.
fn compare_lock(lock_path: &Path, sites: &[String]) -> Result<Vec<String>, Failure> {
    let text = match fs::read_to_string(lock_path) {
        Ok(text) => text,
        Err(e) => {
            return Err(Failure(format!(
                "cannot read {LOCK} ({e}); create it with `lint --bless`"
            )));
        }
    };
    let mut expected: BTreeMap<&str, isize> = BTreeMap::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        *expected.entry(line).or_insert(0) += 1;
    }
    for site in sites {
        *expected.entry(site.as_str()).or_insert(0) -= 1;
    }
    let mut findings = Vec::new();
    for (site, balance) in expected {
        for _ in 0..balance.unsigned_abs() {
            findings.push(if balance > 0 {
                format!("{LOCK} lists a panic site that no longer exists: {site} (run `lint --bless`)")
            } else {
                format!("new panic site, not in {LOCK}: {site} (review it, then run `lint --bless`)")
            });
        }
    }
    Ok(findings)
}

fn tokenize(file: &Path, source: &str) -> Result<TokenStream, Failure> {
    TokenStream::from_str(source)
        .map_err(|e| Failure(format!("{} does not tokenize: {e}", file.display())))
}

/// Every `.rs` file under `dir`, relative to `root`, skipping `target`
/// directories.
fn collect_rust_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Failure> {
    for entry in
        fs::read_dir(dir).map_err(|e| Failure(format!("cannot list {}: {e}", dir.display())))?
    {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect_rust_files(root, &path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path
                .strip_prefix(root)
                .map_err(|e| Failure(format!("{} is outside the repository: {e}", path.display())))?;
            out.push(relative.to_path_buf());
        }
    }
    Ok(())
}

/// Production code: `crates/<crate>/src/**`, except generated bindings and
/// plx-bug, which defines the panic macros.
fn is_production(file: &Path) -> bool {
    let mut parts = file.iter();
    let (Some(top), Some(krate), Some(src)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    top == "crates" && src == "src" && krate != "plx-bug" && !file.ends_with("bindings.rs")
}

/// Whether `file` is production code of a domain crate.
fn is_domain(file: &Path) -> bool {
    is_production(file)
        && file
            .iter()
            .nth(1)
            .is_some_and(|krate| DOMAIN_CRATES.iter().any(|domain| krate == *domain))
}

/// `(line, type)` of every struct or enum-variant field whose type mentions
/// a raw type, except the one field of a tuple newtype. Skips `#[cfg(test)]`
/// items.
fn raw_fields(tokens: TokenStream) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    raw_fields_in(tokens, &mut found);
    found
}

fn raw_fields_in(stream: TokenStream, found: &mut Vec<(usize, String)>) {
    let trees: Vec<TokenTree> = stream.into_iter().collect();
    let mut i = 0;
    while let Some(tree) = trees.get(i) {
        match tree {
            TokenTree::Punct(punct) if punct.as_char() == '#' && is_cfg_test(trees.get(i + 1)) => {
                i = skip_item(&trees, i + 2);
                continue;
            }
            TokenTree::Ident(ident) if ident == "struct" || ident == "enum" => {
                let is_enum = ident == "enum";
                // The body is the first group after the name and generics.
                if let Some(TokenTree::Group(body)) = trees
                    .get(i + 1..)
                    .and_then(|rest| rest.iter().find(|t| matches!(t, TokenTree::Group(_))))
                {
                    if is_enum {
                        variant_fields(body.stream(), found);
                    } else {
                        fields(body.delimiter(), body.stream(), Newtype::Allowed, found);
                    }
                }
            }
            TokenTree::Group(group) => raw_fields_in(group.stream(), found),
            TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
        i += 1;
    }
}

/// Whether a single-field tuple body counts as a newtype.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Newtype {
    Allowed,
    NotAllowed,
}

/// The fields of each variant of an enum body.
fn variant_fields(body: TokenStream, found: &mut Vec<(usize, String)>) {
    for tree in body {
        if let TokenTree::Group(group) = tree {
            match group.delimiter() {
                Delimiter::Brace | Delimiter::Parenthesis => {
                    fields(group.delimiter(), group.stream(), Newtype::NotAllowed, found);
                }
                // Attributes and discriminants carry no fields.
                Delimiter::Bracket | Delimiter::None => {}
            }
        }
    }
}

/// Checks the fields of a `{ name: Type, .. }` or `(Type, ..)` body.
fn fields(delimiter: Delimiter, body: TokenStream, newtype: Newtype, found: &mut Vec<(usize, String)>) {
    let mut field: Vec<TokenTree> = Vec::new();
    let mut all: Vec<Vec<TokenTree>> = Vec::new();
    for tree in body {
        match &tree {
            TokenTree::Punct(punct) if punct.as_char() == ',' => all.push(core::mem::take(&mut field)),
            TokenTree::Group(_) | TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {
                field.push(tree);
            }
        }
    }
    if !field.is_empty() {
        all.push(field);
    }
    if delimiter == Delimiter::Parenthesis && newtype == Newtype::Allowed && all.len() == 1 {
        return;
    }
    for field in all {
        let ty = match delimiter {
            // The type follows the first lone `:` (not part of a `::` path).
            Delimiter::Brace => field_type(&field),
            Delimiter::Parenthesis | Delimiter::Bracket | Delimiter::None => &field[..],
        };
        if let Some((line, raw)) = first_raw_type(ty) {
            found.push((line, raw));
        }
    }
}

fn field_type(field: &[TokenTree]) -> &[TokenTree] {
    let mut i = 0;
    while let Some(tree) = field.get(i) {
        if let TokenTree::Punct(punct) = tree
            && punct.as_char() == ':'
        {
            let joined = matches!(field.get(i + 1), Some(TokenTree::Punct(next)) if next.as_char() == ':');
            if !joined {
                return field.get(i + 1..).unwrap_or(&[]);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    &[]
}

/// The first raw type mentioned in `ty`, looking inside groups too.
fn first_raw_type(ty: &[TokenTree]) -> Option<(usize, String)> {
    for tree in ty {
        match tree {
            TokenTree::Ident(ident) => {
                let name = format!("{ident}");
                if RAW_FIELD_TYPES.contains(&name.as_str()) {
                    return Some((ident.span().start().line, name));
                }
            }
            TokenTree::Group(group) => {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                if let Some(found) = first_raw_type(&inner) {
                    return Some(found);
                }
            }
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
    None
}

/// Line and zero-based column of every `dyn` keyword, including inside
/// macro invocations and `macro_rules!` bodies.
fn dyn_positions(tokens: TokenStream) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut stack = vec![tokens];
    while let Some(stream) = stack.pop() {
        for tree in stream {
            match tree {
                TokenTree::Ident(ident) if ident == "dyn" => {
                    let start = ident.span().start();
                    found.push((start.line, start.column));
                }
                TokenTree::Group(group) => stack.push(group.stream()),
                TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
        }
    }
    found.sort_unstable();
    found
}

/// A panic site and the invariant it states.
#[derive(Debug, PartialEq, Eq)]
struct Site {
    kind: &'static str,
    /// The message literal as written, quotes included.
    message: String,
}

/// What a production file contains.
#[derive(Debug, Default)]
struct Scan {
    sites: Vec<Site>,
    /// `(line, kind)` of sites without a literal message.
    unlabelled: Vec<(usize, &'static str)>,
    /// `(line, macro)` of runtime asserts.
    asserts: Vec<(usize, String)>,
    /// Lines calling plx-bug's internal failure function directly, which
    /// would bypass the inventory.
    direct_failures: Vec<usize>,
}

fn scan_production(tokens: TokenStream) -> Scan {
    let mut scan = Scan::default();
    scan_stream(tokens, &mut scan);
    scan.sites.sort_by(|a, b| (a.kind, &a.message).cmp(&(b.kind, &b.message)));
    scan
}

/// Scans one token stream, recursing into groups, skipping `#[cfg(test)]`
/// items.
fn scan_stream(stream: TokenStream, scan: &mut Scan) {
    let trees: Vec<TokenTree> = stream.into_iter().collect();
    let mut i = 0;
    // Whether the current item (since the last `;` at this level) began with
    // `const`, which makes a top-level `assert!` compile-time.
    let mut const_item = false;
    while let Some(tree) = trees.get(i) {
        match tree {
            TokenTree::Punct(punct) if punct.as_char() == '#' && is_cfg_test(trees.get(i + 1)) => {
                i = skip_item(&trees, i + 2);
                continue;
            }
            TokenTree::Punct(punct) if punct.as_char() == ';' => const_item = false,
            TokenTree::Ident(ident) if ident == "const" => const_item = true,
            TokenTree::Ident(ident) if ident == "__fail" => {
                scan.direct_failures.push(ident.span().start().line);
            }
            TokenTree::Ident(ident) if is_bang(trees.get(i + 1)) => {
                let name = format!("{ident}");
                let line = ident.span().start().line;
                let args = match trees.get(i + 2) {
                    Some(TokenTree::Group(group)) => Some(group.stream()),
                    _ => None,
                };
                if ASSERTS.contains(&name.as_str()) && !const_item {
                    scan.asserts.push((line, name));
                } else if let Some(kind) = macro_kind(&name) {
                    match args.and_then(|args| macro_message(kind, args)) {
                        Some(message) => scan.sites.push(Site { kind, message }),
                        None => scan.unlabelled.push((line, kind)),
                    }
                }
            }
            TokenTree::Punct(punct) if punct.as_char() == '.' => {
                if let (Some(TokenTree::Ident(method)), Some(TokenTree::Group(args))) =
                    (trees.get(i + 1), trees.get(i + 2))
                    && let Some(kind) = method_kind(&format!("{method}"))
                    && args.delimiter() == Delimiter::Parenthesis
                {
                    match first_string_literal(args.stream()) {
                        Some(message) => scan.sites.push(Site { kind, message }),
                        None => scan.unlabelled.push((method.span().start().line, kind)),
                    }
                }
            }
            TokenTree::Group(group) => scan_stream(group.stream(), scan),
            TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
        i += 1;
    }
}

fn macro_kind(name: &str) -> Option<&'static str> {
    match name {
        "bug" => Some("bug!"),
        "invariant" => Some("invariant!"),
        "contract" => Some("contract!"),
        "breach" => Some("breach!"),
        _ => None,
    }
}

fn method_kind(name: &str) -> Option<&'static str> {
    match name {
        "or_bug" => Some("or_bug"),
        "or_contract" => Some("or_contract"),
        _ => None,
    }
}

fn is_bang(tree: Option<&TokenTree>) -> bool {
    matches!(tree, Some(TokenTree::Punct(punct)) if punct.as_char() == '!')
}

/// Whether `tree` is the `[cfg(test)]` of an attribute.
fn is_cfg_test(tree: Option<&TokenTree>) -> bool {
    let Some(TokenTree::Group(group)) = tree else {
        return false;
    };
    group.delimiter() == Delimiter::Bracket
        && format!("{}", group.stream()).replace(' ', "") == "cfg(test)"
}

/// The index just past the item starting at `start`: through its first
/// brace group, or its terminating `;`, whichever comes first.
fn skip_item(trees: &[TokenTree], start: usize) -> usize {
    let mut i = start;
    while let Some(tree) = trees.get(i) {
        i += 1;
        match tree {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => return i,
            TokenTree::Punct(punct) if punct.as_char() == ';' => return i,
            TokenTree::Group(_) | TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
    i
}

/// The message of `bug!(msg, …)`, or of `invariant!`/`contract!(cond, msg, …)`:
/// a string literal, first or right after the first top-level comma.
fn macro_message(kind: &str, args: TokenStream) -> Option<String> {
    if kind == "bug!" || kind == "breach!" {
        return first_string_literal(args);
    }
    let mut trees = args.into_iter();
    trees.by_ref().find(|tree| matches!(tree, TokenTree::Punct(p) if p.as_char() == ','))?;
    first_string_literal(trees.collect())
}

/// The first token, if it is a string literal, as written.
fn first_string_literal(args: TokenStream) -> Option<String> {
    match args.into_iter().next() {
        Some(TokenTree::Literal(literal)) => {
            let text = format!("{literal}");
            text.starts_with('"').then_some(text)
        }
        _ => None,
    }
}

/// xtask's `[lints]` against the workspace's `[workspace.lints]`.
fn lint_mirror(root: &Path) -> Result<Vec<String>, Failure> {
    let workspace = lints(&root.join("Cargo.toml"), &["workspace", "lints"])?;
    let xtask = lints(&root.join("xtask/Cargo.toml"), &["lints"])?;
    let mut findings = Vec::new();
    for (key, level) in &workspace {
        let allowed = XTASK_ALLOWS.iter().any(|lint| key.ends_with(&format!(".{lint}")));
        let want = if allowed { "\"allow\"" } else { level.as_str() };
        match xtask.get(key) {
            Some(got) if got == want => {}
            Some(got) => findings.push(format!(
                "xtask/Cargo.toml: lint {key} is {got}, the workspace mirror needs {want}"
            )),
            None => findings.push(format!(
                "xtask/Cargo.toml: lint {key} is missing; mirror the workspace's {want}"
            )),
        }
    }
    for key in xtask.keys().filter(|key| !workspace.contains_key(*key)) {
        findings.push(format!(
            "xtask/Cargo.toml: lint {key} is not in the workspace's [workspace.lints]"
        ));
    }
    Ok(findings)
}

/// `tool.lint` to its setting (as TOML text), from the table at `path`.
fn lints(manifest: &Path, path: &[&str]) -> Result<BTreeMap<String, String>, Failure> {
    let text = fs::read_to_string(manifest)
        .map_err(|e| Failure(format!("cannot read {}: {e}", manifest.display())))?;
    let mut table: toml::Table = toml::from_str(&text)
        .map_err(|e| Failure(format!("{} is not valid TOML: {e}", manifest.display())))?;
    for key in path {
        table = match table.remove(*key) {
            Some(toml::Value::Table(inner)) => inner,
            Some(_) | None => {
                return Err(Failure(format!(
                    "{} has no [{}] table",
                    manifest.display(),
                    path.join(".")
                )));
            }
        };
    }
    let mut out = BTreeMap::new();
    for (tool, lints) in table {
        let toml::Value::Table(lints) = lints else {
            return Err(Failure(format!("{}: [{tool}] lints are not a table", manifest.display())));
        };
        for (lint, setting) in lints {
            out.insert(format!("{tool}.{lint}"), format!("{setting}"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::str::FromStr;

    use proc_macro2::TokenStream;

    use super::{Site, dyn_positions, is_production, raw_fields, scan_production};

    fn tokens(source: &str) -> TokenStream {
        TokenStream::from_str(source).unwrap()
    }

    #[test]
    fn finds_trait_objects_everywhere_code_can_hide_them() {
        let source = "fn a(x: &dyn Fn()) {}\nmacro_rules! m { () => { Box<dyn Send> } }\nfn b() { let v = vec![Box::new(1) as Box<dyn Send>]; }";
        assert_eq!(dyn_positions(tokens(source)), vec![(1, 9), (2, 29), (3, 41)]);
    }

    #[test]
    fn ignores_comments_strings_and_raw_identifiers() {
        let source = "// dyn in a comment\n/* dyn */ fn a() { let s = \"dyn\"; let r#dyn = 1; }";
        assert_eq!(dyn_positions(tokens(source)), []);
    }

    #[test]
    fn inventories_every_kind_of_panic_site() {
        let source = r#"
            fn f(x: Option<u8>) -> u8 {
                invariant!(a(b, c) == 1, "a holds {}", 1);
                contract!(ok, "libghostty keeps {x}");
                if no { bug!("never {}", 2) }
                match k { 1 => {}, _ => breach!("unknown {}", k) }
                let y = x.or_contract("present").max(1);
                x.or_bug("present")
            }
        "#;
        let scan = scan_production(tokens(source));
        let sites: Vec<(&str, &str)> = scan
            .sites
            .iter()
            .map(|Site { kind, message }| (*kind, message.as_str()))
            .collect();
        assert_eq!(
            sites,
            [
                ("breach!", "\"unknown {}\""),
                ("bug!", "\"never {}\""),
                ("contract!", "\"libghostty keeps {x}\""),
                ("invariant!", "\"a holds {}\""),
                ("or_bug", "\"present\""),
                ("or_contract", "\"present\""),
            ]
        );
        assert_eq!(scan.unlabelled, []);
        assert_eq!(scan.asserts, []);
    }

    #[test]
    fn sites_need_a_literal_message() {
        let source = "fn f() { bug!(MESSAGE); x.or_bug(what); invariant!(a, b); }\nfn g() { plx_bug::__fail(k, m) }";
        let scan = scan_production(tokens(source));
        assert_eq!(
            scan.unlabelled,
            [(1, "bug!"), (1, "or_bug"), (1, "invariant!")]
        );
        assert_eq!(scan.direct_failures, [2]);
    }

    #[test]
    fn bans_runtime_asserts_but_not_const_ones_or_tests() {
        let source = "
            const _: () = assert!(size_of::<u8>() == 1);
            fn f() { debug_assert!(true); assert_eq!(1, 1); }
            #[cfg(test)]
            mod tests { fn t() { assert!(true); bug!(\"in a test\"); } }
            #[cfg(test)]
            fn helper() { assert_ne!(1, 2); }
            fn g() { assert!(late) }
        ";
        let scan = scan_production(tokens(source));
        let asserts: Vec<(usize, &str)> = scan
            .asserts
            .iter()
            .map(|(line, name)| (*line, name.as_str()))
            .collect();
        assert_eq!(
            asserts,
            [(3, "debug_assert"), (3, "assert_eq"), (8, "assert")]
        );
        assert_eq!(scan.sites, []);
    }

    #[test]
    fn domain_fields_must_not_be_raw() {
        let source = "
            pub struct Name(Box<str>);
            struct Pair(String, u8);
            pub struct Tab { pub(crate) name: Option<String>, id: crate::id::TabId, zoomed: bool }
            enum Event { Renamed { name: Name }, Flag(bool), Plain, Tagged = 3 }
            #[cfg(test)]
            mod tests { struct Probe { ok: bool } }
        ";
        let found: Vec<(usize, String)> = raw_fields(tokens(source));
        let expected: Vec<(usize, String)> = [(3, "String"), (4, "String"), (4, "bool"), (5, "bool")]
            .into_iter()
            .map(|(line, ty)| (line, String::from(ty)))
            .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn production_code_is_crate_sources_minus_bindings_and_plx_bug() {
        assert!(is_production(Path::new("crates/plx-core/src/layout.rs")));
        assert!(!is_production(Path::new("crates/plx-core/tests/it/layout.rs")));
        assert!(!is_production(Path::new("crates/plx-ghostty-sys/src/bindings.rs")));
        assert!(!is_production(Path::new("crates/plx-bug/src/lib.rs")));
        assert!(!is_production(Path::new("xtask/src/lint.rs")));
    }
}
