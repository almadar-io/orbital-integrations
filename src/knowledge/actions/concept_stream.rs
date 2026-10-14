//! Reads concept names out of an expansion as each `<concept>name</concept>` closes (`conceptStream.ts`).

const OPEN: &str = "<concept>";
const CLOSE: &str = "</concept>";

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn index_of_tag(text: &[char], tag: &str, from: usize) -> Option<usize> {
    let tag_chars: Vec<char> = tag.chars().collect();
    let mut i = from;
    while i + tag_chars.len() <= text.len() {
        let window: String = text[i..i + tag_chars.len()]
            .iter()
            .collect::<String>()
            .to_lowercase();
        if window == tag {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedConcept {
    pub name: String,
    /// The offset (in characters) just past `</concept>` in the whole text.
    pub end: usize,
}

/// Tags are case-insensitive, a name never spans a line break, an empty name is skipped, an unclosed tag emits nothing.
#[derive(Default)]
pub struct ConceptScanner {
    pending: Vec<char>,
    consumed: usize,
}

impl ConceptScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &str) -> Vec<ScannedConcept> {
        self.pending.extend(chunk.chars());
        let mut names = Vec::new();
        let mut from = 0usize;
        loop {
            let Some(open) = index_of_tag(&self.pending, OPEN, from) else {
                from = from.max(self.pending.len().saturating_sub(OPEN.chars().count() - 1));
                break;
            };
            let name_start = open + OPEN.chars().count();
            let Some(close) = index_of_tag(&self.pending, CLOSE, name_start) else {
                from = open;
                break;
            };
            let raw: String = self.pending[name_start..close].iter().collect();
            if raw.chars().any(is_line_terminator) {
                from = name_start;
                continue;
            }
            let name = raw.trim().to_string();
            from = close + CLOSE.chars().count();
            if !name.is_empty() {
                names.push(ScannedConcept {
                    name,
                    end: self.consumed + from,
                });
            }
        }
        self.consumed += from;
        self.pending.drain(..from);
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_emit_as_their_tags_close_across_chunks() {
        let mut scanner = ConceptScanner::new();
        assert!(scanner.feed("intro <conce").is_empty());
        assert!(scanner.feed("pt>Heat Tra").is_empty());
        let names = scanner.feed("nsfer</concept> and <CONCEPT> Entropy </Concept>");
        assert_eq!(
            names.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            ["Heat Transfer", "Entropy"]
        );
    }

    #[test]
    fn control_a_name_spanning_a_line_break_or_empty_is_skipped() {
        let mut scanner = ConceptScanner::new();
        assert!(scanner
            .feed("<concept>a\nb</concept><concept>  </concept>")
            .is_empty());
    }

    #[test]
    fn end_is_the_offset_past_the_closing_tag_in_the_whole_text() {
        let mut scanner = ConceptScanner::new();
        let text = "xx<concept>A</concept>";
        let names = scanner.feed(text);
        assert_eq!(names[0].end, text.chars().count());
    }
}
