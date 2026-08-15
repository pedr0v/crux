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

pub(crate) const MAX_MAP_NAMES: usize = 8;
const MAX_MAP_RESULT_LINES: usize = 250;
const AMBIGUITY_LIMIT: usize = 10;
const GROUPED_REFERENCE_THRESHOLD: usize = 30;

struct SymbolCandidate {
    rank: usize,
    reference_count: usize,
    display_name: String,
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

    fn render_grouped(&self) -> String {
        let signature = if self.signature.is_empty() {
            "-"
        } else {
            &self.signature
        };
        format!(
            "- {} | {} | line {} | {signature}",
            self.qualified_name,
            self.kind,
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
            reference_count: index
                .references_for(&information.symbol)
                .map_or(0, BTreeSet::len),
            display_name: display,
            qualified_name: qualified,
            kind: inferred_kind(information),
            file,
            line,
            signature: symbol_signature(information),
            symbol: information.symbol.clone(),
        });
    }

    rank_candidates(&mut candidates);
    candidates.dedup_by(|left, right| left.symbol == right.symbol);
    Ok(candidates)
}

fn rank_candidates(candidates: &mut [SymbolCandidate]) {
    candidates.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| right.reference_count.cmp(&left.reference_count))
            .then_with(|| left.qualified_name.cmp(&right.qualified_name))
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.symbol.cmp(&right.symbol))
    });
}

fn ambiguity_candidate_lines(
    heading: String,
    candidates: Vec<SymbolCandidate>,
) -> Option<Vec<String>> {
    if candidates.len() < 2 {
        return None;
    }

    let total = candidates.len();
    let mut groups: Vec<(String, Vec<&SymbolCandidate>)> = Vec::new();
    for candidate in candidates.iter().take(AMBIGUITY_LIMIT) {
        if let Some((_, entries)) = groups.iter_mut().find(|(file, _)| file == &candidate.file) {
            entries.push(candidate);
        } else {
            groups.push((candidate.file.clone(), vec![candidate]));
        }
    }

    let mut lines = vec![heading];
    for (file, candidates) in groups {
        lines.push(format!("{file}:"));
        lines.extend(candidates.into_iter().map(SymbolCandidate::render_grouped));
    }
    if total > AMBIGUITY_LIMIT {
        lines.push(format!(
            "… {} more candidates; pass a qualified name (scip_find name offset={} limit={} to page)",
            total - AMBIGUITY_LIMIT,
            AMBIGUITY_LIMIT,
            AMBIGUITY_LIMIT
        ));
    }
    Some(lines)
}

fn dominant_candidate<'a>(name: &str, candidates: &'a [SymbolCandidate]) -> Option<&'a str> {
    let [first, second, ..] = candidates else {
        return None;
    };
    let uniquely_outranks = first.rank < second.rank
        || (first.rank == second.rank && first.reference_count > second.reference_count);
    (first.display_name == name && uniquely_outranks).then_some(first.symbol.as_str())
}

pub(crate) fn find(
    index: &SemanticIndex,
    name: &str,
    limit: usize,
    offset: usize,
    unreferenced: bool,
) -> Result<String> {
    let candidates = symbol_candidates(index, name, unreferenced)?;
    let total = candidates.len();
    if total == 0 {
        return Ok("no matches".to_string());
    }

    let mut lines = candidates
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|candidate| candidate.render())
        .collect::<Vec<_>>();
    let next_offset = offset.saturating_add(lines.len());
    if next_offset < total {
        lines.push(format!(
            "… {} more (pass offset={next_offset})",
            total - next_offset
        ));
    }
    Ok(lines.join("\n"))
}

fn ambiguity_lines(
    index: &SemanticIndex,
    name: &str,
    symbols: &MatchingSymbols,
) -> Option<Vec<String>> {
    let mut candidates = symbol_candidates(index, name, false)
        .ok()?
        .into_iter()
        .filter(|candidate| symbols.principals.contains(&candidate.symbol))
        .collect::<Vec<_>>();
    rank_candidates(&mut candidates);
    ambiguity_candidate_lines(format!("ambiguous: {name}"), candidates)
}

fn ambiguity_response(
    index: &SemanticIndex,
    name: &str,
    symbols: &MatchingSymbols,
) -> Option<String> {
    Some(ambiguity_lines(index, name, symbols)?.join("\n"))
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
    let mut compact_insertions = Vec::new();
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
            enumeration_names: Vec::new(),
            truncated: false,
        };
        walk.render_symbols(&symbols.reference_symbols(&symbol), depth, 0);
        let enumeration_names = walk.enumeration_names;
        let truncated = walk.truncated;
        lines.append(&mut walk.lines);
        if lines.len() == before {
            lines.push("no callers".to_string());
        }
        let mut compact_lines = Vec::new();
        append_compact_enumeration(&mut compact_lines, "callers", enumeration_names, truncated);
        if let Some(compact_line) = compact_lines.pop() {
            compact_insertions.push((lines.len(), compact_line));
        }
    }

    let visible_line_count = if lines.len() > MAX_MAP_RESULT_LINES {
        let shown = MAX_MAP_RESULT_LINES - 1;
        let omitted = lines.len() - shown;
        lines.truncate(shown);
        lines.push(format!("… +{omitted} more lines"));
        shown
    } else {
        lines.len()
    };
    for (position, compact_line) in compact_insertions.into_iter().rev() {
        if position <= visible_line_count {
            lines.insert(position, compact_line);
        }
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

fn format_limited_output(
    lines: Vec<String>,
    limit: usize,
    offset: usize,
    paginated: bool,
) -> String {
    let total = lines.len();
    if total == 0 {
        return "no matches".to_string();
    }
    let mut output = lines
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let next_offset = offset.saturating_add(output.len());
    if next_offset < total {
        if paginated {
            output.push(format!(
                "… {} more (pass offset={next_offset})",
                total - next_offset
            ));
        } else {
            output.push(format!("… +{} more", total - next_offset));
        }
    }
    output.join("\n")
}

pub(crate) fn format_limited(lines: Vec<String>, limit: usize, offset: usize) -> String {
    format_limited_output(lines, limit, offset, true)
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
    Ok(format_limited_output(
        hits.into_iter().map(|hit| hit.4).collect(),
        limit,
        0,
        false,
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
    let ambiguities = matches_by_name
        .iter()
        .filter_map(|(name, symbols)| ambiguity_lines(index, name, symbols))
        .flatten()
        .collect::<Vec<_>>();
    if !ambiguities.is_empty() {
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
                lines.push(format!("… {omitted} more (pass offset={refs_limit})"));
            }
        }
    }
    Ok(lines.join("\n"))
}

fn append_limited_section(
    lines: &mut Vec<String>,
    heading: &str,
    entries: Vec<String>,
    total: usize,
    limit: usize,
    offset: usize,
) -> bool {
    lines.push(format!("{heading}:"));
    if total == 0 {
        lines.push("none".to_string());
        return false;
    }
    let shown_count = total.saturating_sub(offset).min(limit);
    let next_offset = offset.saturating_add(shown_count);
    lines.extend(entries);
    if next_offset < total {
        lines.push(format!(
            "… {} more (pass offset={next_offset})",
            total - next_offset
        ));
    }
    offset > 0 || next_offset < total
}

fn append_compact_enumeration(
    lines: &mut Vec<String>,
    heading: &str,
    mut entries: Vec<String>,
    truncated: bool,
) {
    entries.sort();
    entries.dedup();
    if entries.is_empty() {
        return;
    }

    let marker = if truncated { " (truncated)" } else { "" };
    lines.push(format!("{heading}: {}{marker}", entries.join("; ")));
}

fn reference_relevance(definition_file: &str, reference_file: &str) -> u8 {
    if reference_file == definition_file {
        return 0;
    }
    let definition_scope = definition_file.split('/').next();
    let reference_scope = reference_file.split('/').next();
    if definition_scope == reference_scope {
        1
    } else {
        2
    }
}

fn grouped_reference_lines(reference_sites: &[(String, usize)]) -> Vec<String> {
    let mut grouped: Vec<(String, Vec<usize>)> = Vec::new();
    for (file, line) in reference_sites {
        if let Some((_, lines)) = grouped.last_mut().filter(|(entry, _)| entry == file) {
            lines.push(line + 1);
        } else {
            grouped.push((file.clone(), vec![line + 1]));
        }
    }
    grouped
        .into_iter()
        .map(|(file, lines)| {
            let lines = lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{file}: {lines}")
        })
        .collect()
}

pub(crate) fn map_symbols(
    project_root: &Path,
    index: &SemanticIndex,
    names: &[String],
    ref_limit: usize,
    offset: usize,
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
        let mut other_candidate_count = None;
        if matches.len() > 1 {
            let candidates = symbol_candidates(index, name, false)?;
            let dominant_symbol = dominant_candidate(name, &candidates).map(str::to_owned);
            let Some(position) = dominant_symbol
                .as_deref()
                .and_then(|symbol| matches.iter().position(|entry| entry.2 == symbol))
            else {
                if let Some(ambiguity) =
                    ambiguity_candidate_lines(format!("## ambiguous: {name}"), candidates)
                {
                    lines.extend(ambiguity);
                }
                continue;
            };
            other_candidate_count = Some(candidates.len().saturating_sub(1));
            matches.swap(0, position);
            matches.truncate(1);
        }

        let (file, line, symbol, information) = matches.remove(0);
        if !exact_qualified {
            resolutions.push(format!("resolved {name} → {}", qualified_name(information)));
        }
        if let Some(count) = other_candidate_count {
            resolutions.push(format!("other candidates: {count} (scip_find to list)"));
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
        let mut reference_sites = reference_symbols
            .iter()
            .filter_map(|reference_symbol| index.references_for(reference_symbol))
            .flat_map(|sites| sites.iter().cloned())
            .collect::<BTreeSet<_>>();
        reference_sites.retain(|(reference_file, reference_line)| {
            !sources.is_import_or_export(reference_file, *reference_line)
        });
        let mut reference_sites = reference_sites.into_iter().collect::<Vec<_>>();
        reference_sites.sort_by(|left, right| {
            reference_relevance(&file, &left.0)
                .cmp(&reference_relevance(&file, &right.0))
                .then_with(|| left.cmp(right))
        });
        let total_references = reference_sites.len();
        let shown_reference_sites = reference_sites
            .into_iter()
            .skip(offset)
            .take(ref_limit)
            .collect::<Vec<_>>();
        let reference_files = shown_reference_sites
            .iter()
            .map(|(reference_file, _)| reference_file.clone())
            .collect();
        let references = if total_references > GROUPED_REFERENCE_THRESHOLD {
            grouped_reference_lines(&shown_reference_sites)
        } else {
            shown_reference_sites
                .iter()
                .map(|(reference_file, reference_line)| {
                    let prefix = format!("{reference_file}:{}", reference_line + 1);
                    match sources.display_line(reference_file, *reference_line) {
                        Some(source) => format!("{prefix}: {source}"),
                        None => prefix,
                    }
                })
                .collect()
        };
        let references_truncated = append_limited_section(
            &mut lines,
            "references",
            references,
            total_references,
            ref_limit,
            offset,
        );

        let callers = index.direct_caller_entries(project_root, &reference_symbols);
        let total_callers = callers.len();
        let caller_names = callers
            .iter()
            .skip(offset)
            .take(ref_limit)
            .map(|(_, name)| name.clone())
            .collect();
        let caller_lines = callers
            .into_iter()
            .skip(offset)
            .take(ref_limit)
            .map(|(line, _)| line)
            .collect();
        let callers_truncated = append_limited_section(
            &mut lines,
            "callers",
            caller_lines,
            total_callers,
            ref_limit,
            offset,
        );
        if !callers_truncated {
            append_compact_enumeration(&mut lines, "callers", caller_names, false);
        }
        if !references_truncated {
            append_compact_enumeration(&mut lines, "files", reference_files, false);
        }
    }
    let available_lines = MAX_MAP_RESULT_LINES.saturating_sub(resolutions.len());
    if lines.len() > available_lines {
        let shown = available_lines.saturating_sub(1);
        let omitted = lines.len().saturating_sub(shown);
        lines.truncate(shown);
        if available_lines > 0 {
            lines.push(format!("… +{omitted} more lines"));
        }
    }
    resolutions.extend(lines);
    Ok(resolutions.join("\n"))
}

pub(crate) fn references(
    index: &SemanticIndex,
    name: &str,
    limit: usize,
    offset: usize,
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
    let shown_locations = locations
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let shown_count = shown_locations.len();
    let files = shown_locations
        .iter()
        .map(|(file, _)| file.clone())
        .collect();
    let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (file, line) in shown_locations {
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

    let next_offset = offset.saturating_add(shown_count);
    let omitted = total.saturating_sub(next_offset);
    let mut lines = rendered;
    if omitted > 0 {
        lines.push(format!("… {omitted} more (pass offset={next_offset})"));
    }
    if offset == 0 && omitted == 0 {
        append_compact_enumeration(&mut lines, "files", files, false);
    }
    Ok(lines.join("\n"))
}

pub(crate) fn outline(
    index: &SemanticIndex,
    file: &str,
    limit: usize,
    offset: usize,
) -> Result<String> {
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
        limit,
        offset,
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
        let output = find(
            &SemanticIndex::new(fixture(false)),
            "rngState",
            10,
            0,
            false,
        )
        .expect("find candidates");
        assert_eq!(
            output,
            "rng.ts/rngState(). | function | src/lib/rng.ts:3 | function rngState(): number\npersistence.ts/SaveData#rngState. | property | src/persistence.ts:34 | rngState: number"
        );

        let limited = find(
            &SemanticIndex::new(fixture(false)),
            "formatDate",
            1,
            0,
            false,
        )
        .expect("limited candidates");
        assert_eq!(limited.lines().count(), 2);
        assert!(limited.ends_with("… 1 more (pass offset=1)"));
    }

    #[test]
    fn find_unreferenced_excludes_symbols_with_inbound_references() {
        let output = find(&SemanticIndex::new(fixture(false)), "*", 100, 0, true)
            .expect("unreferenced candidates");
        assert!(output.contains("dead.ts/neverUsed(). | function | src/lib/dead.ts:11"));
        assert!(!output.contains("localOnly"));
        assert!(!output.contains("formatDate"));
    }

    #[test]
    fn ambiguity_responses_cap_ranked_candidates_at_ten() {
        let project = TestProject::new();
        let mut fixture = fixture(false);
        for index in 0..12 {
            let symbol =
                format!("scip-typescript npm fixture 1.0.0 duplicate{index}.ts/duplicate().");
            fixture.documents.push(Document {
                language: "typescript".to_string(),
                relative_path: format!("src/duplicates/duplicate{index}.ts"),
                occurrences: vec![legacy_occurrence(&symbol, 0, true)],
                symbols: vec![symbol_information(
                    &symbol,
                    "duplicate",
                    symbol_information::Kind::Function,
                    "function duplicate(): void",
                    "Duplicate candidate.",
                )],
                ..Default::default()
            });
        }
        let index = SemanticIndex::new(fixture);

        let mapped = map_symbols(
            &project.root,
            &index,
            &["duplicate".to_string()],
            DEFAULT_MAP_REFS_LIMIT,
            0,
        )
        .expect("ambiguous map");
        assert!(mapped.starts_with("## ambiguous: duplicate\n"));
        assert_eq!(
            mapped
                .lines()
                .filter(|line| line.starts_with("- duplicate"))
                .count(),
            AMBIGUITY_LIMIT
        );
        assert!(mapped.ends_with(
            "… 2 more candidates; pass a qualified name (scip_find name offset=10 limit=10 to page)"
        ));

        let definition =
            definitions(&project.root, &index, "duplicate").expect("ambiguous definition response");
        assert_eq!(
            definition
                .lines()
                .filter(|line| line.starts_with("- duplicate"))
                .count(),
            AMBIGUITY_LIMIT
        );
        assert!(definition.ends_with(
            "… 2 more candidates; pass a qualified name (scip_find name offset=10 limit=10 to page)"
        ));
    }

    #[test]
    fn map_symbols_auto_resolves_and_caps_references_and_callers() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = SemanticIndex::new(fixture(false));

        let resolved = map_symbols(
            &project.root,
            &index,
            &["rngState".to_string()],
            DEFAULT_MAP_REFS_LIMIT,
            0,
        )
        .expect("auto-resolved map");
        assert!(resolved.starts_with("resolved rngState → rng.ts/rngState().\n"));
        assert!(resolved.contains("other candidates: 1 (scip_find to list)"));
        assert!(resolved.contains("## rng.ts/rngState()."));
        assert!(!resolved.contains("## ambiguous:"));

        let mapped = map_symbols(
            &project.root,
            &index,
            &["date.ts/formatDate().".to_string()],
            1,
            0,
        )
        .expect("qualified map");
        assert!(mapped.contains("definition: function src/lib/date.ts:42"));
        assert!(mapped.contains("signature: function formatDate(input: Date): string"));
        assert_eq!(
            mapped
                .lines()
                .filter(|line| *line == "… 1 more (pass offset=1)")
                .count(),
            2
        );
        assert!(!mapped.lines().any(|line| line.starts_with("callers: ")));
        assert!(!mapped.lines().any(|line| line.starts_with("files: ")));

        let mapped = map_symbols(
            &project.root,
            &index,
            &["date.ts/formatDate().".to_string()],
            DEFAULT_MAP_REFS_LIMIT,
            0,
        )
        .expect("uncapped qualified map");
        assert!(mapped.contains("callers: <module> src/app.ts; formatDateLong"));
        assert!(mapped.contains("files: src/app.ts; src/lib/date.ts"));
    }

    #[test]
    fn map_symbols_groups_large_reference_sets_without_snippets() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut fixture = fixture(false);
        let occurrences = (0..31)
            .map(|line| legacy_occurrence(DATE_SYMBOL, line, false))
            .collect();
        fixture.documents.push(Document {
            language: "typescript".to_string(),
            relative_path: "src/grouped.ts".to_string(),
            occurrences,
            ..Default::default()
        });
        let sources = (0..31)
            .map(|line| format!("const grouped{line} = formatDate(today);"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(project.root.join("src/grouped.ts"), sources)
            .expect("write grouped-reference source");

        let output = map_symbols(
            &project.root,
            &SemanticIndex::new(fixture),
            &["date.ts/formatDate().".to_string()],
            100,
            0,
        )
        .expect("grouped map");
        let reference_lines = output
            .lines()
            .skip_while(|line| *line != "references:")
            .skip(1)
            .take_while(|line| *line != "callers:")
            .collect::<Vec<_>>();
        assert_eq!(reference_lines.first(), Some(&"src/lib/date.ts: 15"));
        assert!(reference_lines
            .iter()
            .any(|line| line.starts_with("src/grouped.ts: 1, 2, 3")));
        assert!(!reference_lines
            .iter()
            .any(|line| line.contains("const grouped")));
    }

    #[test]
    fn offsets_return_disjoint_find_map_reference_and_outline_pages() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = SemanticIndex::new(fixture(true));

        let first_find = find(&index, "formatDate", 1, 0, false).expect("first find page");
        let second_find = find(&index, "formatDate", 1, 1, false).expect("second find page");
        assert!(first_find.starts_with("date.ts/formatDate()."));
        assert!(second_find.starts_with("date.ts/formatDateLong()."));
        assert!(!second_find
            .lines()
            .any(|line| line == first_find.lines().next().unwrap()));

        let first_map = map_symbols(
            &project.root,
            &index,
            &["date.ts/formatDate().".to_string()],
            1,
            0,
        )
        .expect("first map page");
        let second_map = map_symbols(
            &project.root,
            &index,
            &["date.ts/formatDate().".to_string()],
            1,
            1,
        )
        .expect("second map page");
        assert!(first_map.contains("src/lib/date.ts:15:"));
        assert!(second_map.contains("src/app.ts:40:"));
        assert!(!second_map.contains("src/lib/date.ts:15:"));
        assert!(!first_map.lines().any(|line| line.starts_with("files: ")));
        assert!(!second_map.lines().any(|line| line.starts_with("files: ")));

        let first_refs = references(&index, "formatDate", 2, 0).expect("first refs page");
        let second_refs = references(&index, "formatDate", 2, 2).expect("second refs page");
        assert!(first_refs.starts_with("src/app.ts: 8, 12"));
        assert!(!second_refs.contains("src/app.ts: 8, 12"));
        assert!(!first_refs.lines().any(|line| line.starts_with("files: ")));
        assert!(!second_refs.lines().any(|line| line.starts_with("files: ")));

        let first_outline = outline(&index, "src/lib/date.ts", 1, 0).expect("first outline page");
        let second_outline = outline(&index, "src/lib/date.ts", 1, 1).expect("second outline page");
        assert!(first_outline.starts_with("10 function formatDateLong"));
        assert!(second_outline.starts_with("42 function formatDate"));
        assert!(!second_outline.contains("10 function formatDateLong"));
    }

    #[test]
    fn map_symbols_caps_the_total_output_lines() {
        let project = TestProject::new();
        write_fixture(&project, false);
        fs::create_dir_all(project.root.join("src/cap")).expect("create cap source directory");
        let mut fixture = fixture(false);
        let mut names = Vec::new();
        for index in 0..MAX_MAP_NAMES {
            let name = format!("capTarget{index}");
            let symbol = format!("scip-typescript npm fixture 1.0.0 cap{index}.ts/{name}().");
            let file = format!("src/cap/cap{index}.ts");
            let mut occurrences = vec![legacy_occurrence(&symbol, 0, true)];
            occurrences.extend((1..=30).map(|line| legacy_occurrence(&symbol, line, false)));
            fixture.documents.push(Document {
                language: "typescript".to_string(),
                relative_path: file.clone(),
                occurrences,
                symbols: vec![symbol_information(
                    &symbol,
                    &name,
                    symbol_information::Kind::Function,
                    &format!("function {name}(): void"),
                    "Map cap target.",
                )],
                ..Default::default()
            });
            let sources = std::iter::once(format!("function {name}(): void {{}}"))
                .chain((1..=30).map(|line| format!("const use{line} = {name}();")))
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(project.root.join(file), sources).expect("write cap source");
            names.push(name);
        }

        let output = map_symbols(&project.root, &SemanticIndex::new(fixture), &names, 30, 0)
            .expect("capped map");
        assert_eq!(output.lines().count(), MAX_MAP_RESULT_LINES);
        assert!(output.lines().last().unwrap().starts_with("… +"));
    }

    #[test]
    fn scip_map_omits_empty_enumerations() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = map_symbols(
            &project.root,
            &SemanticIndex::new(fixture(false)),
            &["neverUsed".to_string()],
            DEFAULT_MAP_REFS_LIMIT,
            0,
        )
        .expect("empty map");
        assert!(!output.lines().any(|line| line.starts_with("callers: ")));
        assert!(!output.lines().any(|line| line.starts_with("files: ")));
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

        let expected = "ambiguous: rngState\nsrc/lib/rng.ts:\n- rng.ts/rngState(). | function | line 3 | function rngState(): number\nsrc/persistence.ts:\n- persistence.ts/SaveData#rngState. | property | line 34 | rngState: number";
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
        let refs_output =
            references(&index, "rngState", DEFAULT_REFS_LIMIT, 0).expect("references");
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

        let refs = references(&index, "formatDate", DEFAULT_REFS_LIMIT, 0).expect("alias refs");
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
    fn map_and_refs_cap_reference_sites_with_offset_hints() {
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
        assert!(output.ends_with("… 7 more (pass offset=20)"));

        let refs = references(&index, "formatDate", DEFAULT_REFS_LIMIT, 0).expect("large refs");
        assert!(refs.ends_with("… 9 more (pass offset=20)"));
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
            references(&SemanticIndex::new(fixture(true)), "formatDate", 2, 0).expect("references");
        assert_eq!(output, "src/app.ts: 8, 12\n… 3 more (pass offset=2)");

        let output = references(
            &SemanticIndex::new(fixture(false)),
            "formatDate",
            DEFAULT_REFS_LIMIT,
            0,
        )
        .expect("uncapped references");
        assert!(output.ends_with("files: src/app.ts; src/lib/date.ts"));

        let output = references(
            &SemanticIndex::new(fixture(false)),
            "neverUsed",
            DEFAULT_REFS_LIMIT,
            0,
        )
        .expect("empty references");
        assert_eq!(output, "no references");
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
            "## callers of function formatDate src/lib/date.ts:42\n<module> src/app.ts (x1)\nfunction formatDateLong src/lib/date.ts:10 (x1)\n  <module> src/app.ts (x1)\ncallers: <module> src/app.ts; formatDateLong"
        );
    }

    #[test]
    fn scip_callers_marks_truncation_and_omits_empty_enumerations() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = SemanticIndex::new(fixture(false));
        let output = callers(&project.root, &index, "formatDate", 1, 1).expect("limited callers");
        assert!(output.ends_with("callers: <module> src/app.ts (truncated)"));

        let output = callers(&project.root, &index, "neverUsed", 1, DEFAULT_CALLERS_LIMIT)
            .expect("empty callers");
        assert!(output.contains("no callers"));
        assert!(!output.lines().any(|line| line.starts_with("callers: ")));
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
        let output = outline(
            &SemanticIndex::new(fixture(false)),
            "./src/lib/date.ts",
            38,
            0,
        )
        .expect("outline");
        assert_eq!(output, "10 function formatDateLong\n42 function formatDate");
    }
}
