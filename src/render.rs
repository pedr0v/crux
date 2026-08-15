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

    pub(crate) fn is_import_or_export(&mut self, file: &str, line: usize) -> bool {
        self.line(file, line)
            .is_some_and(|source| is_import_or_export_line(&source))
    }

    pub(crate) fn is_exported_declaration(&mut self, file: &str, line: usize) -> Option<bool> {
        let source = self.line(file, line)?;
        let trimmed = source.trim_start();
        if let Some(declaration) = trimmed.strip_prefix("export ") {
            return Some(starts_with_declaration_keyword(declaration));
        }
        if !starts_with_declaration_keyword(trimmed) {
            return Some(false);
        }

        Some((1..=2).any(|offset| {
            line.checked_sub(offset)
                .and_then(|above| self.line(file, above))
                .is_some_and(|source| is_multiline_export_marker(&source))
        }))
    }
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

fn is_import_or_export_line(source: &str) -> bool {
    let trimmed = source.trim_start();
    if trimmed.starts_with("import ")
        || trimmed.starts_with("import{")
        || trimmed.starts_with("export {")
        || trimmed.starts_with("export *")
        || trimmed.starts_with("export type {")
        || trimmed.starts_with("export type *")
    {
        return true;
    }
    if trimmed.starts_with("export ") {
        return false;
    }

    let trimmed = trimmed.trim_end();
    !trimmed.is_empty()
        && !trimmed.contains('(')
        && trimmed.chars().all(|character| {
            character.is_alphanumeric()
                || character.is_whitespace()
                || matches!(character, '_' | '$' | ',' | '{' | '}' | '\'' | '"')
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

    #[test]
    fn import_export_classifier_only_filters_imports_and_reexports() {
        for source in [
            "import { value } from './module';",
            "import{value} from './module';",
            "export { value } from './module';",
            "export * from './module';",
            "export type { Value } from './module';",
            "export type * from './module';",
            "formatDate",
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
        ] {
            assert!(
                !is_import_or_export_line(source),
                "expected reference line: {source}"
            );
        }
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
