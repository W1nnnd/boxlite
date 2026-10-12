//! Python bindings for git operations.

use std::sync::Arc;

use boxlite::{GitBranches, GitFileStatus, GitStatus, LiteBox};
use pyo3::prelude::*;

use crate::util::map_err;

/// One changed path in a git status result.
#[pyclass(name = "GitFileStatus", frozen)]
#[derive(Clone, Debug)]
pub(crate) struct PyGitFileStatus {
    #[pyo3(get)]
    pub(crate) path: String,
    #[pyo3(get)]
    pub(crate) orig_path: Option<String>,
    #[pyo3(get)]
    pub(crate) index_status: String,
    #[pyo3(get)]
    pub(crate) worktree_status: String,
    staged: bool,
    untracked: bool,
}

impl From<GitFileStatus> for PyGitFileStatus {
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

#[pymethods]
impl PyGitFileStatus {
    /// True when the path has staged changes.
    #[getter]
    fn staged(&self) -> bool {
        self.staged
    }

    /// True when the path is untracked.
    #[getter]
    fn untracked(&self) -> bool {
        self.untracked
    }

    fn __repr__(&self) -> String {
        format!(
            "GitFileStatus(path={:?}, index_status={:?}, worktree_status={:?})",
            self.path, self.index_status, self.worktree_status
        )
    }
}

/// Structured `git status` result.
#[pyclass(name = "GitStatus", frozen)]
#[derive(Clone, Debug)]
pub(crate) struct PyGitStatus {
    #[pyo3(get)]
    pub(crate) current_branch: Option<String>,
    #[pyo3(get)]
    pub(crate) upstream: Option<String>,
    #[pyo3(get)]
    pub(crate) ahead: u32,
    #[pyo3(get)]
    pub(crate) behind: u32,
    #[pyo3(get)]
    pub(crate) branch_published: bool,
    #[pyo3(get)]
    pub(crate) head_commit: Option<String>,
    #[pyo3(get)]
    pub(crate) file_status: Vec<PyGitFileStatus>,
}

impl From<GitStatus> for PyGitStatus {
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

#[pymethods]
impl PyGitStatus {
    fn __repr__(&self) -> String {
        format!(
            "GitStatus(current_branch={:?}, ahead={}, behind={}, files={})",
            self.current_branch,
            self.ahead,
            self.behind,
            self.file_status.len()
        )
    }
}

/// Local branches of a repository.
#[pyclass(name = "GitBranches", frozen)]
#[derive(Clone, Debug)]
pub(crate) struct PyGitBranches {
    #[pyo3(get)]
    pub(crate) current: Option<String>,
    #[pyo3(get)]
    pub(crate) names: Vec<String>,
}

impl From<GitBranches> for PyGitBranches {
    fn from(b: GitBranches) -> Self {
        Self {
            current: b.current,
            names: b.names,
        }
    }
}

#[pymethods]
impl PyGitBranches {
    fn __repr__(&self) -> String {
        format!("GitBranches(current={:?}, names={:?})", self.current, self.names)
    }
}

/// Handle for git operations on a box.
///
/// Accessed as a property: `box.git.configure_user(...)`.
#[pyclass(name = "GitHandle")]
pub(crate) struct PyGitHandle {
    pub(crate) handle: Arc<LiteBox>,
}

#[pymethods]
impl PyGitHandle {
    /// Set `user.name` and `user.email` for commits in this box.
    #[pyo3(signature = (name, email, scope=None, path=None))]
    fn configure_user<'py>(
        &self,
        py: Python<'py>,
        name: String,
        email: String,
        scope: Option<String>,
        path: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            handle
                .git()
                .configure_user(&name, &email, scope.as_deref(), path.as_deref())
                .await
                .map_err(map_err)?;
            Ok(())
        })
    }

    /// Write a git config value. `scope` is `"global"` (default), `"local"`,
    /// or `"system"`; `"local"` requires `path`.
    #[pyo3(signature = (key, value, scope=None, path=None))]
    fn set_config<'py>(
        &self,
        py: Python<'py>,
        key: String,
        value: String,
        scope: Option<String>,
        path: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            handle
                .git()
                .set_config(&key, &value, scope.as_deref(), path.as_deref())
                .await
                .map_err(map_err)?;
            Ok(())
        })
    }

    /// Read a git config value. Same `scope` / `path` rules as `set_config`.
    #[pyo3(signature = (key, scope=None, path=None))]
    fn get_config<'py>(
        &self,
        py: Python<'py>,
        key: String,
        scope: Option<String>,
        path: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            handle
                .git()
                .get_config(&key, scope.as_deref(), path.as_deref())
                .await
                .map_err(map_err)
        })
    }

    /// Structured working-tree status of the repository at `path`.
    fn status<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let st = handle.git().status(&path).await.map_err(map_err)?;
            Ok(PyGitStatus::from(st))
        })
    }

    /// Local branches of the repository at `path`, plus the current one.
    fn branches<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let b = handle.git().branches(&path).await.map_err(map_err)?;
            Ok(PyGitBranches::from(b))
        })
    }

    fn __repr__(&self) -> String {
        format!("GitHandle(box_id={:?})", self.handle.id().to_string())
    }
}
