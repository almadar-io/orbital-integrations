//! Sectioned prompts (`PromptBuilder.ts`): titled sections rendered into the system or the user prompt, with
//! `{{key}}` placeholders filled from a context.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    System,
    User,
}

struct Section {
    title: Option<String>,
    content: String,
    channel: Channel,
}

#[derive(Default)]
pub struct PromptBuilder {
    sections: Vec<Section>,
    context: Vec<(String, String)>,
}

/// JS `String.replace(/{{key}}/g, value)`: the replacement honours `$$`, `$&`, `` $` `` and `$'`.
fn js_replace_all(text: &str, token: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut consumed = 0usize;
    while let Some(at) = rest.find(token) {
        out.push_str(&rest[..at]);
        let before = &text[..consumed + at];
        let after = &text[consumed + at + token.len()..];
        let mut chars = replacement.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                out.push(c);
                continue;
            }
            match chars.peek() {
                Some('$') => {
                    chars.next();
                    out.push('$');
                }
                Some('&') => {
                    chars.next();
                    out.push_str(token);
                }
                Some('`') => {
                    chars.next();
                    out.push_str(before);
                }
                Some('\'') => {
                    chars.next();
                    out.push_str(after);
                }
                _ => out.push('$'),
            }
        }
        consumed += at + token.len();
        rest = &rest[at + token.len()..];
    }
    out.push_str(rest);
    out
}

impl PromptBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn section(mut self, title: &str, content: &str, channel: Channel) -> Self {
        self.sections.push(Section {
            title: Some(title.to_string()),
            content: content.to_string(),
            channel,
        });
        self
    }

    /// A user-channel section.
    pub fn user_section(self, title: &str, content: &str) -> Self {
        self.section(title, content, Channel::User)
    }

    pub fn with_context(mut self, entries: &[(&str, String)]) -> Self {
        for (key, value) in entries {
            match self.context.iter_mut().find(|(k, _)| k == key) {
                Some(entry) => entry.1 = value.clone(),
                None => self.context.push(((*key).to_string(), value.clone())),
            }
        }
        self
    }

    pub fn list(self, title: &str, items: &[String]) -> Self {
        if items.is_empty() {
            return self;
        }
        let content = items
            .iter()
            .enumerate()
            .map(|(i, item)| format!("{}. {item}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        self.user_section(title, &content)
    }

    pub fn rules(self, items: &[String]) -> Self {
        let content = items
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.user_section("Rules", &content)
    }

    fn interpolate(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (key, value) in &self.context {
            result = js_replace_all(&result, &format!("{{{{{key}}}}}"), value);
        }
        result
    }

    fn render(&self, channel: Channel) -> String {
        self.sections
            .iter()
            .filter(|s| s.channel == channel)
            .map(|s| {
                let head = s
                    .title
                    .as_ref()
                    .map(|t| format!("## {t}\n"))
                    .unwrap_or_default();
                format!("{head}{}", self.interpolate(&s.content))
            })
            .collect::<Vec<_>>()
            .join("\n\n")
            .trim()
            .to_string()
    }

    pub fn build_system(&self) -> String {
        self.render(Channel::System)
    }

    pub fn build_user(&self) -> String {
        self.render(Channel::User)
    }
}

pub const MERMAID_SECTION_TITLE: &str = "Diagrams (Mermaid)";

pub const MERMAID_DIAGRAM_RULES: &str = "```mermaid fenced blocks are rendered as live diagrams. Use one only when a diagram carries structure prose cannot, and follow this syntax exactly:
- Put the diagram type on the first line (`graph TD`, `sequenceDiagram`, `stateDiagram-v2`, `flowchart LR`, …) and close every `subgraph` with `end`.
- ALWAYS wrap node label text in double quotes: `B[\"Scripting Engine (Vimscript)\"]`, never `B[Scripting Engine (Vimscript)]`. An unquoted label is a parse error the moment it contains `(`, `)`, `[`, `]`, `{`, `}`, `|`, `#`, `;` or a quote, and the whole diagram fails to render.
- Quote edge labels the same way: `A -->|\"calls (sync)\"| B`.
- Quote `subgraph` titles: `subgraph \"Vim Core (Monolithic)\"`.
- Keep labels plain text — no HTML, no `<br>`, no markdown emphasis inside a label.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_render_by_channel_with_context_filled_in() {
        let b = PromptBuilder::new()
            .with_context(&[("name", "Light".to_string())])
            .section("Role", "You teach {{name}}.", Channel::System)
            .user_section("Task", "Explain {{name}}.")
            .list("Steps", &["a".to_string(), "b".to_string()]);
        assert_eq!(b.build_system(), "## Role\nYou teach Light.");
        assert_eq!(
            b.build_user(),
            "## Task\nExplain Light.\n\n## Steps\n1. a\n2. b"
        );
    }

    #[test]
    fn a_dollar_in_a_value_follows_js_replacement_rules() {
        assert_eq!(
            js_replace_all("x {{a}} y", "{{a}}", "$$5 and $&"),
            "x $5 and {{a}} y"
        );
        assert_eq!(js_replace_all("x {{a}} y", "{{a}}", "$1"), "x $1 y");
    }

    #[test]
    fn an_empty_list_adds_no_section() {
        assert_eq!(PromptBuilder::new().list("Steps", &[]).build_user(), "");
    }
}
