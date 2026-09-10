mod annotation;
mod app;
mod change_nav;
mod context;
mod coordinates;
mod diff_algo;
pub mod git;
mod global_search;
pub mod highlight;
mod render;
mod search;
mod state;
mod sticky_lines;
mod text_edit;
pub mod theme;
mod types;
mod watcher;

use std::collections::HashSet;
use std::io;
use std::process::{self, Command};
use std::thread;

use spinoff::{spinners, Color, Spinner, Streams};

use crate::commit_reference::CommitReference;
use crate::vcs::VcsBackend;

pub struct DiffOptions {
    pub reference: Option<CommitReference>,
    pub pr: Option<String>,
    pub detect_pr: bool,
    pub worktree_guidance: bool,
    pub file: Option<Vec<String>>,
    pub watch: bool,
    pub theme: Option<String>,
    pub stacked: bool,
    pub focus: Option<String>,
    pub origin: Option<String>,
    pub wrap: bool,
}

#[derive(Clone)]
pub struct PrInfo {
    pub number: u64,
    pub node_id: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub base_ref: String,
    pub head_ref: String,
    pub base_commit: String,
    pub head_commit: String,
    pub base_repo_owner: String,
    pub head_repo_owner: Option<String>, // None if head repo was deleted (fork deleted)
    pub head_repo: Option<String>,
}

impl PrInfo {
    fn annotation_context(&self, worktree_guidance: bool) -> String {
        let source = self
            .head_repo
            .as_ref()
            .map(|repo| format!("https://github.com/{}.git", repo))
            .unwrap_or_else(|| "unavailable (the source repository was deleted)".to_string());
        let mut context = format!(
            "PR: https://github.com/{}/{}/pull/{}\n\n\
             Base repository: https://github.com/{}/{}.git\n\n\
             Source repository: {}\n\n\
             Source branch: `{}`\n\n\
             Reviewed head commit: `{}`",
            self.repo_owner, self.repo_name, self.number,
            self.base_repo_owner, self.repo_name, source, self.head_ref, self.head_commit,
        );
        if worktree_guidance {
            context.push_str(
                "\n\n## Worktree instructions for the coding agent\n\n\
                 Apply the annotations below in a worktree associated with this PR.\n\n\
                 1. Locate the local repository by matching its remotes to the source or base \
                 repository above. Do not assume the current checkout is the PR's source.\n\
                 2. Inspect registered worktrees before creating anything. Reuse an existing \
                 worktree associated with this PR or its source repository and branch. Do not \
                 create a new worktree for each annotation round.\n\
                 3. Fetch and verify the current source branch head. If the matching worktree \
                 is already up to date, use it. If it is clean and behind, fast-forward it. \
                 Preserve uncommitted edits and local commits; do not reset, discard, or \
                 overwrite them. If it has diverged, report the conflict instead of silently \
                 creating another worktree.\n\
                 4. Only when no matching worktree exists, create one for the source branch \
                 and retain its association with this PR for later annotation rounds. If the \
                 source repository or branch is unavailable, report that before editing.\n\
                 5. Run edits and validation from the selected worktree. These annotations \
                 refer to the reviewed head commit above: if the source branch has advanced, \
                 inspect that commit to map the comments to the current code before applying them.\n\n\
                 Lumen only supplies this handoff; it has not created, updated, or switched a worktree.",
            );
        }
        context
    }
}

fn parse_pr_input(input: &str) -> Option<(Option<String>, Option<String>, u64)> {
    // Try to parse as a URL first
    if input.starts_with("http://") || input.starts_with("https://") {
        // Extract PR number and repo info from URL
        // Format: https://github.com/owner/repo/pull/123
        let parts: Vec<&str> = input.trim_end_matches('/').split('/').collect();
        if parts.len() >= 2 {
            if let Some(pos) = parts.iter().position(|&p| p == "pull") {
                if pos + 1 < parts.len() {
                    if let Ok(num) = parts[pos + 1].parse::<u64>() {
                        // Extract owner and repo
                        if pos >= 2 {
                            let owner = parts[pos - 2].to_string();
                            let repo = parts[pos - 1].to_string();
                            return Some((Some(owner), Some(repo), num));
                        }
                        return Some((None, None, num));
                    }
                }
            }
        }
        None
    } else {
        // Try to parse as a PR number
        input.parse::<u64>().ok().map(|num| (None, None, num))
    }
}

fn resolve_origin_repo() -> Result<String, String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .map_err(|e| format!("Failed to run git: {}", e))?;
    if !output.status.success() {
        return Err(
            "Could not determine repository. Set origin remote or use --origin owner/repo"
                .to_string(),
        );
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let url = url.strip_suffix(".git").unwrap_or(&url);
    let path = url
        .split("github.com")
        .nth(1)
        .ok_or_else(|| format!("Origin URL is not a GitHub URL: {}", url))?;
    let path = path.trim_start_matches(':').trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 2 {
        Ok(format!("{}/{}", parts[0], parts[1]))
    } else {
        Err(format!("Could not parse owner/repo from origin URL: {}", url))
    }
}

fn fetch_pr_info(pr_input: &str, repo_override: Option<&str>) -> Result<PrInfo, String> {
    let (owner, repo, number) = parse_pr_input(pr_input).ok_or_else(|| {
        format!(
            "Invalid PR reference: {}. Use a PR number or URL.",
            pr_input
        )
    })?;

    let repo_full = match (&owner, &repo, repo_override) {
        (Some(o), Some(r), _) => format!("{}/{}", o, r),
        (_, _, Some(r)) => r.to_string(),
        _ => resolve_origin_repo()?,
    };

    let (repo_owner, repo_name) = {
        let parts: Vec<&str> = repo_full.split('/').collect();
        if parts.len() != 2 {
            return Err(format!("Invalid repo format: {}", repo_full));
        }
        (
            owner.unwrap_or_else(|| parts[0].to_string()),
            repo.unwrap_or_else(|| parts[1].to_string()),
        )
    };

    // Use GraphQL to get the PR node ID, branch refs, and repo owners
    let query = format!(
        r#"query {{ repository(owner: "{}", name: "{}") {{ pullRequest(number: {}) {{ id url baseRefName headRefName baseRefOid headRefOid baseRepository {{ owner {{ login }} }} headRepository {{ nameWithOwner owner {{ login }} }} }} }} }}"#,
        repo_owner, repo_name, number
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", query)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh api graphql failed: {}", stderr.trim()));
    }

    parse_pr_info_response(
        &String::from_utf8_lossy(&output.stdout),
        number,
        repo_owner,
        repo_name,
    )
}

fn parse_pr_info_response(
    response: &str,
    number: u64,
    repo_owner: String,
    repo_name: String,
) -> Result<PrInfo, String> {
    let json: serde_json::Value = serde_json::from_str(response)
        .map_err(|e| format!("Could not parse PR GraphQL response: {}", e))?;
    let pr = &json["data"]["repository"]["pullRequest"];
    let node_id = pr["id"]
        .as_str()
        .ok_or_else(|| "Could not parse PR node ID from GraphQL response".to_string())?
        .to_string();
    let base_ref = pr["baseRefName"].as_str().unwrap_or("base").to_string();
    let head_ref = pr["headRefName"].as_str().unwrap_or("head").to_string();
    let base_commit = pr["baseRefOid"]
        .as_str()
        .ok_or_else(|| "Could not parse PR base commit".to_string())?
        .to_string();
    let head_commit = pr["headRefOid"]
        .as_str()
        .ok_or_else(|| "Could not parse PR head commit".to_string())?
        .to_string();
    let base_repo_owner = pr["baseRepository"]["owner"]["login"]
        .as_str()
        .unwrap_or(&repo_owner)
        .to_string();
    let head_repo_owner = pr["headRepository"]["owner"]["login"]
        .as_str()
        .map(str::to_string);
    let head_repo = if pr["headRepository"].is_null() {
        None
    } else {
        Some(
            pr["headRepository"]["nameWithOwner"]
                .as_str()
                .ok_or_else(|| {
                    "Could not parse head repository name from GraphQL response".to_string()
                })?
                .to_string(),
        )
    };

    Ok(PrInfo {
        number,
        node_id,
        repo_owner,
        repo_name,
        base_ref,
        head_ref,
        base_commit,
        head_commit,
        base_repo_owner,
        head_repo_owner,
        head_repo,
    })
}

/// Fetch the list of files that are marked as viewed on GitHub
pub fn fetch_viewed_files(pr_info: &PrInfo) -> Result<HashSet<String>, String> {
    let query = format!(
        r#"query {{ repository(owner: "{}", name: "{}") {{ pullRequest(number: {}) {{ files(first: 100) {{ nodes {{ path viewerViewedState }} }} }} }} }}"#,
        pr_info.repo_owner, pr_info.repo_name, pr_info.number
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", query)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh api graphql failed: {}", stderr.trim()));
    }

    let json_str = String::from_utf8_lossy(&output.stdout);

    // Parse the response to find viewed files
    // Look for patterns like: "path":"filename","viewerViewedState":"VIEWED"
    let mut viewed_files = HashSet::new();

    // Simple parsing: find all path/viewerViewedState pairs
    let mut remaining = json_str.as_ref();
    while let Some(path_start) = remaining.find("\"path\":\"") {
        let path_value_start = path_start + 8;
        let after_path = &remaining[path_value_start..];
        if let Some(path_end) = after_path.find('"') {
            let path = &after_path[..path_end];

            // Look for viewerViewedState after this path
            let after_path_str = &after_path[path_end..];
            if let Some(state_start) = after_path_str.find("\"viewerViewedState\":\"") {
                let state_value_start = state_start + 21;
                let after_state = &after_path_str[state_value_start..];
                if let Some(state_end) = after_state.find('"') {
                    let state = &after_state[..state_end];
                    if state == "VIEWED" {
                        viewed_files.insert(path.to_string());
                    }
                }
            }

            remaining = &remaining[path_value_start + path_end..];
        } else {
            break;
        }
    }

    Ok(viewed_files)
}

/// Mark a file as viewed on GitHub PR (non-blocking, spawns a thread)
pub fn mark_file_as_viewed_async(pr_info: &PrInfo, file_path: &str) {
    let node_id = pr_info.node_id.clone();
    let path = file_path.to_string();

    thread::spawn(move || {
        let _ = mark_file_as_viewed_sync(&node_id, &path);
    });
}

/// Unmark a file as viewed on GitHub PR (non-blocking, spawns a thread)
pub fn unmark_file_as_viewed_async(pr_info: &PrInfo, file_path: &str) {
    let node_id = pr_info.node_id.clone();
    let path = file_path.to_string();

    thread::spawn(move || {
        let _ = unmark_file_as_viewed_sync(&node_id, &path);
    });
}

/// Mark a file as viewed on GitHub PR (blocking)
fn mark_file_as_viewed_sync(node_id: &str, file_path: &str) -> Result<(), String> {
    let mutation = format!(
        r#"mutation {{ markFileAsViewed(input: {{ pullRequestId: "{}", path: "{}" }}) {{ clientMutationId }} }}"#,
        node_id, file_path
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", mutation)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }

    Ok(())
}

/// Unmark a file as viewed on GitHub PR (blocking)
fn unmark_file_as_viewed_sync(node_id: &str, file_path: &str) -> Result<(), String> {
    let mutation = format!(
        r#"mutation {{ unmarkFileAsViewed(input: {{ pullRequestId: "{}", path: "{}" }}) {{ clientMutationId }} }}"#,
        node_id, file_path
    );

    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={}", mutation)])
        .output()
        .map_err(|e| format!("Failed to run gh api graphql: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }

    Ok(())
}

fn detect_current_branch_pr() -> Result<String, String> {
    let output = Command::new("gh")
        .args(["pr", "view", "--json", "number", "-q", ".number"])
        .output()
        .map_err(|e| format!("Failed to run gh: {}", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            return Err("No PR found for the current branch".to_string());
        }
        return Err(msg.to_string());
    }
    let number = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if number.is_empty() {
        return Err("No PR found for the current branch".to_string());
    }
    Ok(number)
}

pub fn run_diff_ui(mut options: DiffOptions, backend: &dyn VcsBackend) -> io::Result<()> {
    // Resolve --detect-pr into options.pr
    if options.detect_pr && options.pr.is_none() {
        let mut spinner = Spinner::new_with_stream(
            spinners::Dots,
            "Detecting PR for current branch",
            Color::Cyan,
            Streams::Stderr,
        );
        match detect_current_branch_pr() {
            Ok(number) => {
                spinner.success(&format!("Detected PR #{}", number));
                options.pr = Some(number);
            }
            Err(e) => {
                spinner.fail(&e);
                process::exit(1);
            }
        }
    }

    // Handle PR mode
    if let Some(ref pr_input) = options.pr {
        let spinner_msg = match parse_pr_input(pr_input) {
            Some((Some(owner), Some(repo), number)) => {
                format!("Fetching PR {}/{}#{}", owner, repo, number)
            }
            Some((_, _, number)) => {
                format!("Fetching PR #{}", number)
            }
            None => "Fetching PR".to_string(),
        };
        let mut spinner = Spinner::new_with_stream(
            spinners::Dots,
            spinner_msg,
            Color::Cyan,
            Streams::Stderr,
        );
        match fetch_pr_info(pr_input, options.origin.as_deref()) {
            Ok(pr_info) => {
                spinner.success("Fetched PR metadata");
                return app::run_app_with_pr(options, pr_info, backend);
            }
            Err(e) => {
                spinner.fail(&e);
                process::exit(1);
            }
        }
    }

    // Also check if the reference looks like a PR (number or URL)
    if let Some(CommitReference::Single(ref input)) = options.reference {
        if input.contains("/pull/") || input.parse::<u64>().is_ok() {
            let spinner_msg = match parse_pr_input(input) {
                Some((Some(owner), Some(repo), number)) => {
                    format!("Fetching PR {}/{}#{}", owner, repo, number)
                }
                Some((_, _, number)) => {
                    format!("Fetching PR #{}", number)
                }
                None => "Fetching PR".to_string(),
            };
            let mut spinner = Spinner::new_with_stream(
            spinners::Dots,
            spinner_msg,
            Color::Cyan,
            Streams::Stderr,
        );
            match fetch_pr_info(input, options.origin.as_deref()) {
                Ok(pr_info) => {
                    spinner.success("Fetched PR metadata");
                    return app::run_app_with_pr(options, pr_info, backend);
                }
                Err(e) => {
                    spinner.fail(&e);
                    process::exit(1);
                }
            }
        }
    }

    if options.worktree_guidance {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--worktree-guidance requires a pull request (URL, number, --pr, or --detect-pr)",
        ));
    }

    // Handle stacked mode for range references
    if options.stacked {
        if let Some(ref reference) = options.reference {
            let (from, to) = match reference {
                CommitReference::Range { from, to } => (from.clone(), to.clone()),
                CommitReference::TripleDots { from, to } => {
                    // Get merge-base for triple dots
                    let merge_base = backend
                        .get_merge_base(from, to)
                        .unwrap_or_else(|_| from.clone());
                    (merge_base, to.clone())
                }
                CommitReference::Single(_) | CommitReference::RangeToWorkingTree { .. } => {
                    eprintln!(
                        "\x1b[91merror:\x1b[0m --stacked requires a range (e.g., main..feature)"
                    );
                    process::exit(1);
                }
            };

            let commits = match backend.get_commits_in_range(&from, &to) {
                Ok(c) if c.is_empty() => {
                    eprintln!(
                        "\x1b[91merror:\x1b[0m No commits found in range {}..{}",
                        from, to
                    );
                    process::exit(1);
                }
                Ok(c) => c,
                Err(e) => {
                    eprintln!("\x1b[91merror:\x1b[0m {}", e);
                    process::exit(1);
                }
            };

            return app::run_app_stacked(options, commits, backend);
        } else {
            eprintln!("\x1b[91merror:\x1b[0m --stacked requires a range (e.g., main..feature)");
            process::exit(1);
        }
    }

    app::run_app(options, None, backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_response(head_repository: serde_json::Value) -> String {
        serde_json::json!({
            "data": {"repository": {"pullRequest": {
                "id": "PR_test",
                "baseRefName": "main",
                "headRefName": "feature",
                "baseRefOid": "base-sha",
                "headRefOid": "head-sha",
                "baseRepository": {"owner": {"login": "upstream"}},
                "headRepository": head_repository
            }}}
        })
        .to_string()
    }

    #[test]
    fn worktree_handoff_identifies_fork_and_requests_reuse() {
        let response = pr_response(serde_json::json!({
            "nameWithOwner": "contributor/project-fork",
            "owner": {"login": "contributor"}
        }));
        let info =
            parse_pr_info_response(&response, 42, "upstream".into(), "project".into()).unwrap();
        let ordinary = info.annotation_context(false);
        assert!(ordinary.contains("https://github.com/upstream/project/pull/42"));
        assert!(ordinary.contains("https://github.com/contributor/project-fork.git"));
        assert!(ordinary.contains("Source branch: `feature`"));
        assert!(ordinary.contains("Reviewed head commit: `head-sha`"));
        assert!(!ordinary.contains("Worktree instructions"));
        let worktree = info.annotation_context(true);
        assert!(worktree.contains("Reuse an existing worktree"));
        assert!(worktree.contains("fast-forward"));
        assert!(worktree.contains("Preserve uncommitted edits and local commits"));
        assert!(worktree.contains("Only when no matching worktree exists"));
        assert!(worktree.contains("if the source branch has advanced"));
        assert!(worktree.contains("it has not created, updated, or switched a worktree"));
    }

    #[test]
    fn pr_info_preserves_renamed_fork_repository() {
        let response = pr_response(serde_json::json!({
            "nameWithOwner": "contributor/project-fork",
            "owner": {"login": "contributor"}
        }));
        let info =
            parse_pr_info_response(&response, 42, "upstream".into(), "project".into()).unwrap();
        assert_eq!(info.head_repo.as_deref(), Some("contributor/project-fork"));
        assert_eq!(info.head_repo_owner.as_deref(), Some("contributor"));
        assert_eq!(info.repo_name, "project");
        assert_eq!(info.base_repo_owner, "upstream");
        assert_eq!(info.base_ref, "main");
        assert_eq!(info.head_ref, "feature");
        assert_eq!(info.base_commit, "base-sha");
        assert_eq!(info.head_commit, "head-sha");
    }

    #[test]
    fn pr_info_handles_same_repository_and_deleted_fork() {
        let response = pr_response(serde_json::json!({
            "nameWithOwner": "upstream/project",
            "owner": {"login": "upstream"}
        }));
        let info =
            parse_pr_info_response(&response, 42, "upstream".into(), "project".into()).unwrap();
        assert_eq!(info.head_repo.as_deref(), Some("upstream/project"));

        let response = pr_response(serde_json::Value::Null);
        let info =
            parse_pr_info_response(&response, 42, "upstream".into(), "project".into()).unwrap();
        assert!(info.head_repo.is_none());
        assert!(info.head_repo_owner.is_none());
    }

    #[test]
    fn pr_info_rejects_missing_head_repository_name() {
        let response = pr_response(serde_json::json!({"owner": {"login": "contributor"}}));
        let result = parse_pr_info_response(&response, 42, "upstream".into(), "project".into());
        assert!(result.err().unwrap().contains("head repository name"));
    }
}
