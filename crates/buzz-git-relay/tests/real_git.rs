use buzz_git_relay::{
    ports::{EvidenceStore, LockLease, LockManager, RepositoryPort, RepositorySession},
    ApprovalEventId, Classification, CommitOid, Enrollment, EnrollmentId, GithubRepositoryId,
    ManagedRef, MutationEvidence, PortError, PortErrorKind, ReconcileRequest, ReconcileResult,
    Reconciler, ReplayKey, RunId, RunIdGenerator, Tips,
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;

#[tokio::test]
async fn real_git_fast_forward_preserves_unlisted_branch() {
    let repositories = Repositories::with_history();
    let harness = Harness::new(&repositories);
    let result = harness.apply(&repositories.github_tip).await;

    assert_eq!(result.classification, Classification::InSync);
    assert_eq!(result.mutation, MutationEvidence::FastForward);
    assert_eq!(
        remote_tip(&repositories.buzz, "refs/heads/main"),
        Some(repositories.github_tip.clone())
    );
    assert_eq!(
        remote_tip(&repositories.buzz, "refs/heads/unlisted"),
        Some(repositories.unlisted_tip.clone())
    );
}

#[tokio::test]
async fn real_git_push_uses_only_exact_sha_to_exact_refspec() {
    let repositories = Repositories::with_history();
    let harness = Harness::new(&repositories);
    let result = harness.apply(&repositories.github_tip).await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        harness.adapter.refspecs.lock().unwrap().as_slice(),
        &[format!("{}:refs/heads/main", repositories.github_tip)]
    );
}

#[tokio::test]
async fn missing_github_main_never_creates_either_managed_ref() {
    let repositories = Repositories::without_github_main();
    let harness = Harness::new(&repositories);
    let result = harness.observe().await;

    assert_eq!(result.classification, Classification::ConfigError);
    assert_eq!(remote_tip(&repositories.github, "refs/heads/main"), None);
    assert_eq!(
        remote_tip(&repositories.buzz, "refs/heads/main"),
        Some(repositories.buzz_tip)
    );
    assert!(harness.adapter.refspecs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_buzz_main_never_creates_either_managed_ref() {
    let repositories = Repositories::without_buzz_main();
    let harness = Harness::new(&repositories);
    let result = harness.observe().await;

    assert_eq!(result.classification, Classification::ConfigError);
    assert_eq!(
        remote_tip(&repositories.github, "refs/heads/main"),
        Some(repositories.github_tip)
    );
    assert_eq!(remote_tip(&repositories.buzz, "refs/heads/main"), None);
    assert!(harness.adapter.refspecs.lock().unwrap().is_empty());
}

struct Harness {
    enrollment: Enrollment,
    reconciler: Reconciler,
    adapter: Arc<LocalGitPort>,
}

impl Harness {
    fn new(repositories: &Repositories) -> Self {
        let adapter = Arc::new(LocalGitPort {
            github: repositories.github.clone(),
            buzz: repositories.buzz.clone(),
            refspecs: Arc::new(Mutex::new(Vec::new())),
        });
        Self {
            enrollment: Enrollment::enabled(
                EnrollmentId::new("disposable").unwrap(),
                GithubRepositoryId::new("98765").unwrap(),
                ManagedRef::main(),
            )
            .with_owner_approved_apply(),
            reconciler: Reconciler::new(
                adapter.clone(),
                Arc::new(OpenLock),
                Arc::new(NoopEvidence),
                Arc::new(FixedRunId),
            ),
            adapter,
        }
    }

    async fn observe(&self) -> ReconcileResult {
        self.reconciler
            .reconcile(&self.enrollment, ReconcileRequest::observe())
            .await
    }

    async fn apply(&self, target: &str) -> ReconcileResult {
        self.reconciler
            .reconcile(
                &self.enrollment,
                ReconcileRequest::apply(
                    CommitOid::new(target).unwrap(),
                    ApprovalEventId::new("c".repeat(64)).unwrap(),
                ),
            )
            .await
    }
}

struct Repositories {
    _root: TempDir,
    github: PathBuf,
    buzz: PathBuf,
    github_tip: String,
    buzz_tip: String,
    unlisted_tip: String,
}

impl Repositories {
    fn with_history() -> Self {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let github = root.path().join("github.git");
        let buzz = root.path().join("buzz.git");
        init_work(&work);
        init_bare(&github);
        init_bare(&buzz);

        write_file(&work, "history.txt", "a\n");
        git(&work, &["add", "history.txt"]);
        git(&work, &["commit", "-m", "A"]);
        let a = git_output(&work, &["rev-parse", "HEAD"]);
        git(
            &work,
            &[
                "push",
                github.to_str().unwrap(),
                &format!("{a}:refs/heads/main"),
            ],
        );
        git(
            &work,
            &[
                "push",
                buzz.to_str().unwrap(),
                &format!("{a}:refs/heads/main"),
            ],
        );
        git(
            &work,
            &[
                "push",
                buzz.to_str().unwrap(),
                &format!("{a}:refs/heads/unlisted"),
            ],
        );

        write_file(&work, "history.txt", "a\nb\n");
        git(&work, &["add", "history.txt"]);
        git(&work, &["commit", "-m", "B"]);
        let b = git_output(&work, &["rev-parse", "HEAD"]);
        git(
            &work,
            &[
                "push",
                github.to_str().unwrap(),
                &format!("{b}:refs/heads/main"),
            ],
        );

        Self {
            _root: root,
            github,
            buzz,
            github_tip: b,
            buzz_tip: a.clone(),
            unlisted_tip: a,
        }
    }

    fn without_github_main() -> Self {
        let mut repositories = Self::with_history();
        git(
            &repositories.github,
            &["update-ref", "-d", "refs/heads/main"],
        );
        repositories.github_tip.clear();
        repositories
    }

    fn without_buzz_main() -> Self {
        let mut repositories = Self::with_history();
        git(&repositories.buzz, &["update-ref", "-d", "refs/heads/main"]);
        repositories.buzz_tip.clear();
        repositories
    }
}

struct LocalGitPort {
    github: PathBuf,
    buzz: PathBuf,
    refspecs: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl RepositoryPort for LocalGitPort {
    async fn open(&self, enrollment: &Enrollment) -> Result<Box<dyn RepositorySession>, PortError> {
        let root = tempfile::tempdir().map_err(|_| PortError::new(PortErrorKind::Unexpected))?;
        let local = root.path().join("session.git");
        if !git_ok(root.path(), &["init", "--bare", local.to_str().unwrap()]) {
            return Err(PortError::new(PortErrorKind::Unexpected));
        }
        let mut session = LocalGitSession {
            _root: root,
            local,
            github: self.github.clone(),
            buzz: self.buzz.clone(),
            managed_ref: enrollment.managed_ref.clone(),
            refspecs: self.refspecs.clone(),
        };
        session.refresh_both_sync()?;
        Ok(Box::new(session))
    }
}

struct LocalGitSession {
    _root: TempDir,
    local: PathBuf,
    github: PathBuf,
    buzz: PathBuf,
    managed_ref: ManagedRef,
    refspecs: Arc<Mutex<Vec<String>>>,
}

impl LocalGitSession {
    fn fetch(&self, remote: &Path, destination: &str) -> Result<CommitOid, PortError> {
        let refspec = format!("+{}:{destination}", self.managed_ref.as_str());
        if !git_ok(
            &self.local,
            &["fetch", "--no-tags", remote.to_str().unwrap(), &refspec],
        ) {
            return Err(PortError::new(PortErrorKind::Config));
        }
        CommitOid::new(git_output(&self.local, &["rev-parse", destination]))
            .map_err(|_| PortError::new(PortErrorKind::Verify))
    }

    fn refresh_both_sync(&mut self) -> Result<Tips, PortError> {
        Ok(Tips {
            github: self.fetch(&self.github, "refs/git-relay/github")?,
            buzz: self.fetch(&self.buzz, "refs/git-relay/buzz")?,
        })
    }
}

#[async_trait::async_trait]
impl RepositorySession for LocalGitSession {
    async fn tips(&mut self) -> Result<Tips, PortError> {
        self.refresh_both_sync()
    }

    async fn is_ancestor(
        &mut self,
        ancestor: &CommitOid,
        descendant: &CommitOid,
    ) -> Result<bool, PortError> {
        let status = Command::new("git")
            .args([
                "merge-base",
                "--is-ancestor",
                ancestor.as_str(),
                descendant.as_str(),
            ])
            .current_dir(&self.local)
            .status()
            .map_err(|_| PortError::new(PortErrorKind::Unexpected))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(PortError::new(PortErrorKind::Verify)),
        }
    }

    async fn refresh_buzz(&mut self) -> Result<CommitOid, PortError> {
        self.fetch(&self.buzz, "refs/git-relay/buzz")
    }

    async fn refresh_both(&mut self) -> Result<Tips, PortError> {
        self.refresh_both_sync()
    }

    async fn push_exact(
        &mut self,
        target: &CommitOid,
        managed_ref: &ManagedRef,
    ) -> Result<(), PortError> {
        let refspec = format!("{}:{}", target.as_str(), managed_ref.as_str());
        self.refspecs.lock().unwrap().push(refspec.clone());
        if git_ok(
            &self.local,
            &["push", self.buzz.to_str().unwrap(), &refspec],
        ) {
            Ok(())
        } else {
            Err(PortError::new(PortErrorKind::Verify))
        }
    }

    async fn close(self: Box<Self>) -> Result<(), PortError> {
        Ok(())
    }
}

struct OpenLock;
struct OpenLease;

#[async_trait::async_trait]
impl LockManager for OpenLock {
    async fn acquire(&self, _key: &str) -> Result<Box<dyn LockLease>, PortError> {
        Ok(Box::new(OpenLease))
    }
}

#[async_trait::async_trait]
impl LockLease for OpenLease {
    async fn release(self: Box<Self>) -> Result<(), PortError> {
        Ok(())
    }
}

struct NoopEvidence;

#[async_trait::async_trait]
impl EvidenceStore for NoopEvidence {
    async fn find_verified(&self, _key: &ReplayKey) -> Result<Option<ReconcileResult>, PortError> {
        Ok(None)
    }

    async fn append(&self, _result: &ReconcileResult) -> Result<(), PortError> {
        Ok(())
    }
}

struct FixedRunId;

impl RunIdGenerator for FixedRunId {
    fn next(&self) -> RunId {
        RunId::new("real-git-run").unwrap()
    }
}

fn init_work(path: &Path) {
    git(path.parent().unwrap(), &["init", path.to_str().unwrap()]);
    git(path, &["config", "user.name", "Git Relay Test"]);
    git(path, &["config", "user.email", "git-relay@example.invalid"]);
}

fn init_bare(path: &Path) {
    git(
        path.parent().unwrap(),
        &["init", "--bare", path.to_str().unwrap()],
    );
}

fn write_file(repo: &Path, name: &str, content: &str) {
    std::fs::write(repo.join(name), content).unwrap();
}

fn git(cwd: &Path, args: &[&str]) {
    assert!(git_ok(cwd, args), "git command failed: {args:?}");
}

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap()
        .success()
}

fn git_output(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git command failed: {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn remote_tip(repo: &Path, managed_ref: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", managed_ref])
        .current_dir(repo)
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
}
