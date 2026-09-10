use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine;
use spinoff::{spinners, Color, Spinner};

use super::types::{is_binary_content, FileDiff, FileStatus};
use super::{DiffOptions, PrInfo};
use crate::commit_reference::CommitReference;
use crate::vcs::VcsBackend;

/// Max concurrent `gh api` requests when fetching PR file contents.
/// GitHub's documented secondary rate limit caps concurrent requests at 100
/// (shared across REST+GraphQL); 8 keeps us comfortably under that while
/// still giving a large speedup over serial fetching.
const PR_FETCH_CONCURRENCY: usize = 8;

pub fn get_current_branch(backend: &dyn VcsBackend) -> String {
    backend
        .get_current_branch()
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Resolved references for diff comparison
pub enum DiffRefs {
    /// Uncommitted changes (working tree vs HEAD)
    WorkingTree,
    /// Single commit (SHA vs SHA^)
    Single(String),
    /// Range between two refs
    Range { from: String, to: String },
    /// Range from a ref to the working tree (range + uncommitted changes).
    RangeToWorkingTree { from: String },
}

impl DiffRefs {
    pub fn from_options(options: &DiffOptions, backend: &dyn VcsBackend) -> Self {
        match &options.reference {
            None => DiffRefs::WorkingTree,
            Some(CommitReference::Single(sha)) => DiffRefs::Single(sha.clone()),
            Some(CommitReference::Range { from, to }) => DiffRefs::Range {
                from: from.clone(),
                to: to.clone(),
            },
            Some(CommitReference::TripleDots { from, to }) => {
                // Get merge-base for triple dots
                let merge_base = backend.get_merge_base(from, to).unwrap_or_else(|e| {
                    eprintln!(
                        "Warning: failed to find merge-base for {}...{}: {}. Using '{}' as base.",
                        from, to, e, from
                    );
                    from.clone()
                });
                DiffRefs::Range {
                    from: merge_base,
                    to: to.clone(),
                }
            }
            Some(CommitReference::RangeToWorkingTree { from }) => {
                DiffRefs::RangeToWorkingTree { from: from.clone() }
            }
        }
    }
}

/// Get the list of files changed
pub fn get_changed_files(options: &DiffOptions, backend: &dyn VcsBackend) -> Vec<String> {
    let refs = DiffRefs::from_options(options, backend);

    let files: Vec<String> = match refs {
        DiffRefs::Single(sha) => backend.get_changed_files(&sha).unwrap_or_default(),
        DiffRefs::Range { from, to } => backend
            .get_range_changed_files(&from, &to)
            .unwrap_or_default(),
        DiffRefs::WorkingTree => backend.get_working_tree_changed_files().unwrap_or_default(),
        DiffRefs::RangeToWorkingTree { from } => {
            // Union of files changed in `from..HEAD` and files changed in the working tree.
            let head_ref = backend.working_copy_parent_ref();
            let range_files = backend
                .get_range_changed_files(&from, head_ref)
                .unwrap_or_default();
            let wt_files = backend.get_working_tree_changed_files().unwrap_or_default();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut combined: Vec<String> = Vec::new();
            for f in range_files.into_iter().chain(wt_files.into_iter()) {
                if seen.insert(f.clone()) {
                    combined.push(f);
                }
            }
            combined
        }
    };

    if let Some(ref filter) = options.file {
        files.into_iter().filter(|f| filter.contains(f)).collect()
    } else {
        files
    }
}

/// Get content of a file at the "old" side of the diff
pub fn get_old_content(filename: &str, refs: &DiffRefs, backend: &dyn VcsBackend) -> String {
    let ref_str = match refs {
        DiffRefs::Single(sha) => {
            // Use get_parent_ref_or_empty to handle root commits gracefully
            backend.get_parent_ref_or_empty(sha).unwrap_or_default()
        }
        DiffRefs::Range { from, .. } => from.clone(),
        DiffRefs::RangeToWorkingTree { from } => from.clone(),
        DiffRefs::WorkingTree => backend.working_copy_parent_ref().to_string(),
    };

    // Empty ref means root commit with no parent - return empty content
    if ref_str.is_empty() {
        return String::new();
    }

    backend
        .get_file_content_at_ref(&ref_str, Path::new(filename))
        .unwrap_or_default()
}

/// Get content of a file at the "new" side of the diff
pub fn get_new_content(filename: &str, refs: &DiffRefs, backend: &dyn VcsBackend) -> String {
    match refs {
        DiffRefs::Single(sha) => backend
            .get_file_content_at_ref(sha, Path::new(filename))
            .unwrap_or_default(),
        DiffRefs::Range { to, .. } => backend
            .get_file_content_at_ref(to, Path::new(filename))
            .unwrap_or_default(),
        DiffRefs::WorkingTree | DiffRefs::RangeToWorkingTree { .. } => {
            // Read from working tree (actual filesystem)
            fs::read_to_string(filename).unwrap_or_default()
        }
    }
}

pub fn load_file_diffs(options: &DiffOptions, backend: &dyn VcsBackend) -> Vec<FileDiff> {
    let refs = DiffRefs::from_options(options, backend);
    get_changed_files(options, backend)
        .into_iter()
        .map(|filename| {
            let old_content = get_old_content(&filename, &refs, backend);
            let new_content = get_new_content(&filename, &refs, backend);
            let status = if old_content.is_empty() && !new_content.is_empty() {
                FileStatus::Added
            } else if !old_content.is_empty() && new_content.is_empty() {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            };
            let is_binary =
                is_binary_content(&old_content) || is_binary_content(&new_content);
            FileDiff {
                filename,
                old_content,
                new_content,
                status,
                is_binary,
            }
        })
        .collect()
}

pub fn load_pr_file_diffs(pr_info: &PrInfo) -> Result<Vec<FileDiff>, String> {
    let repo_arg = format!("{}/{}", pr_info.repo_owner, pr_info.repo_name);

    let mut spinner = Spinner::new(
        spinners::Dots,
        format!(
            "Fetching file list for {}/{}#{}",
            pr_info.repo_owner, pr_info.repo_name, pr_info.number
        ),
        Color::Cyan,
    );

    // GitHub provides explicit file status and rename paths, including empty files.
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{}/pulls/{}/files?per_page=100", repo_arg, pr_info.number),
            "--paginate",
            "--slurp",
        ])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            let msg = format!("Failed to fetch PR file list: {}", e);
            spinner.fail(&msg);
            return Err(msg);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = format!("Failed to fetch PR file list: {}", stderr.trim());
        spinner.fail(&msg);
        return Err(msg);
    }

    let file_list = String::from_utf8_lossy(&output.stdout);
    let changed_files = parse_pr_changed_files(&file_list).map_err(|e| {
        spinner.fail(&e);
        e
    })?;
    let n = changed_files.len();

    if n == 0 {
        spinner.success("PR has no changed files");
        return Ok(Vec::new());
    }

    let base_repo = format!("{}/{}", pr_info.base_repo_owner, pr_info.repo_name);
    let head_repo = pr_info.head_repo.as_deref().unwrap_or(&base_repo);
    // PRs compare the common ancestor to the head, not the current base branch tip.
    let compare_path = format!(
        "repos/{}/compare/{}...{}",
        base_repo, pr_info.base_commit, pr_info.head_commit
    );
    let merge_base = github_api(&compare_path, "application/vnd.github+json")
        .and_then(|response| {
            let comparison: serde_json::Value = serde_json::from_str(&response)
                .map_err(|e| format!("Could not parse GitHub comparison: {}", e))?;
            comparison["merge_base_commit"]["sha"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "GitHub comparison did not return a merge base".to_string())
        })
        .map_err(|e| {
            spinner.fail(&e);
            e
        })?;

    let contents = fetch_pr_file_contents_parallel(
        &changed_files,
        &base_repo,
        &merge_base,
        head_repo,
        &pr_info.head_commit,
        &mut spinner,
    )
    .map_err(|e| {
        spinner.fail(&e);
        e
    })?;

    let file_diffs: Vec<FileDiff> = changed_files
        .into_iter()
        .zip(contents.into_iter())
        .map(|(file, (old_content, new_content))| {
            let is_binary =
                is_binary_content(&old_content) || is_binary_content(&new_content);
            FileDiff {
                filename: file.filename,
                old_content,
                new_content,
                status: file.status,
                is_binary,
            }
        })
        .collect();

    spinner.success(&format!("Fetched {} files", n));
    Ok(file_diffs)
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

struct FetchTask {
    idx: usize,
    filename: String,
    repo: String,
    git_ref: String,
    side: Side,
}

enum FetchEvent {
    Started(String),
    Finished {
        idx: usize,
        side: Side,
        filename: String,
        content: Result<String, String>,
    },
}

/// Fetch (old, new) contents for every changed file using a bounded worker
/// pool, updating `spinner` with live progress.
fn fetch_pr_file_contents_parallel(
    files: &[PrChangedFile],
    base_repo: &str,
    base_ref: &str,
    head_repo: &str,
    head_ref: &str,
    spinner: &mut Spinner,
) -> Result<Vec<(String, String)>, String> {
    let n = files.len();
    let mut tasks: Vec<FetchTask> = Vec::with_capacity(2 * n);
    for (idx, file) in files.iter().enumerate() {
        for (path, repo, git_ref, side) in [
            (&file.old_path, base_repo, base_ref, Side::Old),
            (&file.new_path, head_repo, head_ref, Side::New),
        ] {
            // Added/deleted files have only one side. Never mistake a failed
            // download for an absent side of the diff.
            if let Some(path) = path {
                tasks.push(FetchTask {
                    idx,
                    filename: path.clone(),
                    repo: repo.to_string(),
                    git_ref: git_ref.to_string(),
                    side,
                });
            }
        }
    }
    // Pop from the back, so process files in listed order.
    tasks.reverse();

    let total = tasks.len();
    let queue = Arc::new(Mutex::new(tasks));
    let (tx, rx) = mpsc::channel::<FetchEvent>();

    let worker_count = PR_FETCH_CONCURRENCY.min(total);
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        handles.push(thread::spawn(move || loop {
            let task = { queue.lock().unwrap().pop() };
            let Some(task) = task else { break };
            let _ = tx.send(FetchEvent::Started(task.filename.clone()));
            let content = fetch_file_content_from_github(&task.repo, &task.git_ref, &task.filename);
            let _ = tx.send(FetchEvent::Finished {
                idx: task.idx,
                side: task.side,
                filename: task.filename,
                content,
            });
        }));
    }
    drop(tx);

    let mut contents: Vec<(String, String)> = vec![(String::new(), String::new()); n];
    let mut done = 0usize;
    let mut first_error = None;
    let mut in_flight: Vec<String> = Vec::new();
    let mut last_finished: Option<String> = None;

    while let Ok(ev) = rx.recv() {
        match ev {
            FetchEvent::Started(name) => {
                in_flight.push(name);
            }
            FetchEvent::Finished {
                idx,
                side,
                filename,
                content,
            } => {
                if let Some(pos) = in_flight.iter().position(|f| f == &filename) {
                    in_flight.swap_remove(pos);
                }
                match content {
                    Ok(content) => match side {
                        Side::Old => contents[idx].0 = content,
                        Side::New => contents[idx].1 = content,
                    },
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
                done += 1;
                last_finished = Some(filename);
            }
        }
        spinner.update_text(format_fetch_progress(done, total, &in_flight, last_finished.as_deref()));
    }

    for h in handles {
        if h.join().is_err() {
            first_error.get_or_insert_with(|| "PR file download worker failed".to_string());
        }
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(contents),
    }
}

fn format_fetch_progress(
    done: usize,
    total: usize,
    in_flight: &[String],
    last_finished: Option<&str>,
) -> String {
    let current = if let Some(name) = in_flight.last() {
        name.as_str()
    } else if let Some(name) = last_finished {
        name
    } else {
        ""
    };
    if current.is_empty() {
        format!("Fetching files [{}/{}]", done, total)
    } else {
        format!("Fetching files [{}/{}] · {}", done, total, current)
    }
}

fn fetch_file_content_from_github(repo: &str, git_ref: &str, path: &str) -> Result<String, String> {
    let api_path = format!("repos/{}/contents/{}?ref={}", repo, path, git_ref);
    // Raw content can be changed by gh's output sanitization. Base64 keeps
    // literal escape sequences and binary data intact while passing through gh.
    let response = github_api(&api_path, "application/vnd.github+json")?;
    let mut file: serde_json::Value = serde_json::from_str(&response)
        .map_err(|e| format!("Could not parse file metadata for {}: {}", path, e))?;
    // The Contents API omits content above 1 MB; the blob API still provides it.
    if file["encoding"].as_str() == Some("none") {
        let sha = file["sha"]
            .as_str()
            .ok_or_else(|| format!("Missing blob SHA for {}", path))?;
        let response = github_api(
            &format!("repos/{}/git/blobs/{}", repo, sha),
            "application/vnd.github+json",
        )?;
        file = serde_json::from_str(&response)
            .map_err(|e| format!("Could not parse blob for {}: {}", path, e))?;
    }
    decode_github_content(&file).map_err(|e| format!("Failed to load {}: {}", path, e))
}

fn decode_github_content(file: &serde_json::Value) -> Result<String, String> {
    if file["encoding"].as_str() != Some("base64") {
        return Err("GitHub did not return base64 file content".to_string());
    }
    let encoded = file["content"]
        .as_str()
        .ok_or_else(|| "GitHub did not return file content".to_string())?;
    let encoded: String = encoded
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("Invalid base64 file content: {}", e))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn github_api(api_path: &str, accept: &str) -> Result<String, String> {
    let output = Command::new("gh")
        .args(["api", api_path, "-H", &format!("Accept: {}", accept)])
        .output()
        .map_err(|e| format!("Failed to run gh api for {}: {}", api_path, e))?;
    if !output.status.success() {
        return Err(format!(
            "Failed to fetch {}: {}",
            api_path,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[derive(Debug)]
struct PrChangedFile {
    filename: String,
    old_path: Option<String>,
    new_path: Option<String>,
    status: FileStatus,
}

fn parse_pr_changed_files(response: &str) -> Result<Vec<PrChangedFile>, String> {
    #[derive(serde::Deserialize)]
    struct GithubFile {
        filename: String,
        status: String,
        previous_filename: Option<String>,
    }
    let pages: Vec<Vec<GithubFile>> = serde_json::from_str(response)
        .map_err(|e| format!("Could not parse PR file list: {}", e))?;
    pages
        .into_iter()
        .flatten()
        .map(|file| {
            let status = match file.status.as_str() {
                "added" | "copied" => FileStatus::Added,
                "removed" => FileStatus::Deleted,
                "modified" | "renamed" | "changed" => FileStatus::Modified,
                other => return Err(format!("Unsupported PR file status: {}", other)),
            };
            let old_path = if status == FileStatus::Added {
                None
            } else if file.status == "renamed" {
                Some(file.previous_filename.ok_or_else(|| {
                    format!("Renamed PR file has no previous path: {}", file.filename)
                })?)
            } else {
                Some(file.filename.clone())
            };
            let new_path = if status == FileStatus::Deleted {
                None
            } else {
                Some(file.filename.clone())
            };
            Ok(PrChangedFile {
                filename: file.filename,
                old_path,
                new_path,
                status,
            })
        })
        .collect()
}

/// Load file diffs for a single commit (comparing commit to its parent).
/// Uses VcsBackend for backend-agnostic file content retrieval.
pub fn load_single_commit_diffs(
    commit_id: &str,
    file_filter: &Option<Vec<String>>,
    backend: &dyn VcsBackend,
) -> Vec<FileDiff> {
    // Get the list of changed files for this commit
    let files = backend.get_changed_files(commit_id).unwrap_or_default();

    let files: Vec<String> = if let Some(ref filter) = file_filter {
        files.into_iter().filter(|f| filter.contains(f)).collect()
    } else {
        files
    };

    // Get parent ref (handles root commits gracefully)
    let parent_ref = backend
        .get_parent_ref_or_empty(commit_id)
        .unwrap_or_default();

    files
        .into_iter()
        .map(|filename| {
            let path = Path::new(&filename);

            // Get old content (from parent commit)
            let old_content = if parent_ref.is_empty() {
                String::new()
            } else {
                backend
                    .get_file_content_at_ref(&parent_ref, path)
                    .unwrap_or_default()
            };

            // Get new content (from the commit itself)
            let new_content = backend
                .get_file_content_at_ref(commit_id, path)
                .unwrap_or_default();

            let status = if old_content.is_empty() && !new_content.is_empty() {
                FileStatus::Added
            } else if !old_content.is_empty() && new_content.is_empty() {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            };

            let is_binary =
                is_binary_content(&old_content) || is_binary_content(&new_content);
            FileDiff {
                filename,
                old_content,
                new_content,
                status,
                is_binary,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::test_utils::{git, make_temp_dir, RepoGuard};
    use crate::vcs::GitBackend;
    use std::fs;

    #[test]
    fn github_content_preserves_literal_control_escapes_and_empty_files() {
        let original = "literal \\u0000 and \\u0001\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(original);
        let file = serde_json::json!({"encoding": "base64", "content": format!("{}\n", encoded)});
        assert_eq!(decode_github_content(&file).unwrap(), original);
        let empty = serde_json::json!({"encoding": "base64", "content": ""});
        assert_eq!(decode_github_content(&empty).unwrap(), "");
        let invalid = serde_json::json!({"encoding": "base64", "content": "!!!"});
        assert!(decode_github_content(&invalid).is_err());
        assert!(decode_github_content(&serde_json::json!({})).is_err());
    }

    #[test]
    fn pr_diff_distinguishes_missing_sides_from_empty_files() {
        let response = r#"[
            [{"filename":"empty.txt","status":"added"},
             {"filename":"deleted.txt","status":"removed"}],
            [{"filename":"modified.txt","status":"modified"}]
        ]"#;
        let files = parse_pr_changed_files(response).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].status, FileStatus::Added);
        assert_eq!(files[0].old_path, None);
        assert_eq!(files[0].new_path.as_deref(), Some("empty.txt"));
        assert_eq!(files[1].status, FileStatus::Deleted);
        assert_eq!(files[1].old_path.as_deref(), Some("deleted.txt"));
        assert_eq!(files[1].new_path, None);
        assert_eq!(files[2].status, FileStatus::Modified);
        assert_eq!(files[2].old_path.as_deref(), Some("modified.txt"));
        assert_eq!(files[2].new_path.as_deref(), Some("modified.txt"));
    }

    #[test]
    fn pr_diff_fetches_renamed_files_from_their_original_path() {
        let response = r#"[[{
            "filename":"new.txt","status":"renamed","previous_filename":"old.txt"
        }]]"#;
        let files = parse_pr_changed_files(response).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "new.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("old.txt"));
        assert_eq!(files[0].new_path.as_deref(), Some("new.txt"));
        assert_eq!(files[0].status, FileStatus::Modified);
    }

    #[test]
    fn test_load_file_diffs_working_tree_untracked_in_new_dir() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-wt-untracked-dir");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Initial commit
        fs::write(dir.join("existing.txt"), "existing content\n").expect("write existing");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "initial"]);

        // Create untracked files in a NEW directory
        fs::create_dir_all(dir.join("new_dir")).expect("create new dir");
        fs::write(dir.join("new_dir/file1.txt"), "file 1\n").expect("write file1");
        fs::write(dir.join("new_dir/file2.txt"), "file 2\n").expect("write file2");

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let options = super::super::DiffOptions {
            reference: None,
            pr: None,
            detect_pr: false,
            file: None,
            watch: false,
            theme: None,
            stacked: false,
            focus: None,
            origin: None,
            wrap: false,
        };

        let diffs = load_file_diffs(&options, &backend);
        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();

        // Should have individual files from the untracked directory, not just "new_dir/"
        assert!(
            diffs.iter().any(|d| d.filename == "new_dir/file1.txt"),
            "should include new_dir/file1.txt, got: {:?}",
            filenames
        );
        assert!(
            diffs.iter().any(|d| d.filename == "new_dir/file2.txt"),
            "should include new_dir/file2.txt, got: {:?}",
            filenames
        );

        // Verify they're detected as Added with correct content
        let file1 = diffs
            .iter()
            .find(|d| d.filename == "new_dir/file1.txt")
            .unwrap();
        assert_eq!(file1.status, FileStatus::Added);
        assert!(file1.old_content.is_empty());
        assert_eq!(file1.new_content, "file 1\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_file_diffs_range_to_working_tree() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-range-to-wt");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Base commit (will be referenced as HEAD~1 later)
        fs::write(dir.join("base.txt"), "base\n").expect("write base");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "base"]);

        // Commit on top — committed change
        fs::write(dir.join("committed.txt"), "committed\n").expect("write committed");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "add committed"]);

        // Working tree changes (uncommitted)
        fs::write(dir.join("uncommitted.txt"), "uncommitted\n").expect("write uncommitted");
        fs::write(dir.join("base.txt"), "base modified\n").expect("modify base");

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = crate::vcs::GitBackend::from_cwd().expect("should open repo");
        let options = super::super::DiffOptions {
            reference: Some(crate::commit_reference::CommitReference::RangeToWorkingTree {
                from: "HEAD~1".to_string(),
            }),
            pr: None,
            detect_pr: false,
            file: None,
            watch: false,
            theme: None,
            stacked: false,
            focus: None,
            origin: None,
            wrap: false,
        };

        let diffs = load_file_diffs(&options, &backend);
        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();

        // Should include both the committed file and the uncommitted/modified ones
        assert!(
            filenames.contains(&"committed.txt"),
            "should include committed file, got: {:?}",
            filenames
        );
        assert!(
            filenames.contains(&"uncommitted.txt"),
            "should include untracked working tree file, got: {:?}",
            filenames
        );
        assert!(
            filenames.contains(&"base.txt"),
            "should include modified working tree file, got: {:?}",
            filenames
        );

        // base.txt: old=from HEAD~1 ("base\n"), new=working tree ("base modified\n")
        let base = diffs.iter().find(|d| d.filename == "base.txt").unwrap();
        assert_eq!(base.old_content, "base\n");
        assert_eq!(base.new_content, "base modified\n");

        // committed.txt: old=empty (not in HEAD~1), new=fs content
        let committed = diffs.iter().find(|d| d.filename == "committed.txt").unwrap();
        assert_eq!(committed.old_content, "");
        assert_eq!(committed.new_content, "committed\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_single_commit_diffs_added_file() {
        let _repo = RepoGuard::new();
        let backend = GitBackend::from_cwd().expect("should open repo");

        // HEAD is the initial commit with README.md added
        let diffs = load_single_commit_diffs("HEAD", &None, &backend);

        assert_eq!(diffs.len(), 1, "should have 1 file diff");
        assert_eq!(diffs[0].filename, "README.md");
        assert_eq!(diffs[0].status, FileStatus::Added);
        assert!(
            diffs[0].old_content.is_empty(),
            "old content should be empty for added file"
        );
        assert_eq!(diffs[0].new_content.trim(), "hello");
    }

    #[test]
    fn test_load_single_commit_diffs_modified_file() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-modified");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // First commit
        fs::write(dir.join("file.txt"), "original content\n").expect("write file");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);

        // Second commit - modify file
        fs::write(dir.join("file.txt"), "modified content\n").expect("modify file");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "second"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let diffs = load_single_commit_diffs("HEAD", &None, &backend);

        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].filename, "file.txt");
        assert_eq!(diffs[0].status, FileStatus::Modified);
        assert_eq!(diffs[0].old_content, "original content\n");
        assert_eq!(diffs[0].new_content, "modified content\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_single_commit_diffs_multiple_files() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-multi");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Commit with multiple files
        fs::write(dir.join("a.txt"), "file a\n").expect("write a");
        fs::write(dir.join("b.txt"), "file b\n").expect("write b");
        fs::write(dir.join("c.txt"), "file c\n").expect("write c");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "multi"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let diffs = load_single_commit_diffs("HEAD", &None, &backend);

        assert_eq!(diffs.len(), 3, "should have 3 file diffs");

        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();
        assert!(filenames.contains(&"a.txt"));
        assert!(filenames.contains(&"b.txt"));
        assert!(filenames.contains(&"c.txt"));

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_single_commit_diffs_with_filter() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-filter");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        fs::write(dir.join("wanted.txt"), "wanted\n").expect("write wanted");
        fs::write(dir.join("unwanted.txt"), "unwanted\n").expect("write unwanted");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "filter test"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let filter = Some(vec!["wanted.txt".to_string()]);
        let diffs = load_single_commit_diffs("HEAD", &filter, &backend);

        assert_eq!(diffs.len(), 1, "filter should limit to 1 file");
        assert_eq!(diffs[0].filename, "wanted.txt");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stacked_diff_integration_git() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-stacked-integration");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Base commit
        fs::write(dir.join("base.txt"), "base\n").expect("write base");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "base"]);

        // Commit A
        fs::write(dir.join("a.txt"), "commit A\n").expect("write a");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "commit A"]);

        // Commit B
        fs::write(dir.join("b.txt"), "commit B\n").expect("write b");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "commit B"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");

        // Get commits in range (simulating stacked diff)
        let commits = backend
            .get_commits_in_range("HEAD~2", "HEAD")
            .expect("should get commits");

        assert_eq!(commits.len(), 2, "should have 2 commits");
        assert_eq!(commits[0].summary, "commit A");
        assert_eq!(commits[1].summary, "commit B");

        // Load diffs for each commit (as stacked diff would do)
        let diffs_a = load_single_commit_diffs(&commits[0].commit_id, &None, &backend);
        assert_eq!(diffs_a.len(), 1);
        assert_eq!(diffs_a[0].filename, "a.txt");
        assert_eq!(diffs_a[0].new_content, "commit A\n");

        let diffs_b = load_single_commit_diffs(&commits[1].commit_id, &None, &backend);
        assert_eq!(diffs_b.len(), 1);
        assert_eq!(diffs_b[0].filename, "b.txt");
        assert_eq!(diffs_b[0].new_content, "commit B\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }
}
