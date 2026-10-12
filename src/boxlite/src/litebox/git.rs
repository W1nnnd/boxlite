//! Git sub-resource on LiteBox.

use std::sync::Arc;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::litebox::BoxCommand;
use crate::runtime::backend::BoxBackend;

/// Closed set of `git config` scopes. Public methods take `&str`; flags
/// written into the guest argv come from this enum, never from interpolation.
#[derive(Clone, Copy, Debug)]
enum GitConfigScope {
    Global,
    Local,
    System,
}

impl GitConfigScope {
    fn parse(scope: &str) -> BoxliteResult<Self> {
        match scope {
            "global" => Ok(Self::Global),
            "local" => Ok(Self::Local),
            "system" => Ok(Self::System),
            other => Err(BoxliteError::InvalidArgument(format!(
                "scope must be \"global\", \"local\", or \"system\", got {other:?}"
            ))),
        }
    }

    fn as_flag(self) -> &'static str {
        match self {
            Self::Global => "--global",
            Self::Local => "--local",
            Self::System => "--system",
        }
    }
}

/// Map a failed git invocation. Identity errors name `configure_user()`
/// instead of passing git's "Please tell me who you are" through verbatim.
///
/// `git config` never emits that message; `commit()` will call this.
#[allow(dead_code)]
fn map_git_failure(stderr: &str, fallback: impl Into<String>) -> BoxliteError {
    if stderr.contains("Please tell me who you are") {
        BoxliteError::InvalidArgument(
            "git identity is not configured; call configure_user() first".into(),
        )
    } else {
        BoxliteError::Execution(fallback.into())
    }
}

/// One changed path reported by [`GitHandle::status`].
///
/// `index_status` / `worktree_status` are git's single-letter XY codes from
/// `git status --porcelain=v2`: `.` unmodified, `M` modified, `T` type
/// changed, `A` added, `D` deleted, `R` renamed, `C` copied, `U` unmerged,
/// `?` untracked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileStatus {
    /// Path relative to the repository root, exactly as stored by git.
    pub path: String,
    /// Source path of a rename or copy; `None` otherwise.
    pub orig_path: Option<String>,
    /// Staged (index) state code.
    pub index_status: String,
    /// Unstaged (worktree) state code.
    pub worktree_status: String,
}

impl GitFileStatus {
    /// True when the path has staged changes.
    pub fn is_staged(&self) -> bool {
        !matches!(self.index_status.as_str(), "." | "?")
    }

    /// True when the path is untracked.
    pub fn is_untracked(&self) -> bool {
        self.index_status == "?"
    }
}

/// Structured result of [`GitHandle::status`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    /// Checked-out branch; `None` when HEAD is detached.
    pub current_branch: Option<String>,
    /// Upstream tracking ref (e.g. `origin/main`), if configured.
    pub upstream: Option<String>,
    /// Commits on the local branch not on its upstream (0 without upstream).
    pub ahead: u32,
    /// Commits on the upstream not on the local branch (0 without upstream).
    pub behind: u32,
    /// True when the branch has an upstream that exists.
    pub branch_published: bool,
    /// HEAD commit id; `None` on an unborn branch.
    pub head_commit: Option<String>,
    /// One entry per changed, staged, untracked, or unmerged path.
    /// Empty for a clean tree.
    pub file_status: Vec<GitFileStatus>,
}

/// Structured result of [`GitHandle::branches`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBranches {
    /// Checked-out local branch; `None` when HEAD is detached.
    pub current: Option<String>,
    /// Every local branch name, sorted by git's refname order.
    pub names: Vec<String>,
}

/// Parse `git status --porcelain=v2 --branch -z` output.
fn parse_status_v2(out: &str) -> BoxliteResult<GitStatus> {
    let bad = |what: &str| BoxliteError::Internal(format!("unexpected git status output: {what}"));
    let mut st = GitStatus::default();
    let mut has_upstream = false;
    let mut records = out.split('\0').filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        if let Some(header) = rec.strip_prefix("# ") {
            let (key, val) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => {
                    st.head_commit = (val != "(initial)").then(|| val.to_string());
                }
                "branch.head" => {
                    st.current_branch = (val != "(detached)").then(|| val.to_string());
                }
                "branch.upstream" => {
                    st.upstream = Some(val.to_string());
                }
                "branch.ab" => {
                    has_upstream = true;
                    let (a, b) = val.split_once(' ').ok_or_else(|| bad("branch.ab"))?;
                    st.ahead = a
                        .trim_start_matches('+')
                        .parse()
                        .map_err(|_| bad("ahead"))?;
                    st.behind = b
                        .trim_start_matches('-')
                        .parse()
                        .map_err(|_| bad("behind"))?;
                }
                _ => {}
            }
            continue;
        }
        let kind = rec.as_bytes()[0];
        match kind {
            b'1' | b'2' | b'u' => {
                // 1 XY sub mH mI mW hH hI path
                // 2 XY sub mH mI mW hH hI Xscore path  (+ NUL origPath)
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let fields = match kind {
                    b'1' => 8,
                    b'2' => 9,
                    _ => 10,
                };
                let mut parts = rec.splitn(fields + 1, ' ');
                parts.next();
                let xy = parts.next().ok_or_else(|| bad("XY"))?;
                let path = parts.nth(fields - 2).ok_or_else(|| bad("path"))?;
                let mut xy_chars = xy.chars();
                let x = xy_chars.next().ok_or_else(|| bad("XY"))?;
                let y = xy_chars.next().ok_or_else(|| bad("XY"))?;
                let orig_path = if kind == b'2' {
                    Some(
                        records
                            .next()
                            .ok_or_else(|| bad("rename source"))?
                            .to_string(),
                    )
                } else {
                    None
                };
                st.file_status.push(GitFileStatus {
                    path: path.to_string(),
                    orig_path,
                    index_status: x.to_string(),
                    worktree_status: y.to_string(),
                });
            }
            b'?' => {
                let path = rec.get(2..).ok_or_else(|| bad("untracked"))?;
                st.file_status.push(GitFileStatus {
                    path: path.to_string(),
                    orig_path: None,
                    index_status: "?".into(),
                    worktree_status: "?".into(),
                });
            }
            _ => {} // `!` ignored entries are not requested
        }
    }
    st.branch_published = has_upstream;
    Ok(st)
}

/// Parse `git for-each-ref --format='%(HEAD)%00%(refname:short)' refs/heads`.
fn parse_branches(out: &str) -> GitBranches {
    let mut b = GitBranches::default();
    for line in out.lines().filter(|l| !l.is_empty()) {
        let (marker, name) = line.split_once('\0').unwrap_or(("", line));
        if marker == "*" {
            b.current = Some(name.to_string());
        }
        b.names.push(name.to_string());
    }
    b
}

/// Handle for git operations on a LiteBox.
///
/// Obtained via `litebox.git()`. Owns a backend handle and can be
/// used independently from the originating `LiteBox` borrow.
#[derive(Clone)]
pub struct GitHandle {
    box_backend: Arc<dyn BoxBackend>,
}

impl GitHandle {
    pub(crate) fn new(box_backend: Arc<dyn BoxBackend>) -> Self {
        Self { box_backend }
    }

    /// Write a git config value. `scope` is `"global"` (default), `"local"`,
    /// or `"system"`; `"local"` requires `path`.
    pub async fn set_config(
        &self,
        key: &str,
        value: &str,
        scope: Option<&str>,
        path: Option<&str>,
    ) -> BoxliteResult<()> {
        let scope = scope.unwrap_or("global");
        self.run_git(&[key, value], scope, path).await?;
        Ok(())
    }

    /// Read a git config value. Same `scope` / `path` rules as [`Self::set_config`].
    pub async fn get_config(
        &self,
        key: &str,
        scope: Option<&str>,
        path: Option<&str>,
    ) -> BoxliteResult<String> {
        let scope = scope.unwrap_or("global");
        let out = self.run_git(&["--get", key], scope, path).await?;
        Ok(out.trim().to_string())
    }

    /// Set `user.name` and `user.email` for commits in this box.
    pub async fn configure_user(
        &self,
        name: &str,
        email: &str,
        scope: Option<&str>,
        path: Option<&str>,
    ) -> BoxliteResult<()> {
        self.set_config("user.name", name, scope, path).await?;
        self.set_config("user.email", email, scope, path).await?;
        Ok(())
    }

    /// Structured working-tree status of the repository at `path`.
    ///
    /// Parsed from NUL-delimited `git status --porcelain=v2 --branch -z`, so
    /// paths containing spaces or newlines are reported verbatim.
    pub async fn status(&self, path: &str) -> BoxliteResult<GitStatus> {
        let out = self
            .run_git_read(
                &[
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "-z",
                    "--untracked-files=all",
                ],
                path,
                "git status failed",
            )
            .await?;
        parse_status_v2(&out)
    }

    /// Local branches of the repository at `path`, plus the current one.
    pub async fn branches(&self, path: &str) -> BoxliteResult<GitBranches> {
        let out = self
            .run_git_read(
                &[
                    "for-each-ref",
                    "--format=%(HEAD)%00%(refname:short)",
                    "refs/heads",
                ],
                path,
                "git branch listing failed",
            )
            .await?;
        let mut branches = parse_branches(&out);
        if branches.current.is_none() {
            // Unborn branch (fresh `git init`): no ref yet, but HEAD names it.
            let st = self.status(path).await?;
            if st.head_commit.is_none() {
                branches.current = st.current_branch;
            }
        }
        Ok(branches)
    }

    async fn run_git_read(
        &self,
        args: &[&str],
        path: &str,
        failure: &str,
    ) -> BoxliteResult<String> {
        if path.is_empty() {
            return Err(BoxliteError::InvalidArgument(
                "path must not be empty".into(),
            ));
        }
        // -c core.quotepath=off keeps non-ASCII names as raw UTF-8.
        let mut cmd = BoxCommand::new("git")
            .arg("-c")
            .arg("core.quotepath=off")
            .arg("-C")
            .arg(path);
        for arg in args {
            cmd = cmd.arg(*arg);
        }
        self.collect_stdout(cmd, failure).await
    }

    async fn collect_stdout(&self, cmd: BoxCommand, failure: &str) -> BoxliteResult<String> {
        let mut execution = self.box_backend.exec(cmd).await?;
        let mut stdout = String::new();
        if let Some(mut stream) = execution.stdout() {
            while let Some(chunk) = stream.next().await {
                stdout.push_str(&chunk);
            }
        }
        let result = execution.wait().await?;
        if !result.success() {
            return Err(BoxliteError::Execution(failure.into()));
        }
        Ok(stdout)
    }

    async fn run_git(
        &self,
        args: &[&str],
        scope: &str,
        path: Option<&str>,
    ) -> BoxliteResult<String> {
        let scope = GitConfigScope::parse(scope)?;
        if matches!(scope, GitConfigScope::Local) && path.is_none() {
            return Err(BoxliteError::InvalidArgument(
                "scope=\"local\" requires path".into(),
            ));
        }

        let mut cmd = BoxCommand::new("git").arg("config").arg(scope.as_flag());
        for arg in args {
            cmd = cmd.arg(*arg);
        }
        if let Some(dir) = path {
            cmd = cmd.working_dir(dir);
        }

        let mut execution = self.box_backend.exec(cmd).await?;

        let mut stdout = String::new();
        if let Some(mut stream) = execution.stdout() {
            while let Some(chunk) = stream.next().await {
                stdout.push_str(&chunk);
            }
        }

        let result = execution.wait().await?;
        if !result.success() {
            // Typed error; do not pass git stderr through.
            return Err(BoxliteError::Execution("git config failed".into()));
        }
        Ok(stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_known_scopes() {
        assert_eq!(
            GitConfigScope::parse("global").unwrap().as_flag(),
            "--global"
        );
        assert_eq!(GitConfigScope::parse("local").unwrap().as_flag(), "--local");
        assert_eq!(
            GitConfigScope::parse("system").unwrap().as_flag(),
            "--system"
        );
    }

    #[test]
    fn parse_rejects_unknown_scope() {
        for scope in ["file", "globel", "--global", ""] {
            let err = GitConfigScope::parse(scope).unwrap_err();
            assert!(
                matches!(err, BoxliteError::InvalidArgument(ref msg) if msg.contains("global")),
                "{scope:?} -> {err}"
            );
        }
    }

    #[test]
    fn status_clean_tracking_branch() {
        let out = "# branch.oid 1111111111111111111111111111111111111111\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +1 -2\0";
        let st = parse_status_v2(out).unwrap();
        assert_eq!(st.current_branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (1, 2));
        assert!(st.branch_published);
        assert!(st.file_status.is_empty());
    }

    #[test]
    fn status_entries_with_spaces_newlines_and_renames() {
        let h = "0000000000000000000000000000000000000000";
        let out = format!(
            "# branch.oid (initial)\0# branch.head (detached)\0\
             1 .M N... 100644 100644 100644 {h} {h} README.md\0\
             1 A. N... 000000 100644 100644 {h} {h} with space.txt\0\
             2 R. N... 100644 100644 100644 {h} {h} R100 new name\0old name\0\
             u UU N... 100644 100644 100644 100644 {h} {h} {h} conflict\0\
             ? line\nbreak\0"
        );
        let st = parse_status_v2(&out).unwrap();
        assert_eq!(st.current_branch, None);
        assert_eq!(st.head_commit, None);
        assert!(!st.branch_published);
        let paths: Vec<_> = st.file_status.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "README.md",
                "with space.txt",
                "new name",
                "conflict",
                "line\nbreak"
            ]
        );
        assert_eq!(st.file_status[0].worktree_status, "M");
        assert!(!st.file_status[0].is_staged());
        assert!(st.file_status[1].is_staged());
        assert_eq!(st.file_status[2].orig_path.as_deref(), Some("old name"));
        assert_eq!(st.file_status[3].index_status, "U");
        assert!(st.file_status[4].is_untracked());
    }

    #[test]
    fn branches_marks_current() {
        let b = parse_branches(" \0feature/x\n*\0main\n");
        assert_eq!(b.current.as_deref(), Some("main"));
        assert_eq!(b.names, ["feature/x", "main"]);
    }
}
