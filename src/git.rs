use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use gix::{
    bstr::ByteSlice,
    remote::Direction,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchInfo {
    pub name: String,
    pub head: String,
    pub committed_at: i64,
    pub checked_out_paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChangeCounts {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub deleted: usize,
    pub conflicted: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamState {
    NotConfigured,
    Missing,
    Present,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchStatus {
    pub branch: String,
    pub upstream: Option<String>,
    pub upstream_state: UpstreamState,
    pub ahead: usize,
    pub behind: usize,
    pub checked_out_paths: Vec<PathBuf>,
    pub changes: ChangeCounts,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchDeletionAssessment {
    pub checked_out_paths: Vec<PathBuf>,
    pub unmerged: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchDeleteResult {
    Deleted,
    NeedsForce(BranchDeletionAssessment),
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

#[derive(Clone, Debug)]
pub struct RepositoryManager {
    main_git_dir: PathBuf,
    common_dir: PathBuf,
    root: PathBuf,
}

impl RepositoryManager {
    pub fn discover(path: &Path) -> Result<Self> {
        let repository = gix::discover(path).with_context(|| {
            format!(
                "failed to discover a Git repository from {}",
                path.display()
            )
        })?;
        Self::from_repository(&repository)
    }

    fn from_repository(repository: &gix::Repository) -> Result<Self> {
        let main = repository
            .main_repo()
            .context("failed to open the main repository")?;
        let main_git_dir = fs::canonicalize(main.git_dir())
            .with_context(|| format!("failed to validate {}", main.git_dir().display()))?;
        let common_dir = fs::canonicalize(main.common_dir())
            .with_context(|| format!("failed to validate {}", main.common_dir().display()))?;
        let root = main.worktree().map_or_else(
            || Ok::<_, anyhow::Error>(common_dir.clone()),
            |worktree| {
                fs::canonicalize(worktree.base())
                    .with_context(|| format!("failed to validate {}", worktree.base().display()))
            },
        )?;
        Ok(Self {
            main_git_dir,
            common_dir,
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
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

    pub fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        let main = self.open_main()?;
        let checked_out = self
            .list_worktrees()?
            .into_iter()
            .filter_map(|worktree| worktree.branch.map(|branch| (branch, worktree.path)))
            .fold(
                HashMap::<String, Vec<PathBuf>>::new(),
                |mut paths, (branch, path)| {
                    paths.entry(branch).or_default().push(path);
                    paths
                },
            );
        let reference_platform = main
            .references()
            .context("failed to access repository references")?;
        let references = reference_platform
            .local_branches()
            .context("failed to enumerate local branches")?
            .peeled()
            .context("failed to prepare local branch references")?;
        let mut branches = Vec::new();
        for reference in references {
            let reference = reference
                .map_err(|error| anyhow!("failed to read a local branch reference: {error}"))?;
            let name = reference.name().shorten().to_string();
            let id = reference
                .try_id()
                .ok_or_else(|| anyhow!("local branch {name} does not point to an object"))?;
            let (head, committed_at) = commit_details(&id)?;
            branches.push(BranchInfo {
                name: name.clone(),
                head,
                committed_at,
                checked_out_paths: checked_out.get(&name).cloned().unwrap_or_default(),
            });
        }
        branches.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(branches)
    }

    pub fn branch_status(&self, name: &str) -> Result<BranchStatus> {
        let main = self.open_main()?;
        let full_name: gix::refs::FullName = format!("refs/heads/{name}")
            .try_into()
            .with_context(|| format!("invalid local branch name {name}"))?;
        let mut reference = main
            .find_reference(&full_name)
            .with_context(|| format!("failed to find local branch {name}"))?;
        let branch_id = reference
            .peel_to_id()
            .with_context(|| format!("failed to resolve local branch {name}"))?;
        let checked_out_paths = self
            .list_branches()?
            .into_iter()
            .find(|branch| branch.name == name)
            .map(|branch| branch.checked_out_paths)
            .unwrap_or_default();
        let (upstream, upstream_state, ahead, behind) = match reference
            .remote_tracking_ref_name(Direction::Fetch)
            .transpose()
            .context("failed to resolve the branch upstream")?
        {
            Some(upstream_name) => {
                let upstream_label = upstream_name.shorten().to_string();
                match main.try_find_reference(upstream_name.as_ref())? {
                    Some(mut upstream_ref) => {
                        let upstream_id = upstream_ref
                            .peel_to_id()
                            .context("failed to resolve the branch upstream commit")?;
                        let branch_commits = reachable_commits(&branch_id)?;
                        let upstream_commits = reachable_commits(&upstream_id)?;
                        let ahead = branch_commits.difference(&upstream_commits).count();
                        let behind = upstream_commits.difference(&branch_commits).count();
                        (Some(upstream_label), UpstreamState::Present, ahead, behind)
                    }
                    None => (Some(upstream_label), UpstreamState::Missing, 0, 0),
                }
            }
            None => (None, UpstreamState::NotConfigured, 0, 0),
        };
        let mut changes = ChangeCounts::default();
        for worktree in self
            .list_worktrees()?
            .into_iter()
            .filter(|worktree| worktree.branch.as_deref() == Some(name) && worktree.available)
        {
            for entry in self.status(&worktree.key)?.entries {
                match (entry.area, entry.kind) {
                    (StatusArea::Index, ChangeKind::Deleted)
                    | (StatusArea::Worktree, ChangeKind::Deleted) => changes.deleted += 1,
                    (StatusArea::Index, ChangeKind::Conflict)
                    | (StatusArea::Worktree, ChangeKind::Conflict) => changes.conflicted += 1,
                    (StatusArea::Index, _) => changes.staged += 1,
                    (StatusArea::Worktree, ChangeKind::Added) => changes.untracked += 1,
                    (StatusArea::Worktree, _) => changes.unstaged += 1,
                }
            }
        }
        Ok(BranchStatus {
            branch: name.to_owned(),
            upstream,
            upstream_state,
            ahead,
            behind,
            checked_out_paths,
            changes,
        })
    }

    pub fn assess_branch_deletion(&self, name: &str) -> Result<BranchDeletionAssessment> {
        let main = self.open_main()?;
        let full_name: gix::refs::FullName = format!("refs/heads/{name}")
            .try_into()
            .with_context(|| format!("invalid local branch name {name}"))?;
        let mut reference = main
            .find_reference(&full_name)
            .with_context(|| format!("failed to find local branch {name}"))?;
        let branch_id = reference
            .peel_to_id()
            .with_context(|| format!("failed to resolve local branch {name}"))?;
        let checked_out_paths = self
            .list_branches()?
            .into_iter()
            .find(|branch| branch.name == name)
            .map(|branch| branch.checked_out_paths)
            .unwrap_or_default();
        let unmerged = main
            .head_id()
            .ok()
            .map(|head| {
                let branch_id = branch_id.detach();
                reachable_commits(&head).map(|commits| !commits.contains(&branch_id))
            })
            .transpose()?
            .unwrap_or(true);
        Ok(BranchDeletionAssessment {
            checked_out_paths,
            unmerged,
        })
    }

    pub fn delete_branch(&self, name: &str, force: bool) -> Result<BranchDeleteResult> {
        let assessment = self.assess_branch_deletion(name)?;
        if !assessment.checked_out_paths.is_empty() {
            bail!("branch {name} is checked out in a worktree");
        }
        if assessment.unmerged && !force {
            return Ok(BranchDeleteResult::NeedsForce(assessment));
        }
        let full_name: gix::refs::FullName = format!("refs/heads/{name}")
            .try_into()
            .with_context(|| format!("invalid local branch name {name}"))?;
        let mut main = self.open_main()?;
        main.delete_local_branches([full_name])
            .with_context(|| format!("failed to delete local branch {name}"))?;
        Ok(BranchDeleteResult::Deleted)
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
        log_from_id(&head)
    }

    pub fn log_branch(&self, name: &str) -> Result<Vec<LogEntry>> {
        let repository = self.open_main()?;
        let full_name: gix::refs::FullName = format!("refs/heads/{name}")
            .try_into()
            .with_context(|| format!("invalid local branch name {name}"))?;
        let mut reference = repository
            .find_reference(&full_name)
            .with_context(|| format!("failed to find local branch {name}"))?;
        let head = reference
            .peel_to_id()
            .with_context(|| format!("failed to resolve local branch {name}"))?;
        log_from_id(&head)
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

fn log_from_id(id: &gix::Id<'_>) -> Result<Vec<LogEntry>> {
    let walk = id
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

pub fn discover_repositories(path: &Path, recursive: bool) -> Result<Vec<RepositoryManager>> {
    if !recursive {
        return Ok(vec![RepositoryManager::discover(path)?]);
    }

    let search_root = fs::canonicalize(path)
        .with_context(|| format!("failed to access search directory {}", path.display()))?;
    if !search_root.is_dir() {
        bail!("search path {} is not a directory", search_root.display());
    }

    let mut repositories = Vec::new();
    match gix::discover(&search_root) {
        Ok(repository) => push_unique_repository(
            &mut repositories,
            RepositoryManager::from_repository(&repository)?,
        ),
        Err(gix::discover::Error::Discover(gix::discover::upwards::Error::NoGitRepository {
            ..
        })) => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to discover a Git repository from {}",
                    search_root.display()
                )
            });
        }
    }

    let mut children = fs::read_dir(&search_root)
        .with_context(|| format!("failed to read {}", search_root.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read entries in {}", search_root.display()))?;
    children.sort_by_key(std::fs::DirEntry::path);

    for child in children {
        if !child
            .file_type()
            .with_context(|| format!("failed to inspect {}", child.path().display()))?
            .is_dir()
            || !child.path().join(".git").exists()
        {
            continue;
        }
        let Ok(repository) = gix::open(child.path()) else {
            continue;
        };
        push_unique_repository(
            &mut repositories,
            RepositoryManager::from_repository(&repository)?,
        );
    }

    if repositories.is_empty() {
        bail!(
            "no Git repositories found in {} or its immediate children",
            search_root.display()
        );
    }
    repositories.sort_by(|left, right| left.root.cmp(&right.root));
    Ok(repositories)
}

fn push_unique_repository(
    repositories: &mut Vec<RepositoryManager>,
    repository: RepositoryManager,
) {
    if !repositories
        .iter()
        .any(|existing| existing.common_dir == repository.common_dir)
    {
        repositories.push(repository);
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

fn commit_details(id: &gix::Id<'_>) -> Result<(String, i64)> {
    let commit = id
        .object()
        .context("failed to read branch tip")?
        .try_into_commit()
        .map_err(|_| anyhow!("branch tip is not a commit"))?;
    let committed_at = commit
        .time()
        .context("failed to read branch commit time")?
        .seconds;
    Ok((abbreviate_id(&id.to_string()), committed_at))
}

fn reachable_commits(id: &gix::Id<'_>) -> Result<HashSet<gix::ObjectId>> {
    let mut commits = HashSet::new();
    for info in id
        .ancestors()
        .all()
        .context("failed to walk commit history")?
    {
        commits.insert(info.context("failed while walking commit history")?.id);
    }
    Ok(commits)
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

    #[test]
    fn lists_local_branches_and_reports_status() {
        let (_directory, manager) = init_repository();
        let repository = gix::open(manager.root()).expect("repository should open");
        let signature = gix::actor::SignatureRef {
            name: b"Test Author".as_bstr(),
            email: b"test@example.invalid".as_bstr(),
            time: "1700000000 +0000",
        };
        let tree = repository.empty_tree().id;
        repository
            .commit_as(
                signature,
                signature,
                "HEAD",
                "initial",
                tree,
                std::iter::empty::<gix::ObjectId>(),
            )
            .expect("initial commit should be created");

        let branches = manager.list_branches().expect("branches should be listed");
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].checked_out_paths.len(), 1);
        assert_eq!(branches[0].head.len(), 8);

        let status = manager
            .branch_status(&branches[0].name)
            .expect("branch status should be calculated");
        assert_eq!(status.upstream_state, UpstreamState::NotConfigured);
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);

        let head = repository
            .head_id()
            .expect("HEAD should resolve after initial commit")
            .detach();
        repository
            .reference(
                "refs/heads/topic",
                head,
                gix::refs::transaction::PreviousValue::MustNotExist,
                "create topic",
            )
            .expect("topic branch should be created");
        assert_eq!(
            manager
                .delete_branch("topic", false)
                .expect("topic branch should be deleted"),
            BranchDeleteResult::Deleted
        );
    }

    #[test]
    fn recursive_discovery_finds_only_immediate_child_repositories() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let alpha = directory.path().join("alpha");
        let beta = directory.path().join("beta");
        let broken = directory.path().join("broken/.git");
        let nested = directory.path().join("plain/nested");
        fs::create_dir_all(&broken).expect("broken metadata should be created");
        fs::create_dir_all(&nested).expect("nested directory should be created");
        gix::init(&alpha).expect("alpha repository should initialize");
        gix::init(&beta).expect("beta repository should initialize");
        gix::init(&nested).expect("nested repository should initialize");

        let repositories = discover_repositories(directory.path(), true)
            .expect("repositories should be discovered");
        let roots = repositories
            .iter()
            .map(|repository| repository.root().to_path_buf())
            .collect::<Vec<_>>();

        assert_eq!(
            roots,
            vec![
                fs::canonicalize(alpha).expect("alpha path should canonicalize"),
                fs::canonicalize(beta).expect("beta path should canonicalize"),
            ]
        );
        assert!(
            !roots.contains(&fs::canonicalize(nested).expect("nested path should canonicalize"))
        );
    }

    #[test]
    fn recursive_discovery_deduplicates_linked_checkouts() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let main = directory.path().join("main");
        let linked = directory.path().join("linked");
        gix::init(&main).expect("main repository should initialize");
        add_linked_fixture(&main, "linked", &linked);

        let repositories = discover_repositories(directory.path(), true)
            .expect("repositories should be discovered");

        assert_eq!(repositories.len(), 1);
        assert_eq!(
            repositories[0].root(),
            fs::canonicalize(main).expect("main path should canonicalize")
        );
        assert_eq!(
            repositories[0]
                .list_worktrees()
                .expect("worktrees should be listed")
                .len(),
            2
        );
    }
}
