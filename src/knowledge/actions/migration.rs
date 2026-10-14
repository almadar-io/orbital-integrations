//! The deterministic import path for markdown and pasted text (`migration/*`): extract units from the source,
//! map them onto the knowledge target, validate them, and drain them through a sink. Specialised to the knowledge
//! target, whose fields are all strings.

use std::collections::{HashMap, HashSet};

use regex::Regex;
use serde_json::{json, Map, Value};

use super::import_sink::MIGRATION_RELATIONSHIPS;

// ---- extraction ----

#[derive(Debug, Clone)]
pub struct ExtractedUnit {
    pub reference: String,
    /// `root` | `folder` | `section` | `qa`
    pub kind: &'static str,
    pub title: Option<String>,
    pub content: Option<String>,
    pub parent_ref: Option<String>,
    /// Deterministic metadata in the order the adapter sets it.
    pub metadata: Vec<(String, Value)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Markdown,
    Paste,
}

#[derive(Clone)]
pub struct RawDocument {
    pub reference: String,
    pub raw: String,
}

#[derive(Default)]
pub struct Extraction {
    pub units: Vec<ExtractedUnit>,
}

struct MarkdownSection {
    heading: String,
    depth: usize,
    body: String,
    children: Vec<MarkdownSection>,
}

struct MarkdownDocument {
    title: String,
    title_from_heading: bool,
    preamble: String,
    title_body: Option<(String, usize)>,
    sections: Vec<MarkdownSection>,
    line_count: usize,
}

struct FlatSection {
    heading: String,
    depth: usize,
    heading_line: usize,
    body_start_line: usize,
}

const LINE: &str = r"[^\n\r\u{2028}\u{2029}]";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|_| {
        Regex::new("$^").unwrap_or_else(|_| unreachable!("a fixed pattern compiles"))
    })
}

fn trim_trailing_newlines(text: &str) -> String {
    text.trim_end_matches('\n').to_string()
}

fn trim_newlines(text: &str) -> String {
    text.trim_matches('\n').to_string()
}

fn slice_lines(lines: &[&str], from: usize, to: usize) -> String {
    let from = from.min(lines.len());
    let to = to.min(lines.len()).max(from);
    lines[from..to].join("\n")
}

fn slugify(text: &str) -> String {
    let lowered = text.to_lowercase();
    let replaced = regex("[^a-z0-9]+").replace_all(&lowered, "-").into_owned();
    let slug = replaced.trim_matches('-').to_string();
    if slug.is_empty() {
        "section".to_string()
    } else {
        slug
    }
}

fn parse_markdown(raw: &str, reference: &str) -> MarkdownDocument {
    let lines: Vec<&str> = raw.split('\n').collect();
    let heading_re = regex(&format!(r"^(#{{1,6}})\s+({LINE}*?)\s*$"));
    let fence_re = regex(r"^(```|~~~)");
    let mut flat: Vec<FlatSection> = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = String::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = fence_re.captures(line.trim_start()) {
            let marker = caps[1].to_string();
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if marker == fence_marker {
                in_fence = false;
            }
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(caps) = heading_re.captures(line) {
            flat.push(FlatSection {
                heading: caps[2].to_string(),
                depth: caps[1].len(),
                heading_line: i + 1,
                body_start_line: i + 2,
            });
        }
    }
    let first = flat.first();
    let promote_title = first.is_some_and(|f| f.depth == 1)
        && !flat.iter().enumerate().any(|(i, s)| i > 0 && s.depth == 1);

    let mut sections: Vec<MarkdownSection> = Vec::new();
    let mut stack: Vec<Vec<usize>> = Vec::new(); // paths into the tree: indices from the root list
    let mut depths: Vec<usize> = Vec::new();
    for (i, f) in flat.iter().enumerate() {
        let next = flat.get(i + 1);
        let body_end_line = next.map_or(lines.len(), |n| n.heading_line - 1);
        let section = MarkdownSection {
            heading: f.heading.clone(),
            depth: f.depth,
            body: trim_trailing_newlines(&slice_lines(
                &lines,
                f.body_start_line - 1,
                body_end_line,
            )),
            children: Vec::new(),
        };
        if i == 0 && promote_title {
            continue;
        }
        while depths.last().is_some_and(|d| *d >= f.depth) {
            depths.pop();
            stack.pop();
        }
        let path = match stack.last() {
            None => {
                sections.push(section);
                vec![sections.len() - 1]
            }
            Some(parent_path) => {
                let mut parent = &mut sections[parent_path[0]];
                for index in &parent_path[1..] {
                    parent = &mut parent.children[*index];
                }
                parent.children.push(section);
                let mut path = parent_path.clone();
                path.push(parent.children.len() - 1);
                path
            }
        };
        stack.push(path);
        depths.push(f.depth);
    }

    let first_heading_line = first.map_or(lines.len() + 1, |f| f.heading_line);
    let head = slice_lines(&lines, 0, first_heading_line - 1);
    let preamble = if head.trim().is_empty() {
        String::new()
    } else {
        trim_newlines(&head)
    };
    let first_h1 = flat.iter().find(|s| s.depth == 1);
    let title_body = match first {
        Some(f) if promote_title => {
            let end = flat.get(1).map_or(lines.len() + 1, |s| s.heading_line) - 1;
            Some((
                trim_trailing_newlines(&slice_lines(&lines, f.body_start_line - 1, end)),
                f.body_start_line,
            ))
        }
        _ => None,
    };
    MarkdownDocument {
        title: match first_h1 {
            Some(h) if !h.heading.is_empty() => h.heading.clone(),
            _ => reference.to_string(),
        },
        title_from_heading: first_h1.is_some_and(|h| !h.heading.is_empty()),
        preamble,
        title_body,
        sections,
        line_count: lines.len(),
    }
}

struct QaPair {
    front: String,
    back: String,
}

fn extract_qa_pairs(body: &str) -> (Vec<QaPair>, String) {
    let qa_line = regex(&format!(r"^Q::\s*({LINE}+?)\s*$"));
    let answer_line = regex(&format!(r"^A::\s*({LINE}+?)\s*$"));
    let term_line = regex(&format!(r"^\*\*({LINE}+?)\*\*\s+—\s+({LINE}+?)\s*$"));
    let lines: Vec<&str> = body.split('\n').collect();
    let mut pairs = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(qa) = qa_line.captures(line) {
            let mut back = String::new();
            let mut consumed = 0;
            if let Some(next) = lines.get(i + 1) {
                if let Some(answer) = answer_line.captures(next) {
                    back = answer[1].to_string();
                    consumed = 1;
                }
            }
            pairs.push(QaPair {
                front: qa[1].to_string(),
                back,
            });
            i += 1 + consumed;
            continue;
        }
        if let Some(term) = term_line.captures(line) {
            pairs.push(QaPair {
                front: term[1].to_string(),
                back: term[2].to_string(),
            });
            i += 1;
            continue;
        }
        kept.push(line);
        i += 1;
    }
    (pairs, trim_newlines(&kept.join("\n")))
}

fn qa_unit(doc_ref: &str, counter: usize, pair: &QaPair, parent: &str) -> ExtractedUnit {
    ExtractedUnit {
        reference: format!("{doc_ref}#qa:{counter}"),
        kind: "qa",
        title: Some(pair.front.clone()),
        content: (!pair.back.is_empty()).then(|| pair.back.clone()),
        parent_ref: Some(parent.to_string()),
        metadata: vec![
            ("front".into(), json!(pair.front)),
            ("back".into(), json!(pair.back)),
        ],
    }
}

fn emit_section_units(
    section: &MarkdownSection,
    parent_ref: &str,
    doc_ref: &str,
    used_slugs: &mut HashMap<String, usize>,
    qa_counter: &mut usize,
    units: &mut Vec<ExtractedUnit>,
) {
    let base = slugify(&section.heading);
    let seen = used_slugs.get(&base).copied().unwrap_or(0);
    used_slugs.insert(base.clone(), seen + 1);
    let slug = if seen == 0 {
        base
    } else {
        format!("{base}-{}", seen + 1)
    };
    let reference = format!("{parent_ref}/h:{slug}");
    let (pairs, remaining) = extract_qa_pairs(&section.body);
    units.push(ExtractedUnit {
        reference: reference.clone(),
        kind: "section",
        title: Some(section.heading.clone()),
        content: (!remaining.is_empty()).then_some(remaining),
        parent_ref: Some(parent_ref.to_string()),
        metadata: vec![
            ("depth".into(), json!(section.depth)),
            ("slug".into(), json!(slug)),
        ],
    });
    for pair in &pairs {
        *qa_counter += 1;
        units.push(qa_unit(doc_ref, *qa_counter, pair, &reference));
    }
    for child in &section.children {
        emit_section_units(child, &reference, doc_ref, used_slugs, qa_counter, units);
    }
}

fn extract_document(
    doc: &MarkdownDocument,
    reference: &str,
    parent_ref: Option<&str>,
) -> Vec<ExtractedUnit> {
    let mut units = Vec::new();
    let doc_ref = format!("doc:{reference}");
    let title_body = doc
        .title_body
        .as_ref()
        .map(|(b, _)| b.as_str())
        .unwrap_or_default();
    if doc.preamble.is_empty()
        && title_body.is_empty()
        && doc.sections.is_empty()
        && !doc.title_from_heading
    {
        return units;
    }
    let (preamble_pairs, preamble_rest) = extract_qa_pairs(&doc.preamble);
    let (title_pairs, title_rest) = extract_qa_pairs(title_body);
    let root_content = [preamble_rest, title_rest]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    units.push(ExtractedUnit {
        reference: doc_ref.clone(),
        kind: "root",
        title: Some(doc.title.clone()),
        content: (!root_content.is_empty()).then_some(root_content),
        parent_ref: parent_ref.map(str::to_string),
        metadata: vec![("lineCount".into(), json!(doc.line_count))],
    });
    let mut qa_counter = 0usize;
    for pair in preamble_pairs.iter().chain(title_pairs.iter()) {
        qa_counter += 1;
        units.push(qa_unit(&doc_ref, qa_counter, pair, &doc_ref));
    }
    let mut used_slugs: HashMap<String, usize> = HashMap::new();
    for section in &doc.sections {
        emit_section_units(
            section,
            &doc_ref,
            &doc_ref,
            &mut used_slugs,
            &mut qa_counter,
            &mut units,
        );
    }
    units
}

fn extract_collection(sources: &[RawDocument]) -> Vec<ExtractedUnit> {
    let mut units: Vec<ExtractedUnit> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for source in sources {
        let parts: Vec<&str> = source
            .reference
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        let mut parent: Option<String> = None;
        for i in 0..dirs.len() {
            let path = dirs[..=i].join("/");
            let reference = format!("folder:{path}");
            if seen.insert(path.clone()) {
                units.push(ExtractedUnit {
                    reference: reference.clone(),
                    kind: "folder",
                    title: Some(dirs[i].to_string()),
                    content: None,
                    parent_ref: parent.clone(),
                    metadata: vec![("path".into(), json!(path))],
                });
            }
            parent = Some(reference);
        }
        let doc = parse_markdown(&source.raw, &source.reference);
        units.extend(extract_document(&doc, &source.reference, parent.as_deref()));
    }
    units
}

/// The units a source's documents extract to: a lone document through its adapter, several markdown files as a folder tree.
pub fn extract(adapter: AdapterKind, documents: &[RawDocument]) -> Extraction {
    if documents.len() > 1 && adapter == AdapterKind::Markdown {
        return Extraction {
            units: extract_collection(documents),
        };
    }
    let mut units = Vec::new();
    for doc in documents {
        match adapter {
            AdapterKind::Markdown => {
                let parsed = parse_markdown(&doc.raw, "document");
                units.extend(extract_document(&parsed, &doc.reference, None));
            }
            AdapterKind::Paste => {
                if !doc.raw.trim().is_empty() {
                    units.push(ExtractedUnit {
                        reference: format!("doc:{}", doc.reference),
                        kind: "root",
                        title: Some(doc.reference.clone()),
                        content: Some(doc.raw.clone()),
                        parent_ref: None,
                        metadata: vec![("lineCount".into(), json!(doc.raw.split('\n').count()))],
                    });
                }
            }
        }
    }
    Extraction { units }
}

// ---- mapping onto the knowledge target ----

pub struct Mapping {
    pub root: &'static str,
    pub folder: &'static str,
    pub section: &'static str,
    pub qa: &'static str,
}

/// `KNOWLEDGE_DETERMINISTIC_MAPPING`.
pub const KNOWLEDGE_MAPPING: Mapping = Mapping {
    root: "LearningGoal",
    folder: "Concept",
    section: "Concept",
    qa: "FlashCard",
};

/// Target entity -> generic slot -> target field.
pub fn field_for_slot(entity: &str, slot: &str) -> Option<&'static str> {
    match (entity, slot) {
        ("LearningGoal", "title") => Some("name"),
        ("LearningGoal", "content") => Some("description"),
        ("Concept", "title") => Some("name"),
        ("Concept", "content") => Some("lesson"),
        ("FlashCard", "title") | ("FlashCard", "front") => Some("front"),
        ("FlashCard", "content") | ("FlashCard", "back") => Some("back"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ImportUnit {
    pub reference: String,
    pub target_entity: String,
    pub fields: Map<String, Value>,
    pub parent_ref: Option<String>,
}

pub fn map_units(extracted: &[ExtractedUnit]) -> Vec<ImportUnit> {
    extracted
        .iter()
        .map(|unit| {
            let entity = match unit.kind {
                "root" => KNOWLEDGE_MAPPING.root,
                "folder" => KNOWLEDGE_MAPPING.folder,
                "section" => KNOWLEDGE_MAPPING.section,
                _ => KNOWLEDGE_MAPPING.qa,
            };
            let mut fields = Map::new();
            let mut slots: Vec<(String, Value)> = Vec::new();
            if let Some(t) = &unit.title {
                slots.push(("title".into(), json!(t)));
            }
            if let Some(c) = &unit.content {
                slots.push(("content".into(), json!(c)));
            }
            slots.extend(unit.metadata.iter().cloned());
            for (slot, value) in slots {
                if let Some(field) = field_for_slot(entity, &slot) {
                    fields.insert(field.to_string(), value);
                }
            }
            ImportUnit {
                reference: unit.reference.clone(),
                target_entity: entity.to_string(),
                fields,
                parent_ref: unit.parent_ref.clone(),
            }
        })
        .collect()
}

pub struct StagedRelationship {
    pub rel_type: &'static str,
    pub from_ref: String,
    pub to_ref: String,
}

pub fn hierarchy_relationships(units: &[ImportUnit]) -> Vec<StagedRelationship> {
    let by_ref: HashMap<&str, &ImportUnit> =
        units.iter().map(|u| (u.reference.as_str(), u)).collect();
    let mut out = Vec::new();
    for unit in units {
        let Some(parent) = unit.parent_ref.as_deref().and_then(|p| by_ref.get(p)) else {
            continue;
        };
        let mut candidates: Vec<&'static str> = MIGRATION_RELATIONSHIPS
            .iter()
            .filter(|(_, from, to)| *from == parent.target_entity && *to == unit.target_entity)
            .map(|(t, _, _)| *t)
            .collect();
        candidates.sort_unstable();
        if let Some(rel_type) = candidates.first() {
            out.push(StagedRelationship {
                rel_type,
                from_ref: parent.reference.clone(),
                to_ref: unit.reference.clone(),
            });
        }
    }
    out
}

// ---- validation ----

/// `(entity) -> [(field, required)]` of the knowledge target.
fn entity_fields(entity: &str) -> Option<&'static [(&'static str, bool)]> {
    match entity {
        "Concept" => Some(&[("name", true), ("description", false), ("lesson", false)]),
        "Lesson" => Some(&[("content", true)]),
        "FlashCard" => Some(&[("front", true), ("back", true)]),
        "LearningGoal" => Some(&[("name", true), ("description", false)]),
        _ => None,
    }
}

pub struct UnitError {
    pub reference: String,
    pub message: String,
}

pub fn validate_units(units: &[ImportUnit]) -> Vec<UnitError> {
    let known: HashSet<&str> = units.iter().map(|u| u.reference.as_str()).collect();
    let mut errors = Vec::new();
    for unit in units {
        let Some(fields) = entity_fields(&unit.target_entity) else {
            errors.push(UnitError {
                reference: unit.reference.clone(),
                message: format!("unknown target entity '{}'", unit.target_entity),
            });
            continue;
        };
        for (name, value) in &unit.fields {
            match fields.iter().find(|(n, _)| n == name) {
                None => errors.push(UnitError {
                    reference: unit.reference.clone(),
                    message: format!("unknown field '{name}' on entity '{}'", unit.target_entity),
                }),
                Some(_) => {
                    let problem = match value {
                        Value::Null => None,
                        Value::String(_) => None,
                        Value::Array(_) => Some("expected string, got array".to_string()),
                        Value::Number(_) => Some("expected string, got number".to_string()),
                        Value::Bool(_) => Some("expected string, got boolean".to_string()),
                        Value::Object(_) => Some("expected string, got object".to_string()),
                    };
                    if let Some(problem) = problem {
                        errors.push(UnitError {
                            reference: unit.reference.clone(),
                            message: format!("field '{name}': {problem}"),
                        });
                    } else if value.is_null()
                        && fields.iter().any(|(n, required)| n == name && *required)
                    {
                        errors.push(UnitError {
                            reference: unit.reference.clone(),
                            message: format!("field '{name}': value is null on a required field"),
                        });
                    }
                }
            }
        }
        for (name, required) in fields {
            if *required && !unit.fields.contains_key(*name) {
                errors.push(UnitError {
                    reference: unit.reference.clone(),
                    message: format!(
                        "missing required field '{name}' on entity '{}'",
                        unit.target_entity
                    ),
                });
            }
        }
        if let Some(parent) = &unit.parent_ref {
            if !known.contains(parent.as_str()) {
                errors.push(UnitError {
                    reference: unit.reference.clone(),
                    message: format!("parentRef '{parent}' does not match any staged unit"),
                });
            }
        }
    }
    errors
}

/// Parents before children, original order kept within a depth.
pub fn topo_order(units: &[ImportUnit]) -> Vec<&ImportUnit> {
    let by_ref: HashMap<&str, &ImportUnit> =
        units.iter().map(|u| (u.reference.as_str(), u)).collect();
    let depth_of = |unit: &ImportUnit| -> usize {
        let mut depth = 0;
        let mut current = unit;
        let mut seen: HashSet<&str> = HashSet::from([unit.reference.as_str()]);
        while let Some(parent) = current.parent_ref.as_deref().and_then(|p| by_ref.get(p)) {
            if !seen.insert(parent.reference.as_str()) {
                break;
            }
            depth += 1;
            current = parent;
        }
        depth
    };
    let mut ordered: Vec<&ImportUnit> = units.iter().collect();
    ordered.sort_by_key(|u| depth_of(u));
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(reference: &str, raw: &str) -> RawDocument {
        RawDocument {
            reference: reference.into(),
            raw: raw.into(),
        }
    }

    #[test]
    fn a_lone_h1_titles_the_document_and_its_sections_nest() {
        let units = extract(AdapterKind::Markdown, &[doc("a.md", "# Botany\nIntro.\n\n## Roots\nThey drink.\nQ:: Why?\nA:: To live.\n### Hairs\nTiny.")]).units;
        let refs: Vec<&str> = units.iter().map(|u| u.reference.as_str()).collect();
        assert_eq!(
            refs,
            [
                "doc:a.md",
                "doc:a.md/h:roots",
                "doc:a.md#qa:1",
                "doc:a.md/h:roots/h:hairs"
            ]
        );
        assert_eq!(units[0].title.as_deref(), Some("Botany"));
        assert_eq!(units[0].content.as_deref(), Some("Intro."));
        let qa = units.iter().find(|u| u.kind == "qa").unwrap();
        assert_eq!(qa.title.as_deref(), Some("Why?"));
        assert_eq!(qa.content.as_deref(), Some("To live."));
        assert_eq!(qa.parent_ref.as_deref(), Some("doc:a.md/h:roots"));
    }

    #[test]
    fn a_fence_hides_its_hash_lines_from_the_headings() {
        let units = extract(
            AdapterKind::Markdown,
            &[doc("a.md", "# T\n```\n# not a heading\n```\n## Real")],
        )
        .units;
        assert_eq!(units.iter().filter(|u| u.kind == "section").count(), 1);
    }

    #[test]
    fn several_files_nest_under_folder_units() {
        let units = extract(
            AdapterKind::Markdown,
            &[doc("notes/a.md", "# A"), doc("notes/b.md", "# B")],
        )
        .units;
        let folders: Vec<_> = units.iter().filter(|u| u.kind == "folder").collect();
        assert_eq!(folders.len(), 1);
        assert_eq!(
            units
                .iter()
                .find(|u| u.reference == "doc:notes/a.md")
                .unwrap()
                .parent_ref
                .as_deref(),
            Some("folder:notes")
        );
    }

    #[test]
    fn pasted_text_is_one_verbatim_root_and_blank_text_is_nothing() {
        let units = extract(AdapterKind::Paste, &[doc("note", "line 1\nline 2")]).units;
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].content.as_deref(), Some("line 1\nline 2"));
        assert!(extract(AdapterKind::Paste, &[doc("note", "  \n ")])
            .units
            .is_empty());
    }

    #[test]
    fn units_map_onto_the_knowledge_target_and_validate() {
        let units = map_units(
            &extract(
                AdapterKind::Markdown,
                &[doc(
                    "a.md",
                    "# Botany\n## Roots\nThey drink.\nQ:: Why?\nA:: To live.",
                )],
            )
            .units,
        );
        assert_eq!(units[0].target_entity, "LearningGoal");
        assert_eq!(units[1].fields["lesson"], json!("They drink."));
        assert!(validate_units(&units).is_empty());
        let rels = hierarchy_relationships(&units);
        assert_eq!(
            rels.iter().map(|r| r.rel_type).collect::<Vec<_>>(),
            ["containsConcept", "hasFlashCard"]
        );
    }
}
