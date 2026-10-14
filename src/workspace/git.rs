//! A thin client over the local `git` CLI, as `@almadar/workspace` drives it (`internal/git-client.ts`): the
//! history of a workspace is a real git repository, and the bucket's history objects are `git bundle` files,
//! so only git itself can read and write them byte-compatibly with the TS host.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::clock::Clock;
use crate::error::IntegrationError;

#[derive(Debug, Clone)]
pub struct GitHistoryEntry {
    pub oid: String,
    pub timestamp_ms: u64,
    pub subject: String,
    pub trailers: Vec<(String, String)>,
}

impl GitHistoryEntry {
    pub fn trailer(&self, key: &str) -> Option<&str> {
        self.trailers
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPathChange {
    pub status: char,
    pub path: String,
}

pub struct GitClient {
    cwd: PathBuf,
    clock: Clock,
}

fn git_binary() -> String {
    std::env::var("ALMADAR_GIT_BINARY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "git".to_string())
}

fn run(
    args: &[String],
    cwd: &Path,
    ceiling: Option<&Path>,
    clock: &Clock,
) -> Result<String, IntegrationError> {
    let mut command = Command::new(git_binary());
    command.args(args).current_dir(cwd);
    if let Some(parent) = ceiling {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    if clock.is_pinned() {
        let when = clock.iso();
        command.env("GIT_AUTHOR_DATE", &when);
        command.env("GIT_COMMITTER_DATE", &when);
    }
    let output = command.output().map_err(|e| {
        IntegrationError::plain(format!(
            "git {}: {e}",
            args.first().map_or("", String::as_str)
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exited with {}", output.status)
        };
        return Err(IntegrationError::plain(format!(
            "git {}: {message}",
            args.first().map_or("", String::as_str)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn with_paths(head: &[&str], paths: &[String]) -> Vec<String> {
    let mut args = owned(head);
    args.push("--".into());
    args.extend(paths.iter().cloned());
    args
}

fn parse_name_status(out: &str) -> Vec<GitPathChange> {
    let mut changes = Vec::new();
    for line in out.split('\n') {
        let mut parts = line.splitn(2, '\t');
        let code = parts.next().unwrap_or("");
        let Some(rel) = parts.next().filter(|r| !r.is_empty()) else {
            continue;
        };
        let rel = rel.split('\t').next().unwrap_or(rel);
        let status = match code {
            "A" => 'A',
            "D" => 'D',
            _ => 'M',
        };
        changes.push(GitPathChange {
            status,
            path: rel.to_string(),
        });
    }
    changes
}

impl GitClient {
    pub fn new(cwd: &Path, clock: Clock) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            clock,
        }
    }

    fn exec(&self, args: Vec<String>) -> Result<String, IntegrationError> {
        let resolved = super::layout::normalize(&self.cwd);
        run(&args, &self.cwd, resolved.parent(), &self.clock)
    }

    fn exec_str(&self, args: &[&str]) -> Result<String, IntegrationError> {
        self.exec(owned(args))
    }

    pub fn is_repo(&self) -> bool {
        self.cwd.join(".git").exists()
    }

    pub fn init(&self) -> Result<(), IntegrationError> {
        if self.is_repo() {
            return Ok(());
        }
        self.exec_str(&["init"])?;
        self.ensure_identity()
    }

    pub fn ensure_identity(&self) -> Result<(), IntegrationError> {
        self.exec_str(&["config", "user.email", "agent@almadar.io"])?;
        self.exec_str(&["config", "user.name", "Almadar Agent"])?;
        Ok(())
    }

    pub fn add_all(&self) -> Result<(), IntegrationError> {
        self.exec_str(&["add", "-A"]).map(|_| ())
    }

    pub fn add_paths(&self, pathspecs: &[String]) -> Result<(), IntegrationError> {
        match self.exec(with_paths(&["add"], pathspecs)) {
            Err(e) if e.message.contains("did not match any files") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    pub fn untrack(&self, pathspecs: &[String]) -> Result<(), IntegrationError> {
        self.exec(with_paths(
            &["rm", "-r", "--cached", "--quiet", "--ignore-unmatch"],
            pathspecs,
        ))
        .map(|_| ())
    }

    /// Commit the index; `None` when nothing is staged.
    pub fn commit(
        &self,
        message: &str,
        trailers: &[(&str, String)],
    ) -> Result<Option<String>, IntegrationError> {
        let mut args = owned(&["commit", "-m", message, "--allow-empty-message"]);
        if !trailers.is_empty() {
            args.push("-m".into());
            args.push(
                trailers
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        match self.exec(args) {
            Ok(_) => {}
            Err(e) if nothing_to_commit(&e.message) => return Ok(None),
            Err(e) => return Err(e),
        }
        self.head_sha()
    }

    pub fn head_sha(&self) -> Result<Option<String>, IntegrationError> {
        Ok(self
            .exec_str(&["rev-parse", "HEAD"])
            .ok()
            .map(|out| out.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    /// Commit history of `pathspecs` (newest first) with each commit's trailers; empty before the first commit.
    pub fn history(&self, pathspecs: &[String], limit: Option<&str>) -> Vec<GitHistoryEntry> {
        let mut args = owned(&[
            "log",
            "--format=%H%x00%at%x00%s%x00%(trailers:only,unfold)%x1e",
        ]);
        if let Some(n) = limit {
            args.push("-n".into());
            args.push(n.to_string());
        }
        args.push("--".into());
        args.extend(pathspecs.iter().cloned());
        let Ok(out) = self.exec(args) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for record in out.split('\u{1e}') {
            let record = record.trim_start_matches('\n');
            let mut fields = record.split('\u{0}');
            let oid = fields.next().unwrap_or("");
            let ts = fields.next().unwrap_or("");
            let subject = fields.next().unwrap_or("");
            let trailer_block = fields.next().unwrap_or("");
            if oid.is_empty() || ts.is_empty() {
                continue;
            }
            let mut trailers = Vec::new();
            for line in trailer_block.split('\n') {
                if let Some(i) = line.find(": ") {
                    if i > 0 {
                        trailers.push((line[..i].to_string(), line[i + 2..].trim().to_string()));
                    }
                }
            }
            entries.push(GitHistoryEntry {
                oid: oid.to_string(),
                timestamp_ms: ts.parse::<u64>().unwrap_or(0) * 1000,
                subject: subject.to_string(),
                trailers,
            });
        }
        entries
    }

    /// Content of `filepath` at commit `oid`; `None` when absent.
    pub fn show(&self, oid: &str, filepath: &str) -> Option<String> {
        self.exec(owned(&["show", &format!("{oid}:{filepath}")]))
            .ok()
    }

    pub fn changed_files(
        &self,
        oid: &str,
        pathspecs: &[String],
    ) -> Result<Vec<String>, IntegrationError> {
        let mut args = owned(&[
            "diff-tree",
            "--root",
            "-r",
            "--name-only",
            "--no-commit-id",
            "--no-renames",
            oid,
        ]);
        args.push("--".into());
        args.extend(pathspecs.iter().cloned());
        Ok(lines(&self.exec(args)?))
    }

    pub fn tracked_files(&self, pathspecs: &[String]) -> Result<Vec<String>, IntegrationError> {
        Ok(lines(&self.exec(with_paths(&["ls-files"], pathspecs))?))
    }

    pub fn diff_from(
        &self,
        oid: &str,
        pathspecs: &[String],
    ) -> Result<Vec<GitPathChange>, IntegrationError> {
        Ok(parse_name_status(&self.exec(with_paths(
            &["diff", "--name-status", "--no-renames", oid],
            pathspecs,
        ))?))
    }

    pub fn restore_paths(&self, oid: &str, paths: &[String]) -> Result<(), IntegrationError> {
        if paths.is_empty() {
            return Ok(());
        }
        let source = format!("--source={oid}");
        self.exec(with_paths(
            &["restore", &source, "--staged", "--worktree"],
            paths,
        ))
        .map(|_| ())
    }

    pub fn remove_paths(&self, paths: &[String]) -> Result<(), IntegrationError> {
        if paths.is_empty() {
            return Ok(());
        }
        self.exec(with_paths(
            &["rm", "-r", "-q", "-f", "--ignore-unmatch"],
            paths,
        ))
        .map(|_| ())
    }

    pub fn is_commit(&self, oid: &str) -> bool {
        self.exec(owned(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{oid}^{{commit}}"),
        ]))
        .is_ok()
    }

    pub fn parent_of(&self, oid: &str) -> Option<String> {
        self.exec(owned(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{oid}^"),
        ]))
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|s| !s.is_empty())
    }

    pub fn ref_sha(&self, reference: &str) -> Option<String> {
        self.exec(owned(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ]))
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|s| !s.is_empty())
    }

    pub fn update_ref(&self, reference: &str, sha: &str) -> Result<(), IntegrationError> {
        self.exec_str(&["update-ref", reference, sha]).map(|_| ())
    }

    pub fn get_config(&self, key: &str) -> Option<String> {
        self.exec_str(&["config", "--get", key])
            .ok()
            .map(|out| out.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn set_config(&self, key: &str, value: &str) -> Result<(), IntegrationError> {
        self.exec_str(&["config", key, value]).map(|_| ())
    }

    /// Bundle the history not reachable from `since` (every ref when `None`); `false` when nothing is new.
    pub fn bundle_since(
        &self,
        out_path: &Path,
        since: Option<&str>,
    ) -> Result<bool, IntegrationError> {
        let out = out_path.to_string_lossy().into_owned();
        match since {
            Some(since) => {
                let exclude = format!("^{since}");
                let fresh = self.exec(owned(&[
                    "rev-list",
                    "--count",
                    "--branches",
                    "--tags",
                    &exclude,
                ]))?;
                if fresh.trim().parse::<u64>().unwrap_or(0) == 0 {
                    return Ok(false);
                }
                self.exec(owned(&[
                    "bundle",
                    "create",
                    &out,
                    "--branches",
                    "--tags",
                    &exclude,
                ]))?;
            }
            None => {
                self.exec(owned(&["bundle", "create", &out, "--all"]))?;
            }
        }
        Ok(true)
    }

    /// Apply an incremental bundle: its branches and tags, the checked-out branch included.
    pub fn fetch_bundle(&self, bundle: &Path) -> Result<(), IntegrationError> {
        self.exec(owned(&[
            "fetch",
            "--quiet",
            "--update-head-ok",
            &bundle.to_string_lossy(),
            "+refs/heads/*:refs/heads/*",
            "+refs/tags/*:refs/tags/*",
        ]))
        .map(|_| ())
    }

    /// Clone a bundle file into `target` and drop the `origin` that points at the (soon deleted) bundle.
    pub fn clone_bundle(&self, bundle: &Path, target: &Path) -> Result<(), IntegrationError> {
        let dir = bundle.parent().unwrap_or(Path::new("."));
        run(
            &owned(&[
                "clone",
                &bundle.to_string_lossy(),
                &target.to_string_lossy(),
            ]),
            dir,
            None,
            &self.clock,
        )?;
        let resolved = super::layout::normalize(target);
        let _ = run(
            &owned(&["remote", "remove", "origin"]),
            target,
            resolved.parent(),
            &self.clock,
        );
        Ok(())
    }
}

fn lines(out: &str) -> Vec<String> {
    out.split('\n')
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn nothing_to_commit(message: &str) -> bool {
    message.contains("nothing to commit") || message.contains("no changes added to commit")
}
