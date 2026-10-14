//! The workspace layout: paths, the sandbox, mint-time templates and the principal segment.

use std::path::{Component, Path, PathBuf};

use serde_json::to_string as json_string;

use crate::error::IntegrationError;

pub const ALMADAR_DIR: &str = ".almadar";
pub const ORBITALS_DIR: &str = "orbitals";
pub const SESSIONS_DIR: &str = ".almadar/sessions";
pub const COORDINATOR_DIR: &str = ".almadar/sessions/Coordinator";
pub const SCHEMA_FILE: &str = "schema.orb";
pub const APP_MARKER: &str = ".almadar/app-marker.json";
pub const USER_MEMORY: &str = ".almadar/user.orb";
pub const PROJECT_MEMORY: &str = ".almadar/project.orb";
pub const INDEX_MANIFEST: &str = ".almadar/index.json";
pub const LEGACY_ARCHIVE_DIR: &str = "orbitals/.archived";
pub const ORBITAL_SPEC_FILE: &str = "spec.json";

/// What the workspace itself owns at its root; anything else in a mint target is foreign.
pub const WORKSPACE_OWNED_ENTRIES: [&str; 5] =
    [".almadar", "orbitals", "schema.orb", ".git", ".gitignore"];

/// The app's program sources in git: schema.orb, the files directly in orbitals/, every `.lolo` source, and
/// each orbital's recorded spec.
pub const APP_PATHSPECS: [&str; 7] = [
    "schema.orb",
    ":(glob)orbitals/*",
    ":(glob)**/*.lolo",
    ":(glob).almadar/sessions/*/spec.json",
    ":(exclude,glob).almadar/**/*.lolo",
    ":(exclude)apps",
    ":(exclude)node_modules",
];

const GITIGNORE_LINES: [&str; 18] = [
    "# Almadar — version only the app; ignore generated/churny state",
    "# `.almadar/` is an allowlist: its app state is history, anything else there is scratch.",
    "/.almadar/*",
    "!/.almadar/app-marker.json",
    "!/.almadar/ledger.json",
    "!/.almadar/project.orb",
    "!/.almadar/user.orb",
    "!/.almadar/fill-candidates/",
    "# A session is churn except each orbital's spec.json: its .orb is derived from it.",
    "!/.almadar/sessions/",
    "/.almadar/sessions/*",
    "!/.almadar/sessions/*/",
    "/.almadar/sessions/*/**",
    "!/.almadar/sessions/*/spec.json",
    "/apps/",
    "/.almadar-archive.bundle",
    "",
    "",
];

/// The workspace-owned `.gitignore`.
pub fn gitignore() -> String {
    let mut lines = GITIGNORE_LINES.to_vec();
    lines.pop();
    lines.join("\n")
}

/// A principal's directory name: letters, digits and `-` kept, every other UTF-8 byte written as `_` plus two
/// lowercase hex digits.
pub fn workspace_owner_segment(principal: &str) -> String {
    let mut out = String::new();
    for byte in principal.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("_{byte:02x}"));
        }
    }
    out
}

/// A user-supplied relative path resolved inside `work_dir`; traversal and absolute paths are refused.
pub fn sandboxed_path(work_dir: &Path, rel: &str) -> Result<PathBuf, IntegrationError> {
    if rel.is_empty() {
        return Err(IntegrationError::plain("sandbox: empty relative path"));
    }
    if rel.starts_with('/') {
        return Err(IntegrationError::plain(format!(
            "sandbox: absolute paths are not allowed ({rel})"
        )));
    }
    let bytes = rel.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return Err(IntegrationError::plain(format!(
            "sandbox: drive-letter paths are not allowed ({rel})"
        )));
    }
    let root = normalize(work_dir);
    let target = normalize(&root.join(rel));
    if target != root && !target.starts_with(&root) {
        return Err(IntegrationError::plain(format!(
            "sandbox: path escapes workspace ({rel})"
        )));
    }
    Ok(target)
}

/// Lexical normalization of an absolute path (`.`, `..` and repeated separators resolved).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn js(s: &str) -> String {
    json_string(s).unwrap_or_else(|_| String::from("\"\""))
}

/// `schema.orb` as a fresh workspace has it.
pub fn schema_template(name: &str) -> String {
    format!(
        "{{\n  \"name\": {},\n  \"version\": \"1.0.0\",\n  \"description\": \"\",\n  \"orbitals\": []\n}}",
        js(name)
    )
}

/// `.almadar/user.orb`.
pub fn user_template(user_id: &str) -> String {
    format!(
        "{{\n  \"name\": \"UserMemory\",\n  \"version\": \"1.0.0\",\n  \"description\": \"User preferences and learned patterns\",\n  \"orbitals\": [\n    {{\n      \"name\": \"PreferenceTracking\",\n      \"entity\": {{\n        \"name\": \"UserPreference\",\n        \"fields\": [],\n        \"instances\": [\n          {{\n            \"id\": {},\n            \"userId\": {}\n          }}\n        ]\n      }},\n      \"traits\": [],\n      \"pages\": []\n    }}\n  ]\n}}",
        js(&format!("pref-{user_id}")),
        js(user_id)
    )
}

/// `.almadar/project.orb`.
pub fn project_template(project_name: &str, app_id: Option<&str>, updated_at_iso: &str) -> String {
    let id = app_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("project-{project_name}"));
    format!(
        "{{\n  \"name\": \"ProjectMemory\",\n  \"version\": \"1.0.0\",\n  \"description\": {},\n  \"orbitals\": [\n    {{\n      \"name\": \"DomainKnowledge\",\n      \"entity\": {{\n        \"name\": \"ProjectContext\",\n        \"fields\": [],\n        \"instances\": [\n          {{\n            \"id\": {},\n            \"appId\": {},\n            \"projectName\": {},\n            \"lastUpdatedAt\": {}\n          }}\n        ]\n      }},\n      \"traits\": [],\n      \"pages\": []\n    }}\n  ]\n}}",
        js(&format!("Domain knowledge for {project_name}")),
        js(&id),
        js(app_id.unwrap_or("")),
        js(project_name),
        js(updated_at_iso)
    )
}

/// `.almadar/index.json` of a workspace with no orbitals.
pub fn empty_index_manifest(baked_at_ms: u64) -> String {
    format!(
        "{{\n  \"schemaVersion\": 2,\n  \"bm25\": {{\n    \"documents\": {{}},\n    \"docFreq\": {{}},\n    \"docCount\": 0,\n    \"avgDocLen\": 0\n  }},\n  \"intent\": {{\n    \"entityNameOverrides\": {{}},\n    \"traitRefImporters\": {{}},\n    \"ruleCapabilities\": {{}},\n    \"organism\": {{}}\n  }},\n  \"composed\": {{\n    \"events\": {{}},\n    \"entityNameComposed\": {{}}\n  }},\n  \"recency\": {{}},\n  \"bakedAt\": {baked_at_ms}\n}}"
    )
}

/// `.almadar/app-marker.json`.
pub fn app_marker(app_id: &str, user_id: &str, created_at_ms: u64) -> String {
    format!(
        "{{\n  \"appId\": {},\n  \"userId\": {},\n  \"createdAt\": {created_at_ms}\n}}",
        js(app_id),
        js(user_id)
    )
}

/// A number in base 36, as `Number.prototype.toString(36)` writes a whole number.
pub fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(std::char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
        n /= 36;
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_segment_keeps_alphanumerics_and_dash_and_escapes_every_other_byte() {
        assert_eq!(workspace_owner_segment("user-1"), "user-1");
        assert_eq!(workspace_owner_segment("a_b@c"), "a_5fb_40c");
        assert_eq!(workspace_owner_segment("é"), "_c3_a9");
    }

    #[test]
    fn control_two_principals_never_share_a_segment() {
        assert_ne!(
            workspace_owner_segment("a_b"),
            workspace_owner_segment("a-b")
        );
        assert_ne!(
            workspace_owner_segment("a/b"),
            workspace_owner_segment("a_2fb")
        );
    }

    #[test]
    fn the_sandbox_refuses_escapes_absolute_and_empty_paths() {
        let root = Path::new("/w/session");
        assert_eq!(
            sandboxed_path(root, "../x").unwrap_err().message,
            "sandbox: path escapes workspace (../x)"
        );
        assert_eq!(
            sandboxed_path(root, "/etc/passwd").unwrap_err().message,
            "sandbox: absolute paths are not allowed (/etc/passwd)"
        );
        assert_eq!(
            sandboxed_path(root, "C:\\x").unwrap_err().message,
            "sandbox: drive-letter paths are not allowed (C:\\x)"
        );
        assert_eq!(
            sandboxed_path(root, "").unwrap_err().message,
            "sandbox: empty relative path"
        );
    }

    #[test]
    fn control_the_sandbox_resolves_inner_dots() {
        let root = Path::new("/w/session");
        assert_eq!(
            sandboxed_path(root, "a/../b//c.lolo").unwrap(),
            Path::new("/w/session/b/c.lolo")
        );
        assert_eq!(sandboxed_path(root, ".").unwrap(), root);
        assert!(sandboxed_path(root, "../session/x").is_ok());
    }

    #[test]
    fn base36_matches_js() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(1_767_225_600_000), "mjuohs00");
    }

    #[test]
    fn the_gitignore_ends_with_one_newline() {
        let text = gitignore();
        assert!(text.ends_with("/.almadar-archive.bundle\n"));
        assert!(!text.ends_with("\n\n"));
    }
}
