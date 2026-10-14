//! The `github` service: the Rust twin of `GitHubIntegration` (`@almadar/integrations` `integrations/github`).
//! REST actions (`createPR`, `getPRComments`, `listIssues`, `getIssue`, `getFile`, `listCommits`, `createIssue`)
//! go through the `Transport`; the git CLI actions (`cloneRepo`, `createBranch`, `commit`, `push`) run `git`.

mod api;
mod git;

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::Transport;

pub const SERVICE: &str = "github";

pub struct GithubProvider {
    token: String,
    repo: Mutex<Repo>,
    work_dir: Option<String>,
    transport: Arc<dyn Transport>,
}

#[derive(Clone, Default)]
struct Repo {
    owner: String,
    name: String,
}

/// Whether the credentials configure `github` at all (`RuntimeIntegrationManager`: a `GITHUB_TOKEN`).
pub fn is_configured(creds: &Credentials) -> bool {
    creds.get("GITHUB_TOKEN").is_some()
}

/// Register `github` on `integrations`: ready with a token, unconfigured without.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    if !is_configured(creds) {
        integrations.register_unconfigured(SERVICE);
        return;
    }
    match GithubProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl GithubProvider {
    /// The constructor of `GitHubIntegration`: a missing token is a plain error.
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let token = creds
            .get("GITHUB_TOKEN")
            .ok_or_else(|| IntegrationError::plain("GITHUB_TOKEN not configured"))?
            .to_string();
        Ok(Self {
            token,
            repo: Mutex::new(Repo {
                owner: creds.get("GITHUB_OWNER").unwrap_or_default().to_string(),
                name: creds.get("GITHUB_REPO").unwrap_or_default().to_string(),
            }),
            work_dir: creds.get("GITHUB_WORK_DIR").map(str::to_string),
            transport,
        })
    }

    /// The configured repository, or the TS error asking for it.
    fn configured_repo(&self) -> Result<Repo, IntegrationError> {
        let repo = self.repo.lock().expect("repo lock").clone();
        if repo.owner.is_empty() || repo.name.is_empty() {
            return Err(IntegrationError::plain(
                "GitHub owner and repo must be configured. Either set GITHUB_OWNER/GITHUB_REPO or clone a repository first.",
            ));
        }
        Ok(repo)
    }

    fn work_dir(&self, params: &Map<String, Value>) -> String {
        match params.get("workDir").and_then(Value::as_str) {
            Some(dir) if !dir.is_empty() => dir.to_string(),
            _ => self.work_dir.clone().unwrap_or_else(git::process_cwd),
        }
    }
}

fn message(text: String) -> Value {
    serde_json::json!({ "message": text })
}

fn text<'a>(params: &'a Map<String, Value>, key: &str) -> &'a str {
    params.get(key).and_then(Value::as_str).unwrap_or_default()
}

impl Provider for GithubProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "cloneRepo" => {
                let repo_url = text(params, "repoUrl");
                {
                    let mut repo = self.repo.lock().expect("repo lock");
                    if repo.owner.is_empty() || repo.name.is_empty() {
                        let (owner, name) = api::parse_repo_url(repo_url)?;
                        *repo = Repo { owner, name };
                    }
                }
                git::clone_repo(params, &self.token)?;
                Ok(message(format!(
                    "Successfully cloned {repo_url} to {}",
                    text(params, "targetDir")
                )))
            }
            "createBranch" => {
                git::create_branch(params, &self.work_dir(params))?;
                Ok(message(format!(
                    "Successfully created branch: {}",
                    text(params, "branchName")
                )))
            }
            "commit" => {
                git::commit(params, &self.work_dir(params))?;
                Ok(message(format!(
                    "Successfully committed changes: {}",
                    text(params, "message")
                )))
            }
            "push" => {
                git::push(params, &self.work_dir(params), &self.token)?;
                Ok(message(format!(
                    "Successfully pushed branch: {}",
                    text(params, "branchName")
                )))
            }
            "createPR" => api::create_pr(self, params),
            "getPRComments" => api::get_pr_comments(self, params),
            "listIssues" => api::list_issues(self, params),
            "getIssue" => api::get_issue(self, params),
            "getFile" => api::get_file(self, params),
            "listCommits" => api::list_commits(self, params),
            "createIssue" => api::create_issue(self, params),
            other => Err(IntegrationError::plain(format!(
                "Unknown github action: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests;
