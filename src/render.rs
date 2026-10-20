use crate::semantic::symbol_container;
use scip::symbol::{format_symbol_with, parse_symbol, SymbolFormatOptions};
use scip::types::{descriptor, symbol_information, SymbolInformation};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const DOC_CHAR_LIMIT: usize = 200;
const SOURCE_LINE_CHAR_LIMIT: usize = 140;
pub(crate) fn display_name(information: &SymbolInformation) -> String {
    let explicit = information.display_name.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }

    if let Ok(symbol) = parse_symbol(&information.symbol) {
        if let Some(descriptor) = symbol.descriptors.last() {
            return descriptor.name.clone();
        }
    }

    information
        .symbol
        .split_whitespace()
        .last()
        .unwrap_or(&information.symbol)
        .trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '$'
        })
        .to_string()
}

pub(crate) fn qualified_name(information: &SymbolInformation) -> String {
    parse_symbol(&information.symbol)
        .ok()
        .map(|symbol| {
            format_symbol_with(
                symbol,
                SymbolFormatOptions {
                    include_scheme: false,
                    include_package_manager: false,
                    include_package_name: false,
                    include_package_version: false,
                    include_descriptor: true,
                },
            )
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| display_name(information))
}

pub(crate) fn inferred_kind(information: &SymbolInformation) -> String {
    if let Ok(kind) = information.kind.enum_value() {
        if kind != symbol_information::Kind::UnspecifiedKind {
            return format!("{kind:?}").to_ascii_lowercase();
        }
    }

    parse_symbol(&information.symbol)
        .ok()
        .and_then(|symbol| symbol.descriptors.last().cloned())
        .and_then(|descriptor| descriptor.suffix.enum_value().ok())
        .map(|suffix| match suffix {
            descriptor::Suffix::Method => "method",
            descriptor::Suffix::Type => "type",
            descriptor::Suffix::Term => "symbol",
            descriptor::Suffix::Namespace | descriptor::Suffix::Package => "namespace",
            descriptor::Suffix::TypeParameter => "typeparameter",
            descriptor::Suffix::Parameter => "parameter",
            descriptor::Suffix::Meta => "meta",
            descriptor::Suffix::Local => "local",
            descriptor::Suffix::Macro => "macro",
            descriptor::Suffix::UnspecifiedSuffix => "symbol",
        })
        .unwrap_or("symbol")
        .to_string()
}

pub(crate) fn symbol_label(information: &SymbolInformation) -> String {
    let kind = inferred_kind(information);
    let name = display_name(information);
    match symbol_container(information) {
        Some(container) => format!("{kind} {name} ({container})"),
        None => format!("{kind} {name}"),
    }
}

fn compact_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut compact = text
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    compact.push('…');
    compact
}

pub(crate) struct SourceCache<'a> {
    project_root: &'a Path,
    files: HashMap<String, Option<Vec<String>>>,
}

impl<'a> SourceCache<'a> {
    pub(crate) fn new(project_root: &'a Path) -> Self {
        Self {
            project_root,
            files: HashMap::new(),
        }
    }

    pub(crate) fn line(&mut self, file: &str, line: usize) -> Option<String> {
        let lines = self.files.entry(file.to_string()).or_insert_with(|| {
            fs::read_to_string(self.project_root.join(file))
                .ok()
                .map(|source| source.lines().map(str::to_string).collect())
        });
        lines.as_ref()?.get(line).cloned()
    }

    pub(crate) fn display_line(&mut self, file: &str, line: usize) -> Option<String> {
        self.line(file, line)
            .map(|source| truncate_chars(source.trim(), SOURCE_LINE_CHAR_LIMIT))
    }

    /// Tells if a reference on this line is part of an import or a
    /// re-export. A full statement line (`import ...`, `export { ... }`,
    /// `from x import y`) always counts. A bare list item such as
    /// `formatDate,` counts only inside an open multi-line import block.
    pub(crate) fn is_import_or_export(&mut self, file: &str, line: usize) -> bool {
        let Some(source) = self.line(file, line) else {
            return false;
        };
        if is_import_or_export_statement(&source) {
            return true;
        }
        is_import_list_item(&source) && self.is_inside_import_block(file, line)
    }

    /// Scans up from `line` over list items and nested groups. Returns true
    /// if the first other line opens an import, export, or `use` block that
    /// is still open at `line`.
    fn is_inside_import_block(&mut self, file: &str, line: usize) -> bool {
        // Closing groups minus opening groups on the lines between.
        let mut depth = 0isize;
        for above in (line.saturating_sub(IMPORT_BLOCK_SCAN_LINES)..line).rev() {
            let Some(source) = self.line(file, above) else {
                return false;
            };
            let code = strip_line_comment(&source).trim();
            if code.is_empty() {
                continue;
            }
            if is_import_block_opener(code) {
                return group_balance(code) > depth;
            }
            if !is_import_block_continuation(code) {
                return false;
            }
            depth -= group_balance(code);
        }
        false
    }

    /// Tells if the declaration on `line` is visible outside its file.
    /// `Some(true)` means exported, `Some(false)` means private to the
    /// file or module, and `None` means unknown. The check uses the
    /// visibility rules of `language`.
    pub(crate) fn is_exported_declaration(
        &mut self,
        file: &str,
        line: usize,
        language: SourceLanguage,
        name: &str,
    ) -> Option<bool> {
        match language {
            SourceLanguage::Go => {
                return Some(name.chars().next().is_some_and(char::is_uppercase));
            }
            SourceLanguage::Dart => return Some(!name.starts_with('_')),
            SourceLanguage::Unknown => return None,
            _ => {}
        }
        let source = self.line(file, line)?;
        let trimmed = source.trim_start();
        match language {
            SourceLanguage::JavaScript => Some(self.is_exported_javascript(file, line, trimmed)),
            SourceLanguage::Rust => {
                if trimmed.starts_with("macro_rules!") {
                    return Some(self.has_macro_export_attribute(file, line));
                }
                rust_visibility(trimmed)
            }
            SourceLanguage::Python => {
                Some(!source.starts_with(char::is_whitespace) && !name.starts_with('_'))
            }
            SourceLanguage::Java => java_visibility(trimmed),
            SourceLanguage::Kotlin => default_public_visibility(trimmed, &["private"]),
            SourceLanguage::Scala => default_public_visibility(trimmed, &["private", "protected"]),
            SourceLanguage::Cpp => default_public_visibility(trimmed, &["static"]),
            SourceLanguage::Go | SourceLanguage::Dart | SourceLanguage::Unknown => None,
        }
    }

    /// Scans the attribute and doc-comment lines right above a Rust
    /// `macro_rules!` line for `#[macro_export]`.
    fn has_macro_export_attribute(&mut self, file: &str, line: usize) -> bool {
        for above in (0..line).rev() {
            let Some(source) = self.line(file, above) else {
                return false;
            };
            let trimmed = source.trim();
            if trimmed.starts_with("#[macro_export") {
                return true;
            }
            if !trimmed.starts_with("#[") && !trimmed.starts_with("///") {
                return false;
            }
        }
        false
    }

    fn is_exported_javascript(&mut self, file: &str, line: usize, trimmed: &str) -> bool {
        if let Some(declaration) = trimmed.strip_prefix("export ") {
            return starts_with_declaration_keyword(declaration);
        }
        if !starts_with_declaration_keyword(trimmed) {
            return false;
        }

        (1..=2).any(|offset| {
            line.checked_sub(offset)
                .and_then(|above| self.line(file, above))
                .is_some_and(|source| is_multiline_export_marker(&source))
        })
    }
}

/// The source language of a document. It selects the visibility rules
/// for the `exports_only` filter of `scip_dead`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceLanguage {
    JavaScript,
    Rust,
    Python,
    Go,
    Java,
    Kotlin,
    Scala,
    Cpp,
    Dart,
    Unknown,
}

impl SourceLanguage {
    /// Uses `Document.language` first. Some indexers leave it empty or
    /// write an unknown value, so the file extension is the fallback.
    pub(crate) fn of_document(language: &str, relative_path: &str) -> Self {
        let by_name = match language.trim().to_ascii_lowercase().as_str() {
            "typescript" | "typescriptreact" | "tsx" | "javascript" | "javascriptreact" | "jsx" => {
                Self::JavaScript
            }
            "rust" => Self::Rust,
            "python" => Self::Python,
            "go" | "golang" => Self::Go,
            "java" => Self::Java,
            "kotlin" => Self::Kotlin,
            "scala" => Self::Scala,
            "c" | "cpp" | "c++" | "cuda" | "objectivec" | "objective-c" | "objectivecpp"
            | "objective-cpp" => Self::Cpp,
            "dart" => Self::Dart,
            _ => Self::Unknown,
        };
        if by_name != Self::Unknown {
            return by_name;
        }
        let extension = Path::new(relative_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs") => Self::JavaScript,
            Some("rs") => Self::Rust,
            Some("py" | "pyi") => Self::Python,
            Some("go") => Self::Go,
            Some("java") => Self::Java,
            Some("kt" | "kts") => Self::Kotlin,
            Some("scala" | "sc") => Self::Scala,
            Some("c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" | "cu" | "m" | "mm") => {
                Self::Cpp
            }
            Some("dart") => Self::Dart,
            _ => Self::Unknown,
        }
    }
}

const IMPORT_BLOCK_SCAN_LINES: usize = 200;

/// Rust: `pub` and `pub(...)` items are visible outside the module, except
/// `pub(self)`. An item without `pub` is private.
fn rust_visibility(trimmed: &str) -> Option<bool> {
    if let Some(rest) = trimmed.strip_prefix("pub") {
        if rest.starts_with(char::is_whitespace) {
            return Some(true);
        }
        if let Some(scope) = rest.strip_prefix('(') {
            return Some(!scope.trim_start().starts_with("self"));
        }
    }
    let is_declaration = [
        "fn ", "struct ", "enum ", "trait ", "type ", "const ", "static ", "mod ", "union ",
        "async ", "unsafe ", "extern ",
    ]
    .iter()
    .any(|keyword| trimmed.starts_with(keyword));
    is_declaration.then_some(false)
}

/// Returns the modifier words in front of the declared name. The scan
/// stops at the first word that holds punctuation of a signature.
fn leading_modifiers(trimmed: &str) -> impl Iterator<Item = &str> {
    trimmed
        .split_whitespace()
        .take_while(|word| !word.contains(['(', ')', '{', '=', ':', '<', ';', ',']))
}

/// Java: `public` makes a top-level type visible outside its package.
/// A type without a modifier is package-private. Other files in the
/// package can use it, so the result is unknown rather than hidden.
fn java_visibility(trimmed: &str) -> Option<bool> {
    for word in leading_modifiers(trimmed) {
        match word {
            "public" => return Some(true),
            "private" | "protected" => return Some(false),
            _ => {}
        }
    }
    None
}

/// Kotlin, Scala, and C/C++: a declaration is visible unless one of the
/// `hidden` modifiers marks it private or file-local.
fn default_public_visibility(trimmed: &str, hidden: &[&str]) -> Option<bool> {
    if trimmed.is_empty() {
        return None;
    }
    Some(!leading_modifiers(trimmed).any(|word| hidden.contains(&word)))
}

fn starts_with_declaration_keyword(source: &str) -> bool {
    [
        "function ",
        "const ",
        "let ",
        "var ",
        "class ",
        "interface ",
        "enum ",
        "type ",
        "abstract ",
        "async ",
    ]
    .iter()
    .any(|keyword| source.starts_with(keyword))
}

fn is_multiline_export_marker(source: &str) -> bool {
    let trimmed = source.trim();
    let Some(remainder) = trimmed.strip_prefix("export") else {
        return false;
    };
    if remainder
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return false;
    }
    let remainder = remainder.trim_start();
    remainder.is_empty()
        || (!starts_with_declaration_keyword(remainder)
            && !remainder.starts_with("default ")
            && !remainder.starts_with('{')
            && !remainder.starts_with('*')
            && !remainder.starts_with("type {")
            && !remainder.starts_with("type *"))
}

/// Tells if the line is a full import or re-export statement.
fn is_import_or_export_statement(source: &str) -> bool {
    let trimmed = source.trim_start();
    trimmed.starts_with("import ")
        || trimmed.starts_with("import{")
        || trimmed.starts_with("export {")
        || trimmed.starts_with("export *")
        || trimmed.starts_with("export type {")
        || trimmed.starts_with("export type *")
        || (trimmed.starts_with("from ") && trimmed.contains(" import "))
}

/// Tells if the line opens a multi-line import, re-export, or `use` block.
fn is_import_block_opener(code: &str) -> bool {
    let is_statement = is_import_or_export_statement(code) || {
        let without_visibility = match code.strip_prefix("pub") {
            Some(rest) => match rest.trim_start().strip_prefix('(') {
                Some(scope) => scope.split_once(')').map_or("", |(_, tail)| tail),
                None => rest,
            },
            None => code,
        };
        without_visibility.trim_start().starts_with("use ")
    };
    is_statement && group_balance(code) > 0
}

/// Counts opening `{` and `(` minus closing `}` and `)` outside quotes.
fn group_balance(code: &str) -> isize {
    let mut balance = 0isize;
    let mut quote = None;
    for character in code.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None => match character {
                '\'' | '"' | '`' => quote = Some(character),
                '{' | '(' => balance += 1,
                '}' | ')' => balance -= 1,
                _ => {}
            },
        }
    }
    balance
}

/// Removes a trailing `//` or `#` comment. The function does not parse
/// strings, so it can also cut a `#` inside a string. That only makes the
/// import check more strict.
fn strip_line_comment(source: &str) -> &str {
    let end = [source.find("//"), source.find('#')]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(source.len());
    &source[..end]
}

fn is_identifier(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$'))
}

/// A quoted module path without spaces, such as `"fmt"` in a Go import block.
fn is_quoted_path(word: &str) -> bool {
    word.len() >= 2
        && ((word.starts_with('"') && word.ends_with('"'))
            || (word.starts_with('\'') && word.ends_with('\'')))
        && !word[1..word.len() - 1].contains(['"', '\''])
}

/// Tells if the line has the shape of an entry in an import or export
/// list: comma-separated items `name`, `name as alias`, `type name`, or a
/// quoted module path with an optional alias. A statement such as
/// `return find`, `yield cb`, or `raise MyError` does not match.
fn is_import_list_item(source: &str) -> bool {
    let code = strip_line_comment(source).trim();
    if code.is_empty() {
        return false;
    }
    let mut items = code.split(',').map(str::trim).peekable();
    let mut matched = false;
    while let Some(item) = items.next() {
        if item.is_empty() {
            // Only the last item can be empty: it follows a trailing comma.
            if items.peek().is_some() {
                return false;
            }
            continue;
        }
        let words = item.split_whitespace().collect::<Vec<_>>();
        let is_item = match words.as_slice() {
            [name] => is_identifier(name) || is_quoted_path(name),
            ["type", name] => is_identifier(name),
            [name, "as", alias] | ["type", name, "as", alias] => {
                is_identifier(name) && is_identifier(alias)
            }
            [alias, path] => {
                (is_identifier(alias) || matches!(*alias, "_" | ".")) && is_quoted_path(path)
            }
            _ => false,
        };
        if !is_item {
            return false;
        }
        matched = true;
    }
    matched
}

/// Tells if the scan up to a block opener can pass over the line. The line
/// can be a list item, a nested Rust `use` group, or the closing brace of a
/// nested group. A line with `;`, `(`, `)`, `=`, `.`, or quotes stops the scan.
fn is_import_block_continuation(code: &str) -> bool {
    is_import_list_item(code)
        || code.chars().all(|character| {
            character.is_alphanumeric()
                || character.is_whitespace()
                || matches!(character, '_' | '$' | ',' | ':' | '{' | '}' | '*')
        })
}

pub(crate) fn symbol_details(information: &SymbolInformation) -> String {
    let signature = symbol_signature(information);
    let signature = (!signature.is_empty()).then_some(signature);
    let documentation = information
        .documentation
        .first()
        .map(|documentation| compact_whitespace(documentation))
        .filter(|documentation| !documentation.is_empty());

    let details = match (signature, documentation) {
        (Some(signature), Some(documentation)) => format!("{signature} — {documentation}"),
        (Some(signature), None) => signature,
        (None, Some(documentation)) => documentation,
        (None, None) => String::new(),
    };
    truncate_chars(&details, DOC_CHAR_LIMIT)
}

pub(crate) fn symbol_signature(information: &SymbolInformation) -> String {
    information
        .signature_documentation
        .as_ref()
        .map(|signature| compact_whitespace(&signature.text))
        .filter(|signature| !signature.is_empty())
        .map(|signature| truncate_chars(&signature, DOC_CHAR_LIMIT))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::format_limited;

    fn is_import_or_export_line(source: &str) -> bool {
        is_import_or_export_statement(source) || is_import_list_item(source)
    }

    #[test]
    fn import_export_classifier_only_filters_imports_and_reexports() {
        for source in [
            "import { value } from './module';",
            "import{value} from './module';",
            "export { value } from './module';",
            "export * from './module';",
            "export type { Value } from './module';",
            "export type * from './module';",
            "from package.module import value",
            "formatDate",
            "  formatDate,",
            "  formatDate as fd,",
            "  type Value,",
            "  first, second,",
            "\t\"fmt\"",
        ] {
            assert!(
                is_import_or_export_line(source),
                "expected filter: {source}"
            );
        }

        for source in [
            "export const X = f();",
            "export function f() {}",
            "export class X {}",
            "export let x = 1;",
            "export var x = 1;",
            "export default f(value);",
            "    return find",
            "  return formatDate;",
            "return a, b",
            "yield cb",
            "await promise",
            "throw error",
            "raise MyError",
            "del cache",
            "",
        ] {
            assert!(
                !is_import_or_export_line(source),
                "expected reference line: {source}"
            );
        }
    }

    #[test]
    fn bare_list_items_count_as_imports_only_inside_an_open_block() {
        let project = crate::test_support::TestProject::new();
        let source = [
            "import {",                                 // 0
            "  formatDate,",                            // 1
            "  other as alias, // comment",             // 2
            "} from './date';",                         // 3
            "function factory() {",                     // 4
            "  return formatDate",                      // 5
            "}",                                        // 6
            "helper",                                   // 7
            "from pkg import (",                        // 8
            "    find,",                                // 9
            ")",                                        // 10
            "use crate::{",                             // 11
            "    render::{display_name, SourceCache},", // 12
            "    queries,",                             // 13
            "};",                                       // 14
            "export {",                                 // 15
            "  first,",                                 // 16
            "}",                                        // 17
            "second",                                   // 18
            "pub(crate) use crate::{",                  // 19
            "    nested::{",                            // 20
            "        leaf,",                            // 21
            "    },",                                   // 22
            "    sibling,",                             // 23
            "};",                                       // 24
        ]
        .join("\n");
        fs::write(project.root.join("mixed.txt"), source).expect("write mixed source");
        let mut sources = SourceCache::new(&project.root);

        for line in [0, 1, 2, 8, 9, 13, 16, 21, 23] {
            assert!(
                sources.is_import_or_export("mixed.txt", line),
                "expected import line {line}"
            );
        }
        for line in [3, 4, 5, 6, 7, 10, 12, 18] {
            assert!(
                !sources.is_import_or_export("mixed.txt", line),
                "expected reference line {line}"
            );
        }
    }

    #[test]
    fn source_language_uses_document_language_then_extension() {
        assert_eq!(
            SourceLanguage::of_document("TypeScript", "src/a.rs"),
            SourceLanguage::JavaScript
        );
        assert_eq!(
            SourceLanguage::of_document("rust", "src/a.ts"),
            SourceLanguage::Rust
        );
        assert_eq!(
            SourceLanguage::of_document("", "src/a.tsx"),
            SourceLanguage::JavaScript
        );
        assert_eq!(
            SourceLanguage::of_document("UnknownLanguage", "pkg/a.py"),
            SourceLanguage::Python
        );
        assert_eq!(
            SourceLanguage::of_document("", "pkg/main.go"),
            SourceLanguage::Go
        );
        assert_eq!(
            SourceLanguage::of_document("", "src/a.unknown"),
            SourceLanguage::Unknown
        );
    }

    #[test]
    fn exported_declarations_follow_the_rules_of_each_language() {
        let project = crate::test_support::TestProject::new();
        let files = [
            (
                "lib.rs",
                "pub fn public() {}\nfn private() {}\npub(crate) struct Crate;\npub(self) fn own() {}\n#[macro_export]\nmacro_rules! exported { () => {} }\nmacro_rules! local { () => {} }\nlet x = 1;",
            ),
            ("lib.py", "def public():\n    def inner():\n        pass\nVALUE = 1"),
            ("Main.java", "public final class Main {\nfinal class Hidden {\n@Deprecated public class Old {"),
            ("main.kt", "private fun hidden() {}\nfun shown() {}\ninternal class Internal"),
            ("main.cpp", "static int hidden(int x) {\nint shown(int x) {"),
            ("dead.ts", "export function shown() {}\nfunction hidden() {}"),
        ];
        for (file, source) in files {
            fs::write(project.root.join(file), source).expect("write language source");
        }
        let mut sources = SourceCache::new(&project.root);
        let mut check = |file: &str, line: usize, name: &str| {
            let language = SourceLanguage::of_document("", file);
            sources.is_exported_declaration(file, line, language, name)
        };

        assert_eq!(check("lib.rs", 0, "public"), Some(true));
        assert_eq!(check("lib.rs", 1, "private"), Some(false));
        assert_eq!(check("lib.rs", 2, "Crate"), Some(true));
        assert_eq!(check("lib.rs", 3, "own"), Some(false));
        assert_eq!(check("lib.rs", 5, "exported"), Some(true));
        assert_eq!(check("lib.rs", 6, "local"), Some(false));
        assert_eq!(check("lib.rs", 7, "x"), None);
        assert_eq!(check("lib.py", 0, "public"), Some(true));
        assert_eq!(check("lib.py", 1, "inner"), Some(false));
        assert_eq!(check("lib.py", 3, "VALUE"), Some(true));
        assert_eq!(check("pkg/main.go", 0, "Exported"), Some(true));
        assert_eq!(check("pkg/main.go", 0, "unexported"), Some(false));
        assert_eq!(check("Main.java", 0, "Main"), Some(true));
        assert_eq!(check("Main.java", 1, "Hidden"), None);
        assert_eq!(check("Main.java", 2, "Old"), Some(true));
        assert_eq!(check("main.kt", 0, "hidden"), Some(false));
        assert_eq!(check("main.kt", 1, "shown"), Some(true));
        assert_eq!(check("main.kt", 2, "Internal"), Some(true));
        assert_eq!(check("main.cpp", 0, "hidden"), Some(false));
        assert_eq!(check("main.cpp", 1, "shown"), Some(true));
        assert_eq!(check("lib.dart", 0, "Widget"), Some(true));
        assert_eq!(check("lib.dart", 0, "_State"), Some(false));
        assert_eq!(check("dead.ts", 0, "shown"), Some(true));
        assert_eq!(check("dead.ts", 1, "hidden"), Some(false));
        assert_eq!(check("notes.unknown", 0, "anything"), None);
    }

    #[test]
    fn compact_formatting_uses_one_truncation_line() {
        let lines = vec!["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            format_limited(lines, 2, 0),
            "a\nb\n… 2 more (pass offset=2)"
        );
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }
}
