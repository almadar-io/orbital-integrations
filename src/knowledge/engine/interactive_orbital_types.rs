//! Interactive-orbital type taxonomy: the SDK routing allowlist (twin of `engine/interactiveOrbitalTypes.ts`).

pub const INTERACTIVE_ORBITAL_TYPES: [&str; 16] = [
    "algorithms",
    "astronomy",
    "biology",
    "chemistry",
    "civics",
    "earth",
    "economics",
    "geography",
    "history",
    "linguistics",
    "logic",
    "math",
    "physics",
    "probability",
    "psychology",
    "sociology",
];

pub fn is_interactive_orbital_type(name: &str) -> bool {
    INTERACTIVE_ORBITAL_TYPES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_sorted_and_unique() {
        let mut sorted = INTERACTIVE_ORBITAL_TYPES;
        sorted.sort_unstable();
        assert_eq!(sorted, INTERACTIVE_ORBITAL_TYPES);
        let mut dedup = sorted.to_vec();
        dedup.dedup();
        assert_eq!(dedup.len(), INTERACTIVE_ORBITAL_TYPES.len());
    }

    #[test]
    fn membership() {
        assert!(is_interactive_orbital_type("physics"));
        assert!(!is_interactive_orbital_type("cooking"));
    }
}
