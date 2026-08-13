use crate::render::{
    display_name, inferred_kind, qualified_name, symbol_details, symbol_label, symbol_signature,
    SourceCache,
};
use crate::semantic::{
    definition_line, has_member_descriptor_shape, CallerWalk, DeadCandidate, MatchingSymbols,
};
use crate::semantic::{
    is_dead_candidate, is_default_or_anonymous, is_definition, matching_symbol_ids,
    occurrence_start_line, SemanticIndex,
};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

const MAX_RESULT_LINES: usize = 39;
pub(crate) const MAX_MAP_NAMES: usize = 8;
const MAX_MAP_RESULT_LINES: usize = 250;

struct SymbolCandidate {
    rank: usize,
    qualified_name: String,
    kind: String,
    file: String,
    line: usize,
    signature: String,
    symbol: String,
}

impl SymbolCandidate {
    fn render(&self) -> String {
        let signature = if self.signature.is_empty() {
            "-"
        } else {
            &self.signature
        };
        format!(
            "{} | {} | {}:{} | {signature}",
            self.qualified_name,
            self.kind,
            self.file,
            self.line + 1
        )
    }
}

fn symbol_candidates(
    index: &SemanticIndex,
    name: &str,
    unreferenced: bool,
) -> Result<Vec<SymbolCandidate>> {
    let name = name.trim();
    if name.is_empty() {
        bail!("name must not be empty");
    }
    if name == "*" && !unreferenced {
        bail!("'*' requires unreferenced=true");
    }

    let name_lower = name.to_lowercase();
    let information = index
        .index()
        .documents
        .iter()
        .flat_map(|document| document.symbols.iter())
        .chain(index.index().external_symbols.iter());
    let mut candidates = Vec::new();
    for information in information {
        let Some((file, line)) = index.definition_site(&information.symbol).cloned() else {
            continue;
        };
        let display = display_name(information);
        let qualified = qualified_name(information);
        let display_lower = display.to_lowercase();
        let qualified_lower = qualified.to_lowercase();
        let rank = if name == "*" || display_lower == name_lower || qualified_lower == name_lower {
            0
        } else if display_lower.starts_with(&name_lower) || qualified_lower.starts_with(&name_lower)
        {
            1
        } else if display_lower.contains(&name_lower) || qualified_lower.contains(&name_lower) {
            2
        } else {
            continue;
        };
        if unreferenced {
            let family = matching_symbol_ids(index, &display);
            let has_inbound_references =
                family
                    .reference_symbols(&information.symbol)
                    .iter()
                    .any(|symbol| {
                        index
                            .references_for(symbol)
                            .is_some_and(|references| !references.is_empty())
                    });
            if !is_dead_candidate(information)
                || display.starts_with('_')
                || is_default_or_anonymous(&display)
                || has_inbound_references
            {
                continue;
            }
        }
        candidates.push(SymbolCandidate {
            rank,
            qualified_name: qualified,
            kind: inferred_kind(information),
            file,
            line,
            signature: symbol_signature(information),
            symbol: information.symbol.clone(),
        });
    }

    candidates.sort_by(|left, right| {
        (
            &left.rank,
            &left.qualified_name,
            &left.file,
            &left.line,
            &left.symbol,
        )
            .cmp(&(
                &right.rank,
                &right.qualified_name,
                &right.file,
                &right.line,
                &right.symbol,
            ))
    });
    candidates.dedup_by(|left, right| left.symbol == right.symbol);
    Ok(candidates)
}

pub(crate) fn find(
    index: &SemanticIndex,
    name: &str,
    limit: usize,
    unreferenced: bool,
) -> Result<String> {
    let candidates = symbol_candidates(index, name, unreferenced)?;
    let total = candidates.len();
    if total == 0 {
        return Ok("no matches".to_string());
    }

    let mut lines = candidates
        .into_iter()
        .take(limit)
        .map(|candidate| candidate.render())
        .collect::<Vec<_>>();
    if total > limit {
        lines.push(format!("… {} more; raise limit", total - limit));
    }
    Ok(lines.join("\n"))
}

fn ambiguity_lines(
    index: &SemanticIndex,
    name: &str,
    symbols: &MatchingSymbols,
) -> Option<Vec<String>> {
    let mut candidates = symbols
        .principals
        .iter()
        .filter_map(|symbol| {
            let information = index.information(symbol)?;
            let (file, _) = index.definition_site(symbol)?;
            Some((
                qualified_name(information),
                inferred_kind(information),
                file.clone(),
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    if candidates.len() < 2 {
        return None;
    }

    let mut lines = vec![format!("ambiguous: {name}")];
    lines.extend(
        candidates
            .into_iter()
            .map(|(qualified, kind, file)| format!("- {qualified} | {kind} | {file}")),
    );
    Some(lines)
}

fn ambiguity_response(
    index: &SemanticIndex,
    name: &str,
    symbols: &MatchingSymbols,
) -> Option<String> {
    let mut lines = ambiguity_lines(index, name, symbols)?;
    lines.push("specify the qualified name".to_string());
    Some(lines.join("\n"))
}

pub(crate) fn callers(
    project_root: &Path,
    index: &SemanticIndex,
    name: &str,
    depth: usize,
    limit: usize,
) -> Result<String> {
    if name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let symbols = matching_symbol_ids(index, name);
    if symbols.is_empty() {
        return Ok("no matches".to_string());
    }
    if let Some(response) = ambiguity_response(index, name, &symbols) {
        return Ok(response);
    }

    let mut matches = symbols
        .principals
        .iter()
        .filter_map(|symbol| {
            let information = index.information(symbol)?;
            let (file, line) = index.definition_site(symbol)?.clone();
            Some((file, line, symbol.clone(), information))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    if matches.is_empty() {
        return Ok("no matches".to_string());
    }

    let mut lines = Vec::new();
    for (file, line, symbol, information) in matches {
        lines.push(format!(
            "## callers of {} {} {file}:{}",
            inferred_kind(information),
            display_name(information),
            line + 1
        ));
        let before = lines.len();
        let mut walk = CallerWalk {
            index: index.index(),
            definitions: index.callables(),
            sources: SourceCache::new(project_root),
            limit,
            seen_symbols: HashSet::new(),
            lines: Vec::new(),
        };
        walk.render_symbols(&symbols.reference_symbols(&symbol), depth, 0);
        lines.append(&mut walk.lines);
        if lines.len() == before {
            lines.push("no callers".to_string());
        }
    }

    if lines.len() > MAX_MAP_RESULT_LINES {
        let shown = MAX_MAP_RESULT_LINES - 1;
        let omitted = lines.len() - shown;
        lines.truncate(shown);
        lines.push(format!("… +{omitted} more lines"));
    }
    Ok(lines.join("\n"))
}

pub(crate) fn dead_exports(
    project_root: &Path,
    index: &SemanticIndex,
    path_prefix: Option<&str>,
    limit: usize,
    exports_only: bool,
) -> Result<String> {
    let path_prefix = path_prefix
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| {
            if Path::new(prefix).is_absolute() {
                bail!("path_prefix must be relative to project_root");
            }
            if prefix
                .replace('\\', "/")
                .split('/')
                .any(|component| component == "..")
            {
                bail!("path_prefix must not contain '..'");
            }
            Ok(prefix.trim_start_matches("./").replace('\\', "/"))
        })
        .transpose()?;
    let mut sources = SourceCache::new(project_root);
    let mut candidates = Vec::new();

    for document in &index.index().documents {
        if path_prefix
            .as_ref()
            .is_some_and(|prefix| !document.relative_path.starts_with(prefix))
        {
            continue;
        }
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) {
                continue;
            }
            let (Some(line), Some(information)) = (
                occurrence_start_line(occurrence),
                index.information(&occurrence.symbol),
            ) else {
                continue;
            };
            let name = display_name(information);
            if !is_dead_candidate(information)
                || name.starts_with('_')
                || is_default_or_anonymous(&name)
                || sources
                    .line(&document.relative_path, line)
                    .is_some_and(|source| source.trim_start().starts_with("export default "))
            {
                continue;
            }
            if exports_only
                && (has_member_descriptor_shape(information)
                    || sources
                        .is_exported_declaration(&document.relative_path, line)
                        .is_some_and(|exported| !exported))
            {
                continue;
            }
            candidates.push(DeadCandidate {
                symbol: occurrence.symbol.clone(),
                kind: inferred_kind(information),
                name,
                file: document.relative_path.clone(),
                line,
            });
        }
    }
    candidates.sort_by(|left, right| {
        (&left.file, &left.line, &left.symbol).cmp(&(&right.file, &right.line, &right.symbol))
    });
    let mut seen_symbols = HashSet::new();
    candidates.retain(|candidate| seen_symbols.insert(candidate.symbol.clone()));

    let mut reference_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for document in &index.index().documents {
        for occurrence in &document.occurrences {
            if is_definition(occurrence) {
                continue;
            }
            *reference_counts
                .entry(occurrence.symbol.clone())
                .or_default()
                .entry(document.relative_path.clone())
                .or_default() += 1;
        }
    }

    let mut entries = Vec::new();
    for candidate in candidates {
        let counts = reference_counts.get(&candidate.symbol);
        let outside_file = counts
            .into_iter()
            .flat_map(|counts| counts.iter())
            .filter(|(file, _)| *file != &candidate.file)
            .map(|(_, count)| *count)
            .sum::<usize>();
        if outside_file > 0 {
            continue;
        }
        let in_file = counts
            .and_then(|counts| counts.get(&candidate.file))
            .copied()
            .unwrap_or_default();
        entries.push(if in_file == 0 {
            format!(
                "dead: {} {} {}:{}",
                candidate.kind,
                candidate.name,
                candidate.file,
                candidate.line + 1
            )
        } else {
            format!(
                "file-local: {} {} {}:{} (x{in_file} in-file)",
                candidate.kind,
                candidate.name,
                candidate.file,
                candidate.line + 1
            )
        });
    }

    let total = entries.len();
    let mut lines = vec![format!(
        "# dead exports (exports_only={exports_only}) — dynamic/string-based uses aren't visible to SCIP"
    )];
    lines.extend(entries.into_iter().take(limit));
    if total == 0 {
        lines.push("no matches".to_string());
    } else if total > limit {
        lines.push(format!("… +{} more", total - limit));
    }
    Ok(lines.join("\n"))
}

pub(crate) fn format_limited(lines: Vec<String>, limit: usize) -> String {
    let total = lines.len();
    if total == 0 {
        return "no matches".to_string();
    }
    let shown = total.min(limit);
    let mut output = lines.into_iter().take(shown).collect::<Vec<_>>();
    if shown < total {
        output.push(format!("… +{} more", total - shown));
    }
    output.join("\n")
}

pub(crate) fn search(index: &SemanticIndex, query: &str, limit: usize) -> Result<String> {
    if query.trim().is_empty() {
        bail!("query must not be empty");
    }
    let query_lower = query.to_lowercase();
    let mut hits = Vec::new();

    for document in &index.index().documents {
        for information in &document.symbols {
            let name = display_name(information);
            let name_lower = name.to_lowercase();
            let rank = if name_lower == query_lower {
                0
            } else if name_lower.starts_with(&query_lower) {
                1
            } else if name_lower.contains(&query_lower) {
                2
            } else {
                continue;
            };
            let Some(line) = definition_line(document, &information.symbol) else {
                continue;
            };
            hits.push((
                rank,
                name_lower,
                document.relative_path.clone(),
                line,
                format!(
                    "{} {}:{}",
                    symbol_label(information),
                    document.relative_path,
                    line + 1
                ),
            ));
        }
    }

    hits.sort_by(|left, right| {
        (&left.0, &left.1, &left.2, &left.3).cmp(&(&right.0, &right.1, &right.2, &right.3))
    });
    Ok(format_limited(
        hits.into_iter().map(|hit| hit.4).collect(),
        limit,
    ))
}

pub(crate) fn definitions(
    project_root: &Path,
    index: &SemanticIndex,
    name: &str,
) -> Result<String> {
    if name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let symbols = matching_symbol_ids(index, name);
    if symbols.is_empty() {
        return Ok("no matches".to_string());
    }
    if let Some(response) = ambiguity_response(index, name, &symbols) {
        return Ok(response);
    }
    let mut definitions = Vec::new();

    for document in &index.index().documents {
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) || !symbols.principals.contains(&occurrence.symbol) {
                continue;
            }
            let Some(line) = occurrence_start_line(occurrence) else {
                continue;
            };
            definitions.push((
                document.relative_path.clone(),
                line,
                occurrence.symbol.clone(),
            ));
        }
    }

    definitions.sort();
    definitions.dedup();
    let mut sources = SourceCache::new(project_root);
    let mut lines = Vec::new();
    for (file, line, symbol) in definitions {
        let Some(information) = index.information(&symbol) else {
            continue;
        };
        let location = format!("{} {file}:{}", symbol_label(information), line + 1);
        let details = symbol_details(information);
        lines.push(if details.is_empty() {
            location
        } else {
            format!("{location} {details}")
        });
        if let Some(source) = sources.display_line(&file, line) {
            lines.push(format!("def> {source}"));
        }
    }
    if lines.is_empty() {
        Ok("no matches".to_string())
    } else {
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
pub(crate) fn map_bundle(
    project_root: &Path,
    index: &SemanticIndex,
    names: &[String],
    context: bool,
    refs_limit: usize,
    include_imports: bool,
) -> Result<String> {
    if !(1..=MAX_MAP_NAMES).contains(&names.len()) {
        bail!("names must contain between 1 and {MAX_MAP_NAMES} entries");
    }
    if names.iter().any(|name| name.trim().is_empty()) {
        bail!("names must not contain empty entries");
    }

    let matches_by_name = names
        .iter()
        .map(|name| (name, matching_symbol_ids(index, name)))
        .collect::<Vec<_>>();
    let mut ambiguities = matches_by_name
        .iter()
        .filter_map(|(name, symbols)| ambiguity_lines(index, name, symbols))
        .flatten()
        .collect::<Vec<_>>();
    if !ambiguities.is_empty() {
        ambiguities.push("specify the qualified name".to_string());
        return Ok(ambiguities.join("\n"));
    }

    let mut sources = SourceCache::new(project_root);
    let mut lines = Vec::new();

    for (name, symbols) in matches_by_name {
        let mut matches = symbols
            .principals
            .iter()
            .filter_map(|symbol| {
                let information = index.information(symbol)?;
                let (file, line) = index.definition_site(symbol)?.clone();
                Some((file, line, symbol.clone(), information))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2))
        });

        if matches.is_empty() {
            lines.push(format!("## no matches: {name}"));
            continue;
        }

        for (file, line, symbol, information) in matches {
            let references = symbols
                .reference_symbols(&symbol)
                .into_iter()
                .filter_map(|reference_symbol| index.references_for(&reference_symbol))
                .flat_map(|sites| sites.iter().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|(reference_file, reference_line)| {
                    include_imports || !sources.is_import_or_export(reference_file, *reference_line)
                })
                .collect::<Vec<_>>();
            let total_references = references.len();
            let shown_references = references.into_iter().take(refs_limit).collect::<Vec<_>>();

            lines.push(format!(
                "## {} {file}:{}",
                symbol_label(information),
                line + 1
            ));
            let details = symbol_details(information);
            if !details.is_empty() {
                lines.push(details);
            }
            if context {
                if let Some(source) = sources.display_line(&file, line) {
                    lines.push(format!("def> {source}"));
                }
            }
            if total_references > refs_limit {
                lines.push(format!("reference sites: {total_references}"));
            }

            if context {
                for (reference_file, reference_line) in shown_references {
                    let prefix = format!("{reference_file}:{}:", reference_line + 1);
                    match sources.display_line(&reference_file, reference_line) {
                        Some(source) => lines.push(format!("{prefix} {source}")),
                        None => lines.push(prefix),
                    }
                }
            } else {
                let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                for (reference_file, reference_line) in shown_references {
                    grouped
                        .entry(reference_file)
                        .or_default()
                        .push(reference_line + 1);
                }
                lines.extend(
                    grouped
                        .into_iter()
                        .map(|(reference_file, reference_lines)| {
                            let reference_lines = reference_lines
                                .into_iter()
                                .map(|reference_line| reference_line.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{reference_file}: {reference_lines}")
                        }),
                );
            }

            let omitted = total_references.saturating_sub(refs_limit);
            if omitted > 0 {
                lines.push(format!(
                    "{omitted} more reference sites; raise refs_limit to see them."
                ));
            }
        }
    }
    Ok(lines.join("\n"))
}

fn append_limited_section(
    lines: &mut Vec<String>,
    heading: &str,
    entries: Vec<String>,
    limit: usize,
) {
    let total = entries.len();
    lines.push(format!("{heading}:"));
    if total == 0 {
        lines.push("none".to_string());
        return;
    }
    lines.extend(entries.into_iter().take(limit));
    if total > limit {
        lines.push(format!("… {} more; raise ref_limit", total - limit));
    }
}

pub(crate) fn map_symbols(
    project_root: &Path,
    index: &SemanticIndex,
    names: &[String],
    ref_limit: usize,
) -> Result<String> {
    if !(1..=MAX_MAP_NAMES).contains(&names.len()) {
        bail!("names must contain between 1 and {MAX_MAP_NAMES} entries");
    }
    if names.iter().any(|name| name.trim().is_empty()) {
        bail!("names must not contain empty entries");
    }

    let mut sources = SourceCache::new(project_root);
    let mut resolutions = Vec::new();
    let mut lines = Vec::new();
    for name in names {
        let exact_qualified = index
            .index()
            .documents
            .iter()
            .flat_map(|document| document.symbols.iter())
            .chain(index.index().external_symbols.iter())
            .any(|information| qualified_name(information) == *name || information.symbol == *name);
        let exact_bare = index
            .index()
            .documents
            .iter()
            .flat_map(|document| document.symbols.iter())
            .chain(index.index().external_symbols.iter())
            .any(|information| display_name(information) == *name);
        if !exact_qualified && !exact_bare {
            lines.push(format!(
                "no symbol named {name}; try scip_find with a fragment"
            ));
            continue;
        }

        let symbols = matching_symbol_ids(index, name);
        let mut matches = symbols
            .principals
            .iter()
            .filter_map(|symbol| {
                let information = index.information(symbol)?;
                let (file, line) = index.definition_site(symbol)?.clone();
                Some((file, line, symbol.clone(), information))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2))
        });

        if matches.is_empty() {
            lines.push(format!(
                "no symbol named {name}; try scip_find with a fragment"
            ));
            continue;
        }
        if matches.len() > 1 {
            lines.push(format!("## ambiguous: {name}"));
            lines.extend(
                symbol_candidates(index, name, false)?
                    .into_iter()
                    .map(|candidate| candidate.render()),
            );
            continue;
        }

        let (file, line, symbol, information) = matches.remove(0);
        if !exact_qualified {
            resolutions.push(format!("resolved {name} → {}", qualified_name(information)));
        }
        lines.push(format!("## {}", qualified_name(information)));
        lines.push(format!(
            "definition: {} {file}:{}",
            inferred_kind(information),
            line + 1
        ));
        let signature = symbol_signature(information);
        lines.push(format!(
            "signature: {}",
            if signature.is_empty() {
                "-"
            } else {
                &signature
            }
        ));

        let family = matching_symbol_ids(index, &display_name(information));
        let reference_symbols = family.reference_symbols(&symbol);
        let reference_sites = reference_symbols
            .iter()
            .filter_map(|reference_symbol| index.references_for(reference_symbol))
            .flat_map(|sites| sites.iter().cloned())
            .collect::<BTreeSet<_>>();
        let mut references = Vec::new();
        for (reference_file, reference_line) in reference_sites {
            if sources.is_import_or_export(&reference_file, reference_line) {
                continue;
            }
            let prefix = format!("{reference_file}:{}", reference_line + 1);
            references.push(
                match sources.display_line(&reference_file, reference_line) {
                    Some(source) => format!("{prefix}: {source}"),
                    None => prefix,
                },
            );
        }
        append_limited_section(&mut lines, "references", references, ref_limit);

        let callers = index.direct_caller_lines(project_root, &reference_symbols);
        append_limited_section(&mut lines, "callers", callers, ref_limit);
    }
    resolutions.extend(lines);
    Ok(resolutions.join("\n"))
}

pub(crate) fn references(index: &SemanticIndex, name: &str, limit: usize) -> Result<String> {
    if name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let symbols = matching_symbol_ids(index, name);
    if symbols.is_empty() {
        return Ok("no matches".to_string());
    }
    if let Some(response) = ambiguity_response(index, name, &symbols) {
        return Ok(response);
    }
    let reference_symbols = symbols.all_reference_symbols();

    let locations = reference_symbols
        .into_iter()
        .filter_map(|symbol| index.references_for(symbol))
        .flat_map(|sites| sites.iter())
        .map(|(file, line)| (file.clone(), line + 1))
        .collect::<BTreeSet<_>>();
    if locations.is_empty() {
        return Ok("no references".to_string());
    }

    let total = locations.len();
    let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (file, line) in locations.into_iter().take(limit) {
        grouped.entry(file).or_default().push(line);
    }
    let rendered = grouped
        .into_iter()
        .map(|(file, lines)| {
            let lines = lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{file}: {lines}")
        })
        .collect::<Vec<_>>();

    let omitted = total.saturating_sub(limit);
    let mut lines = rendered;
    if omitted > 0 {
        lines.push(format!(
            "{omitted} more reference sites; raise limit to see them."
        ));
    }
    Ok(lines.join("\n"))
}

pub(crate) fn outline(index: &SemanticIndex, file: &str) -> Result<String> {
    if file.trim().is_empty() {
        bail!("file must not be empty");
    }
    if Path::new(file).is_absolute() {
        bail!("file must be relative to project_root");
    }
    let normalized = file.trim_start_matches("./").replace('\\', "/");
    let Some(document) = index
        .index()
        .documents
        .iter()
        .find(|document| document.relative_path == normalized)
    else {
        bail!("file not found in index: {normalized}");
    };
    let mut definitions = Vec::new();

    for occurrence in &document.occurrences {
        if !is_definition(occurrence) {
            continue;
        }
        let (Some(line), Some(information)) = (
            occurrence_start_line(occurrence),
            index.information(&occurrence.symbol),
        ) else {
            continue;
        };
        definitions.push((line, inferred_kind(information), display_name(information)));
    }
    definitions
        .sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    definitions.dedup();

    Ok(format_limited(
        definitions
            .into_iter()
            .map(|(line, kind, name)| format!("{} {kind} {name}", line + 1))
            .collect(),
        MAX_RESULT_LINES - 1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        DEFAULT_CALLERS_LIMIT, DEFAULT_DEAD_LIMIT, DEFAULT_MAP_REFS_LIMIT, DEFAULT_REFS_LIMIT,
    };
    use crate::test_support::*;
    use scip::types::{symbol_information, Document, SymbolInformation};
    use std::fs;

    #[test]
    fn search_ranks_exact_then_prefix_and_truncates() {
        let output = search(&SemanticIndex::new(fixture(false)), "FORMATDATE", 1).expect("search");
        assert_eq!(output, "function formatDate src/lib/date.ts:42\n… +1 more");
    }

    #[test]
    fn find_returns_disambiguated_candidates_with_signatures() {
        let output = find(&SemanticIndex::new(fixture(false)), "rngState", 10, false)
            .expect("find candidates");
        assert_eq!(
            output,
            "persistence.ts/SaveData#rngState. | property | src/persistence.ts:34 | rngState: number\nrng.ts/rngState(). | function | src/lib/rng.ts:3 | function rngState(): number"
        );

        let limited = find(&SemanticIndex::new(fixture(false)), "formatDate", 1, false)
            .expect("limited candidates");
        assert_eq!(limited.lines().count(), 2);
        assert!(limited.ends_with("… 1 more; raise limit"));
    }

    #[test]
    fn find_unreferenced_excludes_symbols_with_inbound_references() {
        let output = find(&SemanticIndex::new(fixture(false)), "*", 100, true)
            .expect("unreferenced candidates");
        assert!(output.contains("dead.ts/neverUsed(). | function | src/lib/dead.ts:11"));
        assert!(!output.contains("localOnly"));
        assert!(!output.contains("formatDate"));
    }

    #[test]
    fn map_symbols_disambiguates_and_caps_references_and_callers() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = SemanticIndex::new(fixture(false));

        let ambiguous = map_symbols(
            &project.root,
            &index,
            &["rngState".to_string()],
            DEFAULT_MAP_REFS_LIMIT,
        )
        .expect("ambiguous map");
        assert!(ambiguous.starts_with("## ambiguous: rngState\n"));
        assert!(ambiguous.contains(
            "persistence.ts/SaveData#rngState. | property | src/persistence.ts:34 | rngState: number"
        ));

        let mapped = map_symbols(
            &project.root,
            &index,
            &["date.ts/formatDate().".to_string()],
            1,
        )
        .expect("qualified map");
        assert!(mapped.contains("definition: function src/lib/date.ts:42"));
        assert!(mapped.contains("signature: function formatDate(input: Date): string"));
        assert_eq!(
            mapped
                .lines()
                .filter(|line| *line == "… 1 more; raise ref_limit")
                .count(),
            2
        );
    }

    #[test]
    fn definition_includes_compact_signature_and_documentation() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = SemanticIndex::new(fixture(false));
        let output = definitions(&project.root, &index, "formatDate").expect("definitions");
        assert_eq!(
            output,
            "function formatDate src/lib/date.ts:42 function formatDate(input: Date): string — Formats a date for display.\ndef> export function formatDate(input: Date): string {"
        );

        let fallback =
            definitions(&project.root, &index, "formatDateLo").expect("fallback definition");
        assert!(fallback.starts_with("function formatDateLong src/lib/date.ts:10"));
        assert!(fallback.contains("def> export function formatDateLong"));
    }

    #[test]
    fn ambiguous_symbol_queries_return_compact_candidates() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = SemanticIndex::new(fixture(false));

        let search_output = search(&index, "rngState", 10).expect("search rngState");
        assert_eq!(
            search_output,
            "function rngState src/lib/rng.ts:3\nproperty rngState (SaveData) src/persistence.ts:34"
        );

        let expected = "ambiguous: rngState\n- persistence.ts/SaveData#rngState. | property | src/persistence.ts\n- rng.ts/rngState(). | function | src/lib/rng.ts\nspecify the qualified name";
        let definition_output = definitions(&project.root, &index, "rngState").expect("definition");
        let map_output = map_bundle(
            &project.root,
            &index,
            &["rngState".to_string()],
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("map rngState");
        let refs_output = references(&index, "rngState", DEFAULT_REFS_LIMIT).expect("references");
        let callers_output =
            callers(&project.root, &index, "rngState", 1, DEFAULT_CALLERS_LIMIT).expect("callers");

        assert_eq!(definition_output, expected);
        assert_eq!(map_output, expected);
        assert_eq!(refs_output, expected);
        assert_eq!(callers_output, expected);

        let qualified =
            definitions(&project.root, &index, "rng.ts/rngState().").expect("qualified definition");
        assert!(qualified.starts_with("function rngState src/lib/rng.ts:3"));
        assert!(!qualified.contains("src/persistence.ts"));
    }

    #[test]
    fn scip_map_filters_import_lines_unless_requested() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = SemanticIndex::new(fixture(false));
        let names = ["formatDate".to_string()];

        let filtered = map_bundle(
            &project.root,
            &index,
            &names,
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("filtered map");
        assert!(!filtered.contains("src/app.ts:8:"));
        assert!(!filtered.contains("src/app.ts:12:"));
        assert!(filtered.contains("src/app.ts:40: const label = formatDate(today);"));

        let included = map_bundle(
            &project.root,
            &index,
            &names,
            true,
            DEFAULT_MAP_REFS_LIMIT,
            true,
        )
        .expect("unfiltered map");
        assert!(included.contains("src/app.ts:8: formatDate"));
        assert!(included.contains("src/app.ts:12: import { formatDate } from './lib/date';"));
    }

    #[test]
    fn pure_alias_references_fold_into_the_defined_symbol() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        index.external_symbols.push(SymbolInformation {
            symbol: BARREL_ALIAS_SYMBOL.to_string(),
            display_name: "formatDate".to_string(),
            ..Default::default()
        });
        index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/app.ts")
            .expect("app document")
            .occurrences
            .push(legacy_occurrence(BARREL_ALIAS_SYMBOL, 34, false));
        let index = SemanticIndex::new(index);

        let map = map_bundle(
            &project.root,
            &index,
            &["formatDate".to_string()],
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("alias map");
        assert_eq!(map.matches("## ").count(), 1);
        assert!(map.contains("src/app.ts:35: return formatDate(today);"));
        assert!(map.contains("src/app.ts:40: const label = formatDate(today);"));

        let refs = references(&index, "formatDate", DEFAULT_REFS_LIMIT).expect("alias refs");
        assert!(refs.contains("src/app.ts: 8, 12, 35, 40"));

        let callers = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("alias callers");
        assert!(callers.contains("<module> src/app.ts (x2)"));
    }

    #[test]
    fn scip_map_without_context_uses_grouped_reference_format() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = map_bundle(
            &project.root,
            &SemanticIndex::new(fixture(false)),
            &["formatDate".to_string()],
            false,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("context-free map");

        assert_eq!(
            output,
            "## function formatDate src/lib/date.ts:42\nfunction formatDate(input: Date): string — Formats a date for display.\nsrc/app.ts: 40\nsrc/lib/date.ts: 15"
        );
        assert!(!output.contains("def>"));
    }

    #[test]
    fn map_and_refs_cap_reference_sites_with_raise_hints() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let mut occurrences = Vec::new();
        let mut sources = Vec::new();
        for line in 0..DEFAULT_MAP_REFS_LIMIT + 5 {
            occurrences.push(legacy_occurrence(DATE_SYMBOL, line as i32, false));
            sources.push(format!("const date{line} = formatDate(today);"));
        }
        index.documents.push(Document {
            language: "typescript".to_string(),
            relative_path: "src/many.ts".to_string(),
            occurrences,
            ..Default::default()
        });
        fs::write(project.root.join("src/many.ts"), sources.join("\n"))
            .expect("write many-reference source");
        let index = SemanticIndex::new(index);

        let output = map_bundle(
            &project.root,
            &index,
            &["formatDate".to_string()],
            false,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("large map");
        assert!(output.contains("reference sites: 27"));
        assert!(output.contains(
            "src/many.ts: 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18"
        ));
        assert!(output.ends_with("7 more reference sites; raise refs_limit to see them."));

        let refs = references(&index, "formatDate", DEFAULT_REFS_LIMIT).expect("large refs");
        assert!(refs.ends_with("9 more reference sites; raise limit to see them."));
        assert!(!refs.contains("src/many.ts: 17"));
    }

    #[test]
    fn scip_map_reports_each_zero_match_name() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = map_bundle(
            &project.root,
            &SemanticIndex::new(fixture(false)),
            &["missingSymbol".to_string()],
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("zero-match map");
        assert_eq!(output, "## no matches: missingSymbol");
    }

    #[test]
    fn references_are_grouped_by_file_and_limited() {
        let output =
            references(&SemanticIndex::new(fixture(true)), "formatDate", 2).expect("references");
        assert_eq!(
            output,
            "src/app.ts: 8, 12\n3 more reference sites; raise limit to see them."
        );
    }

    #[test]
    fn scip_callers_depth_two_walks_up_to_the_callers_caller() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = callers(
            &project.root,
            &SemanticIndex::new(fixture(false)),
            "formatDate",
            2,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("transitive callers");
        assert_eq!(
            output,
            "## callers of function formatDate src/lib/date.ts:42\n<module> src/app.ts (x1)\nfunction formatDateLong src/lib/date.ts:10 (x1)\n  <module> src/app.ts (x1)"
        );
    }

    #[test]
    fn scip_callers_falls_back_when_definition_enclosing_range_is_empty() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let long_date_definition = index
            .documents
            .iter_mut()
            .flat_map(|document| document.occurrences.iter_mut())
            .find(|occurrence| occurrence.symbol == LONG_DATE_SYMBOL && is_definition(occurrence))
            .expect("formatDateLong definition");
        long_date_definition.enclosing_range.clear();
        let index = SemanticIndex::new(index);

        let output = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("fallback callers");
        assert!(output.contains("function formatDateLong src/lib/date.ts:10 (x1)"));
    }

    #[test]
    fn scip_callers_prefers_the_innermost_enclosing_definition() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let date_document = index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/lib/date.ts")
            .expect("date document");
        date_document.symbols.push(symbol_information(
            INNER_SYMBOL,
            "inner",
            symbol_information::Kind::Function,
            "function inner(): void",
            "Nested function.",
        ));
        let mut inner_definition = legacy_occurrence(INNER_SYMBOL, 12, true);
        inner_definition.enclosing_range = vec![12, 0, 16, 0];
        date_document.occurrences.push(inner_definition);
        let index = SemanticIndex::new(index);

        let output = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("innermost callers");
        assert!(output.contains("function inner src/lib/date.ts:13 (x1)"));
        assert!(!output.contains("function formatDateLong"));
    }

    #[test]
    fn scip_callers_attributes_object_literal_methods_with_enclosing_ranges() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let app_document = index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/app.ts")
            .expect("app document");
        app_document.symbols.push(symbol_information(
            OBJECT_METHOD_SYMBOL,
            "getCodeActions",
            symbol_information::Kind::Property,
            "getCodeActions(context): string",
            "Object-literal method.",
        ));
        let mut method_definition = legacy_occurrence(OBJECT_METHOD_SYMBOL, 31, true);
        method_definition.enclosing_range = vec![31, 0, 36, 0];
        app_document.occurrences.push(method_definition);
        app_document
            .occurrences
            .push(legacy_occurrence(DATE_SYMBOL, 34, false));
        let index = SemanticIndex::new(index);

        let output = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("object-literal callers");
        assert!(output.contains("property getCodeActions src/app.ts:32 (x1)"));
        assert!(output.contains("<module> src/app.ts (x1)"));
    }

    #[test]
    fn scip_dead_exports_only_false_preserves_all_declaration_candidates() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = dead_exports(
            &project.root,
            &SemanticIndex::new(fixture(false)),
            Some("src/lib/dead.ts"),
            DEFAULT_DEAD_LIMIT,
            false,
        )
        .expect("all dead declarations");
        assert_eq!(
            output,
            "# dead exports (exports_only=false) — dynamic/string-based uses aren't visible to SCIP\nfile-local: function localOnly src/lib/dead.ts:2 (x1 in-file)\ndead: function neverUsed src/lib/dead.ts:11\ndead: method run src/lib/dead.ts:16\ndead: constant LIMIT src/lib/dead.ts:18\ndead: constant helper src/lib/dead.ts:19"
        );
    }

    #[test]
    fn outline_is_a_sorted_file_skeleton() {
        let output =
            outline(&SemanticIndex::new(fixture(false)), "./src/lib/date.ts").expect("outline");
        assert_eq!(output, "10 function formatDateLong\n42 function formatDate");
    }
}
