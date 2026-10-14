//! The prompts and checks behind a concept's persona (`personaLogic.ts`).

use regex::Regex;

use crate::uricomponent::encode_uri_component;

pub const PERSONA_SYSTEM_PROMPT: &str = "You identify the single most historically credited HUMAN originator, inventor, or creator of a \
learning concept, then greet a learner in their voice. The concept may be a programming language, \
framework, library, algorithm, data structure, mathematical/scientific concept, theory, or technique. \
Use the concept name alone to determine the DISCIPLINE and disambiguate it. \
Reply ONLY with compact JSON: \
{\"name\":<a SPECIFIC REAL HUMAN BEING's full canonical name as it would title a Wikipedia article, \
e.g. \"Isaac Newton\", \"Brendan Eich\">,\
\"description\":<one sentence, max ~20 words: who they are + key contribution>,\
\"greeting\":<one warm first-person sentence, in character, mentioning the concept>}. \
HARD RULES: \"name\" MUST be a human being. It must NEVER be the concept name itself, nor an animal, \
object, place, or product. If the concept is an invention, name its inventor/creator. If no single \
person is credited, name the most iconic human contributor or the field's founder. \
Never invent a fictional person.";

/// A concept id leaked through as the label; the model cannot identify it.
pub fn looks_like_id(label: &str) -> bool {
    let uuid = Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-").ok();
    uuid.is_some_and(|re| re.is_match(label))
        || (!label.is_empty() && label.chars().all(|c| c.is_ascii_digit()))
}

fn letters_and_digits(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// The model echoed the concept word back as the persona name.
pub fn is_degenerate(name: &str, concept_label: &str) -> bool {
    let n = letters_and_digits(name);
    !n.is_empty() && n == letters_and_digits(concept_label)
}

pub fn persona_user_prompt(
    concept_label: &str,
    correction: Option<&str>,
    context: Option<&str>,
) -> String {
    let base = match context {
        Some(c) if !c.is_empty() => {
            format!("Concept: {concept_label}\nThe learner is studying: {c}")
        }
        _ => format!("Concept: {concept_label}"),
    };
    match correction {
        Some(c) if !c.is_empty() => format!(
            "{base}\n\nIMPORTANT: \"{c}\" is the concept itself, not a person. You MUST name the real human being who created or pioneered \"{concept_label}\"."
        ),
        _ => base,
    }
}

/// Lossless Firestore document id for a concept label and optional subject.
pub fn persona_cache_key(concept_label: &str, context: Option<&str>) -> String {
    let label = format!(
        "c:{}",
        encode_uri_component(&concept_label.trim().to_lowercase())
    );
    match context {
        Some(c) if !c.trim().is_empty() => {
            format!("{label}|{}", encode_uri_component(&c.trim().to_lowercase()))
        }
        _ => label,
    }
}

pub fn tutor_persona(concept_label: &str) -> (String, String) {
    (
        format!("{concept_label} Tutor"),
        format!("Your AI tutor for {concept_label}."),
    )
}

/// The system prompt for an in-character chat: grounded in the stored biography, scoped to the concept.
pub fn persona_instructions(
    name: &str,
    description: &str,
    bio: &str,
    concept_label: &str,
    context: Option<&str>,
) -> String {
    let subject = match context {
        Some(c) if !c.is_empty() => format!(" The learner's wider subject is \"{c}\"."),
        _ => String::new(),
    };
    let bio_text = if bio.is_empty() {
        "(no biography available)"
    } else {
        bio
    };
    format!(
        "You are {name}. {description}\n\n\
Your biography (factual grounding, from Wikipedia):\n{bio_text}\n\n\
A learner is studying \"{concept_label}\".{subject} Reply in first person, in character as {name}, \
warmly and concisely (max ~120 words). Draw on your real life and contributions above. \
Stay factual to the biography; if asked something outside it, say plainly that you do not know rather than invent. \
Keep the conversation on \"{concept_label}\" and bring the learner back to it if they drift. \
Do not break character."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_recognised_and_words_are_not() {
        assert!(looks_like_id("123e4567-e89b-12d3-a456-426614174000"));
        assert!(looks_like_id("42"));
        assert!(!looks_like_id("Calculus"));
        assert!(!looks_like_id(""));
    }

    #[test]
    fn an_echoed_concept_is_degenerate_regardless_of_case_and_punctuation() {
        assert!(is_degenerate("Rust!", "rust"));
        assert!(!is_degenerate("Graydon Hoare", "rust"));
        assert!(!is_degenerate("", ""));
    }

    #[test]
    fn the_cache_key_keeps_label_and_subject_apart() {
        assert_eq!(persona_cache_key(" Rust ", None), "c:rust");
        assert_eq!(persona_cache_key("Rust", Some("  ")), "c:rust");
        assert_eq!(
            persona_cache_key("Rust", Some("Languages")),
            "c:rust|languages"
        );
    }
}
