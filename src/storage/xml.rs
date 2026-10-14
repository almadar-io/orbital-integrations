//! The small slice of XML S3 answers with: element text by path, repeated elements, the five predefined
//! entities and numeric character references. No namespaces, no CDATA, no attributes that matter.

#[derive(Debug, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Parse one document into its root element; `None` when the text holds no well-formed element.
pub fn parse(source: &str) -> Option<Element> {
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let mut rest = source;
    while !rest.is_empty() {
        match rest.find('<') {
            None => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&unescape(rest));
                }
                break;
            }
            Some(at) => {
                if at > 0 {
                    if let Some(top) = stack.last_mut() {
                        top.text.push_str(&unescape(&rest[..at]));
                    }
                }
                rest = &rest[at..];
            }
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            let end = rest.find('>')?;
            rest = &rest[end + 1..];
            continue;
        }
        let end = rest.find('>')?;
        let inner = &rest[1..end];
        rest = &rest[end + 1..];
        if let Some(closing) = inner.strip_prefix('/') {
            let done = stack.pop()?;
            if done.name != closing.trim() {
                return None;
            }
            match stack.last_mut() {
                Some(parent) => parent.children.push(done),
                None => root = Some(done),
            }
            continue;
        }
        let self_closing = inner.ends_with('/');
        let head = inner.trim_end_matches('/');
        let name = head.split_whitespace().next()?.to_string();
        let element = Element {
            name,
            text: String::new(),
            children: Vec::new(),
        };
        if self_closing {
            match stack.last_mut() {
                Some(parent) => parent.children.push(element),
                None => root = Some(element),
            }
        } else {
            stack.push(element);
        }
    }
    if stack.is_empty() {
        root
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nested_text_and_repeats() {
        let doc = parse("<?xml version=\"1.0\"?><R xmlns=\"x\"><A>1</A><B><C>c &amp; d</C></B><B><C>&#x41;&#66;</C></B><E/></R>").expect("doc");
        assert_eq!(doc.name, "R");
        assert_eq!(doc.text_of("A"), Some("1"));
        let texts: Vec<&str> = doc
            .children_named("B")
            .filter_map(|b| b.text_of("C"))
            .collect();
        assert_eq!(texts, vec!["c & d", "AB"]);
        assert_eq!(doc.child("E").map(|e| e.text.as_str()), Some(""));
    }

    #[test]
    fn rejects_text_without_an_element_and_unbalanced_tags() {
        assert_eq!(parse("bad gateway"), None);
        assert_eq!(parse("<A><B></A>"), None);
        assert_eq!(parse("<A>"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn leaves_an_unknown_entity_alone() {
        let doc = parse("<A>x &bogus; y</A>").expect("doc");
        assert_eq!(doc.text, "x &bogus; y");
    }
}
