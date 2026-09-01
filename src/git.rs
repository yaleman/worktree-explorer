use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use gix::{
    bstr::ByteSlice,
    status::{Item as GixStatusItem, index_worktree::iter::Summary},
};

const LOG_LIMIT: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeKey {
    Main,
    Linked(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeInfo {
    pub key: WorktreeKey,
    pub path: PathBuf,
    pub head: Option<String>,
    pub committed_at: Option<i64>,
    pub branch: Option<String>,
    pub locked: bool,
    pub available: bool,
}

impl WorktreeInfo {
    pub fn reference_label(&self) -> &str {
        if let Some(branch) = self.branch.as_deref() {
            branch
        } else if self.head.is_some() {
            "detached HEAD"
        } else {
            "unborn HEAD"
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StatusArea {
    Index,
    Worktree,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Conflict,
    IntentToAdd,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type changed",
            Self::Conflict => "conflict",
            Self::IntentToAdd => "intent to add",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StatusEntry {
    pub area: StatusArea,
    pub kind: ChangeKind,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeStatus {
    pub reference: String,
    pub entries: Vec<StatusEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
    pub id: String,
    pub subject: String,
    pub author: String,
    pub committed_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionAssessment {
    pub changes: Vec<StatusEntry>,
    pub locked: bool,
    pub missing: bool,
}

impl DeletionAssessment {
    pub fn requires_force(&self) -> bool {
        !self.changes.is_empty() || self.locked
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeleteResult {
    Deleted,
    NeedsForce(DeletionAssessment),
}

#[derive(Debug)]
pub struct RepositoryManager {
    main_git_dir: PathBuf,
    common_dir: PathBuf,
}

impl RepositoryManager {
    pub fn discover(path: &Path) -> Result<Self> {
        let repository = gix::discover(path).with_context(|| {
            format!(
                "failed to discover a Git repository from {}",
                path.display()
            )
        })?;
        let main = repository
            .main_repo()
            .context("failed to open the main repository")?;
        let main_git_dir = fs::canonicalize(main.git_dir())
            .with_context(|| format!("failed to validate {}", main.git_dir().display()))?;
        let common_dir = fs::canonicalize(main.common_dir())
            .with_context(|| format!("failed to validate {}", main.common_dir().display()))?;
        Ok(Self {
            main_git_dir,
            common_dir,
        })
    }

    pub fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let main = self.open_main()?;
        let mut worktrees = Vec::new();

        if let Some(worktree) = main.worktree() {
            let (head, committed_at, branch) = head_details(&main);
            worktrees.push(WorktreeInfo {
                key: WorktreeKey::Main,
                path: worktree.base().to_path_buf(),
                head,
                committed_at,
                branch,
                locked: false,
                available: worktree.dot_git_exists(),
            });
        }

        for proxy in main
            .worktrees()
            .context("failed to enumerate linked worktrees")?
        {
            let key = WorktreeKey::Linked(proxy.id().to_string());
            let path = proxy
                .base()
                .with_context(|| format!("failed to read the path for worktree {}", proxy.id()))?;
            let locked = proxy.is_locked();
            let available = path.is_dir();
            let (head, committed_at, branch) = proxy
                .into_repo_with_possibly_inaccessible_worktree()
                .ok()
                .map_or((None, None, None), |repository| head_details(&repository));
            worktrees.push(WorktreeInfo {
                key,
                path,
                head,
                committed_at,
                branch,
                locked,
                available,
            });
        }

        Ok(worktrees)
    }

    pub fn status(&self, key: &WorktreeKey) -> Result<WorktreeStatus> {
        let info = self.find_worktree(key)?;
        if !info.available {
            bail!("worktree {} is unavailable", info.path.display());
        }
        let repository = gix::open(&info.path)
            .with_context(|| format!("failed to open worktree {}", info.path.display()))?;
        let reference = info.reference_label().to_owned();
        let mut entries = Vec::new();
        let iter = repository
            .status(gix::progress::Discard)
            .context("failed to prepare worktree status")?
            .into_iter(Vec::new())
            .context("failed to calculate worktree status")?;

        for item in iter {
            let item = item.context("failed while reading worktree status")?;
            if let Some(entry) = status_entry(item) {
                entries.push(entry);
            }
        }
        entries.sort();
        Ok(WorktreeStatus { reference, entries })
    }

    pub fn log(&self, key: &WorktreeKey) -> Result<Vec<LogEntry>> {
        let info = self.find_worktree(key)?;
        if !info.available {
            bail!("worktree {} is unavailable", info.path.display());
        }
        let repository = gix::open(&info.path)
            .with_context(|| format!("failed to open worktree {}", info.path.display()))?;
        let head = match repository.head_id() {
            Ok(head) => head,
            Err(gix::reference::head_id::Error::PeelToId(
                gix::head::peel::into_id::Error::Unborn { .. },
            )) => return Ok(Vec::new()),
            Err(error) => return Err(error).context("failed to resolve worktree HEAD"),
        };
        let walk = head
            .ancestors()
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ))
            .all()
            .context("failed to traverse commit history")?;
        let mut entries = Vec::new();
        for info in walk.take(LOG_LIMIT) {
            let info = info.context("failed while traversing commit history")?;
            let commit = info.object().context("failed to read commit")?;
            let decoded = commit.decode().context("failed to decode commit")?;
            let subject = decoded.message().title.to_str_lossy().trim().to_owned();
            let author = decoded
                .author()
                .context("failed to decode commit author")?
                .name
                .to_str_lossy()
                .into_owned();
            entries.push(LogEntry {
                id: abbreviate_id(&info.id.to_string()),
                subject,
                author,
                committed_at: decoded
                    .time()
                    .context("failed to decode commit time")?
                    .seconds,
            });
        }
        Ok(entries)
    }

    pub fn assess_deletion(&self, key: &WorktreeKey) -> Result<DeletionAssessment> {
        let (info, admin_dir) = self.validated_linked_worktree(key)?;
        self.protect_current_directory(&info.path)?;
        let missing = !info.path.exists();
        let changes = if missing {
            Vec::new()
        } else {
            self.status(key)?.entries
        };
        let locked = admin_dir.join("locked").is_file();
        Ok(DeletionAssessment {
            changes,
            locked,
            missing,
        })
    }

    pub fn delete_worktree(&self, key: &WorktreeKey, force: bool) -> Result<DeleteResult> {
        let assessment = self.assess_deletion(key)?;
        if assessment.requires_force() && !force {
            return Ok(DeleteResult::NeedsForce(assessment));
        }
        let (info, admin_dir) = self.validated_linked_worktree(key)?;
        self.protect_current_directory(&info.path)?;

        if info.path.exists() {
            fs::remove_dir_all(&info.path).with_context(|| {
                format!("failed to remove worktree checkout {}", info.path.display())
            })?;
        }
        fs::remove_dir_all(&admin_dir).with_context(|| {
            format!(
                "checkout was removed, but worktree metadata remains at {}",
                admin_dir.display()
            )
        })?;
        Ok(DeleteResult::Deleted)
    }

    fn open_main(&self) -> Result<gix::Repository> {
        gix::open(&self.main_git_dir).context("failed to open the main repository")
    }

    fn find_worktree(&self, key: &WorktreeKey) -> Result<WorktreeInfo> {
        self.list_worktrees()?
            .into_iter()
            .find(|worktree| &worktree.key == key)
            .ok_or_else(|| anyhow!("selected worktree no longer exists"))
    }

    fn validated_linked_worktree(&self, key: &WorktreeKey) -> Result<(WorktreeInfo, PathBuf)> {
        let WorktreeKey::Linked(id) = key else {
            bail!("the main worktree cannot be deleted");
        };
        let mut components = Path::new(id).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            bail!("invalid linked worktree identifier");
        }

        let info = self.find_worktree(key)?;
        let worktrees_dir = self.common_dir.join("worktrees");
        let admin_dir = worktrees_dir.join(id);
        let canonical_worktrees = fs::canonicalize(&worktrees_dir)
            .with_context(|| format!("failed to validate {}", worktrees_dir.display()))?;
        let canonical_admin = fs::canonicalize(&admin_dir)
            .with_context(|| format!("failed to validate {}", admin_dir.display()))?;
        if canonical_admin.parent() != Some(canonical_worktrees.as_path()) {
            bail!("worktree metadata is outside the repository worktrees directory");
        }

        if info.path.exists() {
            let metadata = fs::symlink_metadata(&info.path).with_context(|| {
                format!("failed to inspect worktree path {}", info.path.display())
            })?;
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                bail!("worktree path is not a real directory");
            }
            let dot_git = info.path.join(".git");
            if !fs::symlink_metadata(&dot_git)
                .with_context(|| format!("failed to inspect {}", dot_git.display()))?
                .file_type()
                .is_file()
            {
                bail!("worktree .git entry is not a file");
            }
            let linked = gix::open(&info.path)
                .with_context(|| format!("failed to validate worktree {}", info.path.display()))?;
            let linked_git_dir = fs::canonicalize(linked.git_dir())
                .with_context(|| format!("failed to validate {}", linked.git_dir().display()))?;
            if linked_git_dir != canonical_admin {
                bail!("worktree .git entry does not point to its administrative directory");
            }
            let linked_common = fs::canonicalize(linked.common_dir())
                .with_context(|| format!("failed to validate {}", linked.common_dir().display()))?;
            let expected_common = fs::canonicalize(&self.common_dir)
                .with_context(|| format!("failed to validate {}", self.common_dir.display()))?;
            if linked_common != expected_common {
                bail!("worktree belongs to a different common repository");
            }
        }
        Ok((info, admin_dir))
    }

    fn protect_current_directory(&self, worktree_path: &Path) -> Result<()> {
        if !worktree_path.exists() {
            return Ok(());
        }
        let current = env::current_dir().context("failed to read the current directory")?;
        let current = fs::canonicalize(&current)
            .with_context(|| format!("failed to validate {}", current.display()))?;
        let worktree = fs::canonicalize(worktree_path)
            .with_context(|| format!("failed to validate {}", worktree_path.display()))?;
        if current.starts_with(&worktree) {
            bail!("the worktree containing the current directory cannot be deleted");
        }
        Ok(())
    }
}

fn head_details(repository: &gix::Repository) -> (Option<String>, Option<i64>, Option<String>) {
    let head = repository.head_id().ok();
    let committed_at = head
        .as_ref()
        .and_then(|id| id.object().ok())
        .and_then(|object| object.try_into_commit().ok())
        .and_then(|commit| commit.time().ok())
        .map(|time| time.seconds);
    let head = head.map(|id| abbreviate_id(&id.to_string()));
    let branch = repository
        .head_name()
        .ok()
        .flatten()
        .map(|name| name.shorten().to_string());
    (head, committed_at, branch)
}

fn abbreviate_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn status_entry(item: GixStatusItem) -> Option<StatusEntry> {
    let path = item.location().to_str_lossy().into_owned();
    match item {
        GixStatusItem::TreeIndex(change) => {
            use gix::diff::index::ChangeRef;
            let kind = match change {
                ChangeRef::Addition { .. } => ChangeKind::Added,
                ChangeRef::Deletion { .. } => ChangeKind::Deleted,
                ChangeRef::Modification { .. } => ChangeKind::Modified,
                ChangeRef::Rewrite { copy, .. } => {
                    if copy {
                        ChangeKind::Copied
                    } else {
                        ChangeKind::Renamed
                    }
                }
            };
            Some(StatusEntry {
                area: StatusArea::Index,
                kind,
                path,
            })
        }
        GixStatusItem::IndexWorktree(change) => {
            let kind = match change.summary()? {
                Summary::Removed => ChangeKind::Deleted,
                Summary::Added => ChangeKind::Added,
                Summary::Modified => ChangeKind::Modified,
                Summary::TypeChange => ChangeKind::TypeChanged,
                Summary::Renamed => ChangeKind::Renamed,
                Summary::Copied => ChangeKind::Copied,
                Summary::IntentToAdd => ChangeKind::IntentToAdd,
                Summary::Conflict => ChangeKind::Conflict,
            };
            Some(StatusEntry {
                area: StatusArea::Worktree,
                kind,
                path,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repository() -> (TempDir, RepositoryManager) {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        gix::init(directory.path()).expect("repository should initialize");
        let manager =
            RepositoryManager::discover(directory.path()).expect("repository should be discovered");
        (directory, manager)
    }

    fn add_linked_fixture(main: &Path, id: &str, checkout: &Path) {
        let admin = main.join(".git/worktrees").join(id);
        fs::create_dir_all(&admin).expect("administrative directory should be created");
        fs::create_dir_all(checkout).expect("checkout directory should be created");
        fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .expect("checkout backlink should be written");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", checkout.join(".git").display()),
        )
        .expect("worktree location should be written");
        fs::write(admin.join("commondir"), "../..\n").expect("common directory should be written");
        fs::write(admin.join("HEAD"), "ref: refs/heads/linked\n").expect("HEAD should be written");
    }

    #[test]
    fn abbreviates_object_ids() {
        assert_eq!(abbreviate_id("0123456789abcdef"), "01234567");
    }

    #[test]
    fn force_is_required_for_dirty_or_locked_worktrees() {
        assert!(
            !DeletionAssessment {
                changes: Vec::new(),
                locked: false,
                missing: false,
            }
            .requires_force()
        );
        assert!(
            DeletionAssessment {
                changes: vec![StatusEntry {
                    area: StatusArea::Worktree,
                    kind: ChangeKind::Modified,
                    path: "changed.txt".to_owned(),
                }],
                locked: false,
                missing: false,
            }
            .requires_force()
        );
        assert!(
            DeletionAssessment {
                changes: Vec::new(),
                locked: true,
                missing: false,
            }
            .requires_force()
        );
    }

    #[test]
    fn discovers_from_a_nested_directory_and_lists_main_worktree() {
        let (directory, _) = init_repository();
        let nested = directory.path().join("one/two");
        fs::create_dir_all(&nested).expect("nested directory should be created");

        let manager =
            RepositoryManager::discover(&nested).expect("nested repository should be discovered");
        let worktrees = manager
            .list_worktrees()
            .expect("worktrees should be listed");

        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].key, WorktreeKey::Main);
        assert_eq!(worktrees[0].reference_label(), "main");
        assert!(worktrees[0].path.is_absolute());
    }

    #[test]
    fn reports_untracked_files_as_worktree_changes() {
        let (directory, manager) = init_repository();
        fs::write(directory.path().join("untracked.txt"), "content")
            .expect("untracked file should be written");

        let status = manager
            .status(&WorktreeKey::Main)
            .expect("status should be calculated");

        assert!(status.entries.iter().any(|entry| {
            entry.area == StatusArea::Worktree
                && entry.kind == ChangeKind::Added
                && entry.path == "untracked.txt"
        }));
    }

    #[test]
    fn deletes_a_valid_clean_linked_worktree_but_keeps_shared_data() {
        let (directory, manager) = init_repository();
        let checkout = directory.path().join("linked");
        add_linked_fixture(directory.path(), "linked", &checkout);
        let shared_head = directory.path().join(".git/HEAD");

        let result = manager
            .delete_worktree(&WorktreeKey::Linked("linked".to_owned()), false)
            .expect("linked worktree should be deleted");

        assert_eq!(result, DeleteResult::Deleted);
        assert!(!checkout.exists());
        assert!(!directory.path().join(".git/worktrees/linked").exists());
        assert!(shared_head.exists());
    }

    #[test]
    fn refuses_to_delete_main_worktree() {
        let (_directory, manager) = init_repository();
        let error = manager
            .assess_deletion(&WorktreeKey::Main)
            .expect_err("main worktree deletion should be rejected");

        assert!(error.to_string().contains("main worktree"));
    }

    #[test]
    fn limits_history_to_two_hundred_commits() {
        let (directory, manager) = init_repository();
        let repository = gix::open(directory.path()).expect("repository should open");
        let signature = gix::actor::SignatureRef {
            name: b"Test Author".as_bstr(),
            email: b"test@example.invalid".as_bstr(),
            time: "1700000000 +0000",
        };
        let tree = repository.empty_tree().id;
        let mut parent = None;
        for number in 0..201 {
            let parents = parent.iter().copied();
            let commit = repository
                .commit_as(
                    signature,
                    signature,
                    "HEAD",
                    format!("commit {number}"),
                    tree,
                    parents,
                )
                .expect("commit should be created")
                .detach();
            parent = Some(commit);
        }

        let entries = manager
            .log(&WorktreeKey::Main)
            .expect("history should be loaded");

        assert_eq!(entries.len(), LOG_LIMIT);
        assert_eq!(entries[0].subject, "commit 200");
        assert_eq!(entries[0].author, "Test Author");
        let worktrees = manager
            .list_worktrees()
            .expect("worktrees should be listed");
        assert_eq!(worktrees[0].committed_at, Some(1_700_000_000));
    }

    #[test]
    fn locked_worktree_requires_force() {
        let (directory, manager) = init_repository();
        let checkout = directory.path().join("linked");
        add_linked_fixture(directory.path(), "linked", &checkout);
        fs::write(
            directory.path().join(".git/worktrees/linked/locked"),
            "reason",
        )
        .expect("lock should be written");

        let result = manager
            .delete_worktree(&WorktreeKey::Linked("linked".to_owned()), false)
            .expect("assessment should succeed");

        assert!(matches!(
            result,
            DeleteResult::NeedsForce(DeletionAssessment { locked: true, .. })
        ));
        assert!(checkout.exists());
    }

    #[test]
    fn dirty_worktree_requires_force_before_deletion() {
        let (directory, manager) = init_repository();
        let checkout = directory.path().join("linked");
        add_linked_fixture(directory.path(), "linked", &checkout);
        fs::write(checkout.join("untracked.txt"), "local data")
            .expect("untracked file should be written");
        let key = WorktreeKey::Linked("linked".to_owned());

        let result = manager
            .delete_worktree(&key, false)
            .expect("assessment should succeed");
        let DeleteResult::NeedsForce(assessment) = result else {
            panic!("dirty worktree should require force");
        };
        assert_eq!(assessment.changes.len(), 1);
        assert_eq!(assessment.changes[0].area, StatusArea::Worktree);
        assert_eq!(assessment.changes[0].kind, ChangeKind::Added);
        assert_eq!(assessment.changes[0].path, "untracked.txt");
        assert!(checkout.exists());

        let result = manager
            .delete_worktree(&key, true)
            .expect("forced deletion should succeed");
        assert_eq!(result, DeleteResult::Deleted);
        assert!(!checkout.exists());
    }

    #[test]
    fn removes_stale_metadata_for_a_missing_worktree() {
        let (directory, manager) = init_repository();
        let checkout = directory.path().join("linked");
        add_linked_fixture(directory.path(), "linked", &checkout);
        fs::remove_dir_all(&checkout).expect("checkout fixture should be removed");

        let result = manager
            .delete_worktree(&WorktreeKey::Linked("linked".to_owned()), false)
            .expect("stale worktree should be pruned");

        assert_eq!(result, DeleteResult::Deleted);
        assert!(!directory.path().join(".git/worktrees/linked").exists());
    }
}
