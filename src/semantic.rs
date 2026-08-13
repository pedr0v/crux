use crate::render::{display_name, inferred_kind, qualified_name, SourceCache};
use scip::symbol::{is_local_symbol, parse_symbol};
use scip::types::{
    descriptor, occurrence, symbol_information, Document, Index, Occurrence, SymbolInformation,
    SymbolRole,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

type DefinitionSites = HashMap<String, (String, usize)>;
type ReferenceSites = HashMap<String, BTreeSet<(String, usize)>>;

#[derive(Clone, Copy)]
enum SymbolInformationPosition {
    Document {
        document_index: usize,
        symbol_index: usize,
    },
    External {
        symbol_index: usize,
    },
}

pub(crate) struct SemanticIndex {
    index: Index,
    symbol_information_by_id: HashMap<String, SymbolInformationPosition>,
    callable_definitions: HashMap<String, Vec<CallableDefinition>>,
    definition_sites: DefinitionSites,
    reference_sites_by_symbol: ReferenceSites,
}

impl SemanticIndex {
    pub(crate) fn new(index: Index) -> Self {
        let symbol_information_by_id = symbol_information_positions(&index);
        let callable_definitions = build_callable_definitions(&index, &symbol_information_by_id);
        let (definition_sites, reference_sites_by_symbol) = symbol_sites(&index);

        Self {
            index,
            symbol_information_by_id,
            callable_definitions,
            definition_sites,
            reference_sites_by_symbol,
        }
    }

    pub(crate) fn index(&self) -> &Index {
        &self.index
    }

    pub(crate) fn information(&self, symbol: &str) -> Option<&SymbolInformation> {
        self.symbol_information_by_id
            .get(symbol)
            .map(|position| symbol_information_at(&self.index, *position))
    }

    pub(crate) fn callables(&self) -> &HashMap<String, Vec<CallableDefinition>> {
        &self.callable_definitions
    }

    pub(crate) fn definition_site(&self, symbol: &str) -> Option<&(String, usize)> {
        self.definition_sites.get(symbol)
    }

    pub(crate) fn references_for(&self, symbol: &str) -> Option<&BTreeSet<(String, usize)>> {
        self.reference_sites_by_symbol.get(symbol)
    }

    pub(crate) fn direct_caller_lines(
        &self,
        project_root: &std::path::Path,
        symbols: &HashSet<String>,
    ) -> Vec<String> {
        let mut sources = SourceCache::new(project_root);
        direct_callers(
            &self.index,
            symbols,
            &self.callable_definitions,
            &mut sources,
        )
        .into_iter()
        .map(|caller| caller.render(""))
        .collect()
    }
}

fn symbol_information_positions(index: &Index) -> HashMap<String, SymbolInformationPosition> {
    let mut positions = HashMap::new();
    for (document_index, document) in index.documents.iter().enumerate() {
        for (symbol_index, information) in document.symbols.iter().enumerate() {
            positions.insert(
                information.symbol.clone(),
                SymbolInformationPosition::Document {
                    document_index,
                    symbol_index,
                },
            );
        }
    }
    for (symbol_index, information) in index.external_symbols.iter().enumerate() {
        positions.insert(
            information.symbol.clone(),
            SymbolInformationPosition::External { symbol_index },
        );
    }
    positions
}

fn symbol_information_at(index: &Index, position: SymbolInformationPosition) -> &SymbolInformation {
    match position {
        SymbolInformationPosition::Document {
            document_index,
            symbol_index,
        } => &index.documents[document_index].symbols[symbol_index],
        SymbolInformationPosition::External { symbol_index } => {
            &index.external_symbols[symbol_index]
        }
    }
}

fn symbol_sites(index: &Index) -> (DefinitionSites, ReferenceSites) {
    let mut definition_sites = HashMap::new();
    let mut reference_sites = HashMap::<String, BTreeSet<(String, usize)>>::new();

    for document in &index.documents {
        for occurrence in &document.occurrences {
            let Some(line) = occurrence_start_line(occurrence) else {
                continue;
            };
            let candidate = (document.relative_path.clone(), line);
            if is_definition(occurrence) {
                let site = definition_sites
                    .entry(occurrence.symbol.clone())
                    .or_insert_with(|| candidate.clone());
                if candidate < *site {
                    *site = candidate;
                }
            } else {
                reference_sites
                    .entry(occurrence.symbol.clone())
                    .or_default()
                    .insert(candidate);
            }
        }
    }

    (definition_sites, reference_sites)
}

pub(crate) fn occurrence_start_line(occurrence: &Occurrence) -> Option<usize> {
    if matches!(occurrence.range.len(), 3 | 4) {
        return usize::try_from(occurrence.range[0]).ok();
    }

    match occurrence.typed_range.as_ref()? {
        occurrence::Typed_range::SingleLineRange(range) => usize::try_from(range.line).ok(),
        occurrence::Typed_range::MultiLineRange(range) => usize::try_from(range.start_line).ok(),
        _ => None,
    }
}

pub(crate) fn is_definition(occurrence: &Occurrence) -> bool {
    occurrence.symbol_roles & SymbolRole::Definition as i32 != 0
}

fn is_member_kind(kind: symbol_information::Kind) -> bool {
    matches!(
        kind,
        symbol_information::Kind::AbstractMethod
            | symbol_information::Kind::Accessor
            | symbol_information::Kind::Constructor
            | symbol_information::Kind::EnumMember
            | symbol_information::Kind::Field
            | symbol_information::Kind::Getter
            | symbol_information::Kind::Method
            | symbol_information::Kind::MethodAlias
            | symbol_information::Kind::MethodSpecification
            | symbol_information::Kind::Property
            | symbol_information::Kind::ProtocolMethod
            | symbol_information::Kind::PureVirtualMethod
            | symbol_information::Kind::Setter
            | symbol_information::Kind::SingletonMethod
            | symbol_information::Kind::StaticDataMember
            | symbol_information::Kind::StaticEvent
            | symbol_information::Kind::StaticField
            | symbol_information::Kind::StaticMethod
            | symbol_information::Kind::StaticProperty
            | symbol_information::Kind::TraitMethod
            | symbol_information::Kind::TypeClassMethod
    )
}

pub(crate) fn symbol_container(information: &SymbolInformation) -> Option<String> {
    let symbol = parse_symbol(&information.symbol).ok()?;
    let descriptors = &symbol.descriptors;
    if descriptors.len() < 2 {
        return None;
    }

    let last_suffix = descriptors.last()?.suffix.enum_value().ok();
    let kind = information.kind.enum_value().ok();
    if last_suffix != Some(descriptor::Suffix::Method) && !kind.is_some_and(is_member_kind) {
        return None;
    }

    let container_descriptor = descriptors.get(descriptors.len() - 2)?;
    if matches!(
        container_descriptor.suffix.enum_value().ok(),
        Some(descriptor::Suffix::Namespace | descriptor::Suffix::Package)
    ) {
        return None;
    }
    let container = container_descriptor.name.trim();
    (!container.is_empty()).then(|| container.to_string())
}

pub(crate) fn definition_line(document: &Document, symbol: &str) -> Option<usize> {
    document
        .occurrences
        .iter()
        .find(|occurrence| occurrence.symbol == symbol && is_definition(occurrence))
        .or_else(|| {
            document
                .occurrences
                .iter()
                .find(|occurrence| occurrence.symbol == symbol)
        })
        .and_then(occurrence_start_line)
}

pub(crate) struct MatchingSymbols {
    pub(crate) principals: HashSet<String>,
    references_by_principal: HashMap<String, HashSet<String>>,
}

impl MatchingSymbols {
    pub(crate) fn is_empty(&self) -> bool {
        self.principals.is_empty()
    }

    pub(crate) fn reference_symbols(&self, principal: &str) -> HashSet<String> {
        self.references_by_principal
            .get(principal)
            .cloned()
            .unwrap_or_else(|| HashSet::from([principal.to_string()]))
    }

    pub(crate) fn all_reference_symbols(&self) -> HashSet<&str> {
        self.references_by_principal
            .values()
            .flat_map(|symbols| symbols.iter().map(String::as_str))
            .collect()
    }
}

fn symbol_information_richness(information: &SymbolInformation) -> (bool, bool) {
    let has_kind = information
        .kind
        .enum_value()
        .is_ok_and(|kind| kind != symbol_information::Kind::UnspecifiedKind);
    let has_documentation = information
        .documentation
        .iter()
        .any(|documentation| !documentation.trim().is_empty())
        || information
            .signature_documentation
            .as_ref()
            .is_some_and(|signature| !signature.text.trim().is_empty());
    (has_kind, has_documentation)
}

pub(crate) fn matching_symbol_ids(index: &SemanticIndex, name: &str) -> MatchingSymbols {
    let all = index
        .index()
        .documents
        .iter()
        .flat_map(|document| document.symbols.iter())
        .chain(index.index().external_symbols.iter());
    let name_lower = name.to_lowercase();
    let symbols = all.collect::<Vec<_>>();
    let mut matches = symbols
        .iter()
        .filter(|information| {
            display_name(information) == name
                || qualified_name(information) == name
                || information.symbol == name
        })
        .map(|information| information.symbol.clone())
        .collect::<HashSet<_>>();
    let exact_match = !matches.is_empty();

    if matches.is_empty() {
        matches = symbols
            .iter()
            .filter(|information| {
                display_name(information)
                    .to_lowercase()
                    .contains(&name_lower)
            })
            .map(|information| information.symbol.clone())
            .collect();
    }

    let mut principals = matches.clone();
    let mut folded_into = HashMap::new();

    if exact_match {
        let mut symbols_by_site: BTreeMap<(String, usize), Vec<String>> = BTreeMap::new();
        for symbol in &matches {
            if let Some(site) = index.definition_site(symbol) {
                symbols_by_site
                    .entry(site.clone())
                    .or_default()
                    .push(symbol.clone());
            }
        }

        for mut colocated in symbols_by_site.into_values() {
            if colocated.len() < 2 {
                continue;
            }
            colocated.sort_by(|left, right| {
                let left_richness = index
                    .information(left)
                    .map(symbol_information_richness)
                    .unwrap_or_default();
                let right_richness = index
                    .information(right)
                    .map(symbol_information_richness)
                    .unwrap_or_default();
                right_richness
                    .cmp(&left_richness)
                    .then_with(|| left.cmp(right))
            });
            let principal = colocated[0].clone();
            for duplicate in colocated.into_iter().skip(1) {
                principals.remove(&duplicate);
                folded_into.insert(duplicate, principal.clone());
            }
        }
    }

    let mut symbols_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for symbol in &matches {
        if let Some(information) = index.information(symbol) {
            symbols_by_name
                .entry(display_name(information))
                .or_default()
                .push(symbol.clone());
        }
    }
    for same_name in symbols_by_name.into_values() {
        let defined_principals = same_name
            .iter()
            .filter(|symbol| {
                principals.contains(*symbol) && index.definition_site(symbol).is_some()
            })
            .collect::<Vec<_>>();
        if defined_principals.len() != 1 {
            continue;
        }
        let principal = defined_principals[0].clone();
        for alias in same_name
            .into_iter()
            .filter(|symbol| index.definition_site(symbol).is_none())
        {
            principals.remove(&alias);
            folded_into.insert(alias, principal.clone());
        }
    }

    let mut references_by_principal = principals
        .iter()
        .map(|symbol| (symbol.clone(), HashSet::from([symbol.clone()])))
        .collect::<HashMap<_, _>>();
    for symbol in &matches {
        if let Some(principal) = folded_into.get(symbol) {
            references_by_principal
                .entry(principal.clone())
                .or_default()
                .insert(symbol.clone());
        }
    }

    MatchingSymbols {
        principals,
        references_by_principal,
    }
}

fn occurrence_enclosing_lines(occurrence: &Occurrence) -> Option<(usize, usize)> {
    if let Some(range) = occurrence.typed_enclosing_range.as_ref() {
        let lines = match range {
            occurrence::Typed_enclosing_range::SingleLineEnclosingRange(range) => {
                let line = usize::try_from(range.line).ok()?;
                (line, line)
            }
            occurrence::Typed_enclosing_range::MultiLineEnclosingRange(range) => (
                usize::try_from(range.start_line).ok()?,
                usize::try_from(range.end_line).ok()?,
            ),
            _ => return None,
        };
        return (lines.0 <= lines.1).then_some(lines);
    }

    let lines = match occurrence.enclosing_range.as_slice() {
        [line, _, _] => {
            let line = usize::try_from(*line).ok()?;
            (line, line)
        }
        [start_line, _, end_line, _] => (
            usize::try_from(*start_line).ok()?,
            usize::try_from(*end_line).ok()?,
        ),
        _ => return None,
    };
    (lines.0 <= lines.1).then_some(lines)
}

fn is_callable(information: &SymbolInformation) -> bool {
    let callable_kind = information.kind.enum_value().is_ok_and(|kind| {
        matches!(
            kind,
            symbol_information::Kind::AbstractMethod
                | symbol_information::Kind::Accessor
                | symbol_information::Kind::Constructor
                | symbol_information::Kind::Function
                | symbol_information::Kind::Getter
                | symbol_information::Kind::Method
                | symbol_information::Kind::MethodAlias
                | symbol_information::Kind::MethodSpecification
                | symbol_information::Kind::ProtocolMethod
                | symbol_information::Kind::PureVirtualMethod
                | symbol_information::Kind::Setter
                | symbol_information::Kind::SingletonMethod
                | symbol_information::Kind::StaticMethod
                | symbol_information::Kind::TraitMethod
                | symbol_information::Kind::TypeClassMethod
        )
    });
    callable_kind
        || parse_symbol(&information.symbol)
            .ok()
            .and_then(|symbol| symbol.descriptors.last().cloned())
            .and_then(|descriptor| descriptor.suffix.enum_value().ok())
            == Some(descriptor::Suffix::Method)
}

fn has_parameter_descriptor(information: &SymbolInformation) -> bool {
    parse_symbol(&information.symbol)
        .ok()
        .and_then(|symbol| symbol.descriptors.last().cloned())
        .and_then(|descriptor| descriptor.suffix.enum_value().ok())
        .is_some_and(|suffix| {
            matches!(
                suffix,
                descriptor::Suffix::Parameter | descriptor::Suffix::TypeParameter
            )
        })
}

#[derive(Clone)]
pub(crate) struct CallableDefinition {
    symbol: String,
    kind: String,
    name: String,
    file: String,
    line: usize,
    enclosing_lines: Option<(usize, usize)>,
}

fn build_callable_definitions(
    index: &Index,
    information_positions: &HashMap<String, SymbolInformationPosition>,
) -> HashMap<String, Vec<CallableDefinition>> {
    let mut definitions: HashMap<String, Vec<CallableDefinition>> = HashMap::new();

    for document in &index.documents {
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) {
                continue;
            }
            let (Some(line), Some(information)) = (
                occurrence_start_line(occurrence),
                information_positions
                    .get(occurrence.symbol.as_str())
                    .map(|position| symbol_information_at(index, *position)),
            ) else {
                continue;
            };
            if has_parameter_descriptor(information) {
                continue;
            }
            let enclosing_lines = occurrence_enclosing_lines(occurrence);
            let has_multiline_enclosing_range =
                enclosing_lines.is_some_and(|(start, end)| start < end);
            if !is_callable(information) && !has_multiline_enclosing_range {
                continue;
            }
            definitions
                .entry(document.relative_path.clone())
                .or_default()
                .push(CallableDefinition {
                    symbol: occurrence.symbol.clone(),
                    kind: inferred_kind(information),
                    name: display_name(information),
                    file: document.relative_path.clone(),
                    line,
                    enclosing_lines,
                });
        }
    }

    for definitions in definitions.values_mut() {
        definitions
            .sort_by(|left, right| (&left.line, &left.symbol).cmp(&(&right.line, &right.symbol)));
    }
    definitions
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CallerIdentity {
    Symbol(String),
    Module(String),
}

#[derive(Clone)]
struct Caller {
    identity: CallerIdentity,
    kind: String,
    name: String,
    file: String,
    line: Option<usize>,
    count: usize,
}

impl Caller {
    fn from_definition(definition: &CallableDefinition) -> Self {
        Self {
            identity: CallerIdentity::Symbol(definition.symbol.clone()),
            kind: definition.kind.clone(),
            name: definition.name.clone(),
            file: definition.file.clone(),
            line: Some(definition.line),
            count: 0,
        }
    }

    fn module(file: &str) -> Self {
        Self {
            identity: CallerIdentity::Module(file.to_string()),
            kind: String::new(),
            name: String::new(),
            file: file.to_string(),
            line: None,
            count: 0,
        }
    }

    fn render(&self, indent: &str) -> String {
        match self.line {
            Some(line) => format!(
                "{indent}{} {} {}:{} (x{})",
                self.kind,
                self.name,
                self.file,
                line + 1,
                self.count
            ),
            None => format!("{indent}<module> {} (x{})", self.file, self.count),
        }
    }
}

fn resolve_caller(definitions: &[CallableDefinition], file: &str, reference_line: usize) -> Caller {
    let enclosing = definitions
        .iter()
        .filter(|definition| {
            definition
                .enclosing_lines
                .is_some_and(|(start, end)| start <= reference_line && reference_line <= end)
        })
        .min_by(|left, right| {
            let (left_start, left_end) = left.enclosing_lines.expect("filtered enclosing range");
            let (right_start, right_end) = right.enclosing_lines.expect("filtered enclosing range");
            (left_end - left_start)
                .cmp(&(right_end - right_start))
                .then_with(|| right_start.cmp(&left_start))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.symbol.cmp(&right.symbol))
        });
    if let Some(definition) = enclosing {
        return Caller::from_definition(definition);
    }

    definitions
        .iter()
        .filter(|definition| {
            definition.enclosing_lines.is_none() && definition.line <= reference_line
        })
        .max_by(|left, right| {
            left.line
                .cmp(&right.line)
                .then_with(|| right.symbol.cmp(&left.symbol))
        })
        .map(Caller::from_definition)
        .unwrap_or_else(|| Caller::module(file))
}

fn direct_callers(
    index: &Index,
    symbols: &HashSet<String>,
    definitions: &HashMap<String, Vec<CallableDefinition>>,
    sources: &mut SourceCache<'_>,
) -> Vec<Caller> {
    let mut callers: HashMap<CallerIdentity, Caller> = HashMap::new();

    for document in &index.documents {
        let document_definitions = definitions
            .get(&document.relative_path)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for occurrence in &document.occurrences {
            if is_definition(occurrence) || !symbols.contains(&occurrence.symbol) {
                continue;
            }
            let Some(line) = occurrence_start_line(occurrence) else {
                continue;
            };
            if sources.is_import_or_export(&document.relative_path, line) {
                continue;
            }
            let caller = resolve_caller(document_definitions, &document.relative_path, line);
            callers
                .entry(caller.identity.clone())
                .and_modify(|existing| existing.count += 1)
                .or_insert_with(|| Caller { count: 1, ..caller });
        }
    }

    let mut callers = callers.into_values().collect::<Vec<_>>();
    callers.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.name.cmp(&right.name))
    });
    callers
}

pub(crate) struct CallerWalk<'a> {
    pub(crate) index: &'a Index,
    pub(crate) definitions: &'a HashMap<String, Vec<CallableDefinition>>,
    pub(crate) sources: SourceCache<'a>,
    pub(crate) limit: usize,
    pub(crate) seen_symbols: HashSet<String>,
    pub(crate) lines: Vec<String>,
}

impl CallerWalk<'_> {
    pub(crate) fn render_symbols(
        &mut self,
        symbols: &HashSet<String>,
        remaining_depth: usize,
        level: usize,
    ) {
        let callers = direct_callers(self.index, symbols, self.definitions, &mut self.sources);
        let total = callers.len();
        let indent = "  ".repeat(level);

        for caller in callers.into_iter().take(self.limit) {
            let should_render = match &caller.identity {
                CallerIdentity::Symbol(symbol) => self.seen_symbols.insert(symbol.clone()),
                CallerIdentity::Module(_) => true,
            };
            if !should_render {
                continue;
            }
            self.lines.push(caller.render(&indent));
            if remaining_depth > 1 {
                if let CallerIdentity::Symbol(symbol) = &caller.identity {
                    self.render_symbols(
                        &HashSet::from([symbol.clone()]),
                        remaining_depth - 1,
                        level + 1,
                    );
                }
            }
        }

        if total > self.limit {
            self.lines
                .push(format!("{indent}… +{} more", total - self.limit));
        }
    }
}

pub(crate) fn is_dead_candidate(information: &SymbolInformation) -> bool {
    if is_local_symbol(&information.symbol) {
        return false;
    }
    let suffix = parse_symbol(&information.symbol)
        .ok()
        .and_then(|symbol| symbol.descriptors.last().cloned())
        .and_then(|descriptor| descriptor.suffix.enum_value().ok());
    if matches!(
        suffix,
        Some(
            descriptor::Suffix::Local
                | descriptor::Suffix::Meta
                | descriptor::Suffix::Parameter
                | descriptor::Suffix::TypeParameter
        )
    ) {
        return false;
    }

    match information.kind.enum_value() {
        Ok(symbol_information::Kind::UnspecifiedKind) | Err(_) => {
            matches!(
                suffix,
                Some(
                    descriptor::Suffix::Method
                        | descriptor::Suffix::Term
                        | descriptor::Suffix::Type
                )
            )
        }
        Ok(kind) => {
            matches!(
                kind,
                symbol_information::Kind::AbstractMethod
                    | symbol_information::Kind::Accessor
                    | symbol_information::Kind::Class
                    | symbol_information::Kind::Constant
                    | symbol_information::Kind::Constructor
                    | symbol_information::Kind::Enum
                    | symbol_information::Kind::Field
                    | symbol_information::Kind::Function
                    | symbol_information::Kind::Getter
                    | symbol_information::Kind::Interface
                    | symbol_information::Kind::Method
                    | symbol_information::Kind::MethodAlias
                    | symbol_information::Kind::MethodSpecification
                    | symbol_information::Kind::Property
                    | symbol_information::Kind::ProtocolMethod
                    | symbol_information::Kind::PureVirtualMethod
                    | symbol_information::Kind::Setter
                    | symbol_information::Kind::SingletonMethod
                    | symbol_information::Kind::StaticDataMember
                    | symbol_information::Kind::StaticField
                    | symbol_information::Kind::StaticMethod
                    | symbol_information::Kind::StaticProperty
                    | symbol_information::Kind::StaticVariable
                    | symbol_information::Kind::TraitMethod
                    | symbol_information::Kind::Type
                    | symbol_information::Kind::TypeAlias
                    | symbol_information::Kind::TypeClassMethod
                    | symbol_information::Kind::Value
                    | symbol_information::Kind::Variable
            )
        }
    }
}

pub(crate) fn has_member_descriptor_shape(information: &SymbolInformation) -> bool {
    let Ok(symbol) = parse_symbol(&information.symbol) else {
        return false;
    };
    let descriptors = &symbol.descriptors;
    if descriptors.len() < 2 {
        return false;
    }
    let last = descriptors
        .last()
        .and_then(|descriptor| descriptor.suffix.enum_value().ok());
    let container = descriptors
        .get(descriptors.len() - 2)
        .and_then(|descriptor| descriptor.suffix.enum_value().ok());
    matches!(
        last,
        Some(descriptor::Suffix::Method | descriptor::Suffix::Term | descriptor::Suffix::Type)
    ) && matches!(
        container,
        Some(descriptor::Suffix::Type | descriptor::Suffix::Term)
    )
}

pub(crate) fn is_default_or_anonymous(name: &str) -> bool {
    let normalized = name
        .trim()
        .trim_matches(|character| matches!(character, '<' | '>' | '(' | ')'))
        .to_ascii_lowercase();
    normalized.is_empty()
        || normalized == "default"
        || normalized == "anonymous"
        || normalized.starts_with("default export")
        || normalized.starts_with("anonymous ")
}

pub(crate) struct DeadCandidate {
    pub(crate) symbol: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) file: String,
    pub(crate) line: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn exact_matches_at_one_definition_site_keep_the_richer_symbol() {
        let mut index = fixture(false);
        let date_document = index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/lib/date.ts")
            .expect("date document");
        date_document.symbols.push(SymbolInformation {
            symbol: DUPLICATE_DATE_SYMBOL.to_string(),
            display_name: "formatDate".to_string(),
            ..Default::default()
        });
        date_document
            .occurrences
            .push(legacy_occurrence(DUPLICATE_DATE_SYMBOL, 41, true));

        let index = SemanticIndex::new(index);
        let matches = matching_symbol_ids(&index, "formatDate");
        assert_eq!(matches.principals, HashSet::from([DATE_SYMBOL.to_string()]));
        assert_eq!(
            matches.reference_symbols(DATE_SYMBOL),
            HashSet::from([DATE_SYMBOL.to_string(), DUPLICATE_DATE_SYMBOL.to_string()])
        );
    }
}
