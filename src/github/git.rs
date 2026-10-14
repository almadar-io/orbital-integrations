//! The git CLI actions: the twin of `github-git.ts`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};

use crate::error::{IntegrationError, SERVICE_ERROR, VALIDATION_ERROR};

pub fn process_cwd() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .into_owned()
}

fn service_error(message: String) -> IntegrationError {
    IntegrationError::coded(SERVICE_ERROR, message)
}

/// `execGit`: run `git` in `cwd`; a failure carries the arguments and stderr. `secret` never appears in the error.
fn exec_git(
    args: &[String],
    cwd: &str,
    env: &[(&str, String)],
    secret: Option<&str>,
) -> Result<String, IntegrationError> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    for (name, value) in env {
        command.env(name, value);
    }
    let hide = |text: String| match secret {
        Some(s) if !s.is_empty() => text.replace(s, "***"),
        _ => text,
    };
    let output = command
        .output()
        .map_err(|e| service_error(hide(format!("Failed to execute git: {e}"))))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(service_error(hide(format!(
            "Git command failed: {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ))))
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

fn text<'a>(params: &'a Map<String, Value>, key: &str) -> &'a str {
    params.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn truthy_text<'a>(params: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `injectTokenIntoUrl`.
fn inject_token(repo_url: &str, token: &str) -> Result<String, IntegrationError> {
    let mut url = url::Url::parse(repo_url).map_err(|_| IntegrationError::plain("Invalid URL"))?;
    let _ = url.set_username("x-access-token");
    let _ = url.set_password(Some(token));
    Ok(url.to_string())
}

pub fn clone_repo(params: &Map<String, Value>, token: &str) -> Result<(), IntegrationError> {
    let repo_url = text(params, "repoUrl");
    let target_dir = text(params, "targetDir");
    let depth = params.get("depth").and_then(Value::as_f64).unwrap_or(1.0);
    let auth_url = inject_token(repo_url, token)?;

    let mut args = strings(&["clone"]);
    if depth > 0.0 {
        args.push("--depth".to_string());
        args.push(format!("{}", depth as i64));
    }
    if let Some(branch) = truthy_text(params, "branch") {
        args.push("--branch".to_string());
        args.push(branch.to_string());
    }
    args.push(auth_url);
    args.push(target_dir.to_string());

    exec_git(&args, &process_cwd(), &[], Some(token))
        .map_err(|e| service_error(format!("Failed to clone repository: {}", e.message)))?;
    scrub_token_from_remote(target_dir, repo_url);
    Ok(())
}

/// `scrubTokenFromRemote`: a failure here is reported, not raised; the clone succeeded.
fn scrub_token_from_remote(work_dir: &str, original_url: &str) {
    let Ok(mut url) = url::Url::parse(original_url) else {
        return;
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let args = strings(&["remote", "set-url", "origin", url.as_str()]);
    if let Err(error) = exec_git(&args, work_dir, &[], None) {
        eprintln!("Warning: Failed to scrub token from remote URL: {error}");
    }
}

pub fn create_branch(params: &Map<String, Value>, work_dir: &str) -> Result<(), IntegrationError> {
    let branch_name = text(params, "branchName");
    let wrap =
        |e: IntegrationError| service_error(format!("Failed to create branch: {}", e.message));
    if let Some(base) = truthy_text(params, "baseBranch") {
        exec_git(&strings(&["checkout", base]), work_dir, &[], None).map_err(wrap)?;
    }
    exec_git(
        &strings(&["checkout", "-b", branch_name]),
        work_dir,
        &[],
        None,
    )
    .map_err(wrap)?;
    Ok(())
}

pub fn commit(params: &Map<String, Value>, work_dir: &str) -> Result<(), IntegrationError> {
    let message = text(params, "message");
    let wrap = |e: IntegrationError| service_error(format!("Failed to commit: {}", e.message));
    let mut add = strings(&["add"]);
    match params.get("files") {
        Some(Value::Array(files)) if !files.is_empty() => {
            for file in files {
                add.push(
                    file.as_str()
                        .map_or_else(|| file.to_string(), str::to_string),
                );
            }
        }
        _ => add.push(".".to_string()),
    }
    exec_git(&add, work_dir, &[], None).map_err(wrap)?;
    exec_git(&strings(&["commit", "-m", message]), work_dir, &[], None).map_err(wrap)?;
    Ok(())
}

const PROTECTED_BRANCHES: [&str; 4] = ["main", "master", "production", "prod"];

pub fn push(
    params: &Map<String, Value>,
    work_dir: &str,
    token: &str,
) -> Result<(), IntegrationError> {
    let branch_name = text(params, "branchName");
    let force = params
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if force && PROTECTED_BRANCHES.contains(&branch_name.to_lowercase().as_str()) {
        return Err(IntegrationError::coded(
            VALIDATION_ERROR,
            format!("Force push to protected branch '{branch_name}' is not allowed"),
        ));
    }
    let wrap = |e: IntegrationError| service_error(format!("Failed to push: {}", e.message));
    let helper = write_credential_helper(token).map_err(wrap)?;
    let mut args = strings(&["push"]);
    if force {
        args.push("--force".to_string());
    }
    args.extend(strings(&["-u", "origin", branch_name]));
    let outcome = exec_git(
        &args,
        work_dir,
        &[
            ("GIT_ASKPASS", helper.to_string_lossy().into_owned()),
            ("GIT_TERMINAL_PROMPT", "0".to_string()),
        ],
        Some(token),
    );
    let _ = std::fs::remove_file(&helper);
    outcome.map_err(wrap)?;
    Ok(())
}

/// A `GIT_ASKPASS` script that prints the token, readable only by the owner.
fn write_credential_helper(token: &str) -> Result<PathBuf, IntegrationError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = PathBuf::from(dir).join(format!("git-cred-{stamp}-{}.sh", std::process::id()));
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\n",
        token.replace('\'', "'\\''")
    );
    write_private(&path, &script).map_err(|e| service_error(e.to_string()))?;
    Ok(path)
}

#[cfg(unix)]
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(path)?;
    file.write_all(content.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}
