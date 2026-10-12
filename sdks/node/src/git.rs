//! Node.js bindings for git operations.

use std::sync::Arc;

use boxlite::{GitBranches, GitFileStatus, GitStatus, LiteBox};
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::util::map_err;

/// One changed path in a git status result.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsGitFileStatus {
    pub path: String,
    #[napi(js_name = "origPath")]
    pub orig_path: Option<String>,
    #[napi(js_name = "indexStatus")]
    pub index_status: String,
    #[napi(js_name = "worktreeStatus")]
    pub worktree_status: String,
    pub staged: bool,
    pub untracked: bool,
}

impl From<GitFileStatus> for JsGitFileStatus {
    fn from(f: GitFileStatus) -> Self {
        Self {
            staged: f.is_staged(),
            untracked: f.is_untracked(),
            path: f.path,
            orig_path: f.orig_path,
            index_status: f.index_status,
            worktree_status: f.worktree_status,
        }
    }
}

/// Structured `git status` result.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsGitStatus {
    #[napi(js_name = "currentBranch")]
    pub current_branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    #[napi(js_name = "branchPublished")]
    pub branch_published: bool,
    #[napi(js_name = "headCommit")]
    pub head_commit: Option<String>,
    #[napi(js_name = "fileStatus")]
    pub file_status: Vec<JsGitFileStatus>,
}

impl From<GitStatus> for JsGitStatus {
    fn from(s: GitStatus) -> Self {
        Self {
            current_branch: s.current_branch,
            upstream: s.upstream,
            ahead: s.ahead,
            behind: s.behind,
            branch_published: s.branch_published,
            head_commit: s.head_commit,
            file_status: s.file_status.into_iter().map(Into::into).collect(),
        }
    }
}

/// Local branches of a repository.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsGitBranches {
    pub current: Option<String>,
    pub names: Vec<String>,
}

impl From<GitBranches> for JsGitBranches {
    fn from(b: GitBranches) -> Self {
        Self {
            current: b.current,
            names: b.names,
        }
    }
}

/// Handle for git operations on a box.
///
/// Accessed as a property: `box.git.configureUser(...)`.
#[napi]
pub struct JsGitHandle {
    pub(crate) handle: Arc<LiteBox>,
}

#[napi]
impl JsGitHandle {
    /// Set `user.name` and `user.email` for commits in this box.
    #[napi]
    pub async fn configure_user(
        &self,
        name: String,
        email: String,
        scope: Option<String>,
        path: Option<String>,
    ) -> Result<()> {
        let handle = Arc::clone(&self.handle);
        handle
            .git()
            .configure_user(&name, &email, scope.as_deref(), path.as_deref())
            .await
            .map_err(map_err)
    }

    /// Write a git config value. `scope` is `"global"` (default), `"local"`,
    /// or `"system"`; `"local"` requires `path`.
    #[napi]
    pub async fn set_config(
        &self,
        key: String,
        value: String,
        scope: Option<String>,
        path: Option<String>,
    ) -> Result<()> {
        let handle = Arc::clone(&self.handle);
        handle
            .git()
            .set_config(&key, &value, scope.as_deref(), path.as_deref())
            .await
            .map_err(map_err)
    }

    /// Read a git config value. Same `scope` / `path` rules as `setConfig`.
    #[napi]
    pub async fn get_config(
        &self,
        key: String,
        scope: Option<String>,
        path: Option<String>,
    ) -> Result<String> {
        let handle = Arc::clone(&self.handle);
        handle
            .git()
            .get_config(&key, scope.as_deref(), path.as_deref())
            .await
            .map_err(map_err)
    }

    /// Structured working-tree status of the repository at `path`.
    #[napi]
    pub async fn status(&self, path: String) -> Result<JsGitStatus> {
        let handle = Arc::clone(&self.handle);
        handle
            .git()
            .status(&path)
            .await
            .map(Into::into)
            .map_err(map_err)
    }

    /// Local branches of the repository at `path`, plus the current one.
    #[napi]
    pub async fn branches(&self, path: String) -> Result<JsGitBranches> {
        let handle = Arc::clone(&self.handle);
        handle
            .git()
            .branches(&path)
            .await
            .map(Into::into)
            .map_err(map_err)
    }
}
