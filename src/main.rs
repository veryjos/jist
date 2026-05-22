use clap::{Parser, Subcommand};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "Joe's Intermediate Tracker";
const SESSION_REMOTE: &str = "jit";
const SESSION_STATUS_LIMIT: usize = 5;

#[derive(Debug, Parser)]
#[command(name = "jit", version, about = APP_NAME)]
struct Cli {
    #[command(subcommand)]
    command: CommandArgs,
}

#[derive(Debug, Subcommand)]
enum CommandArgs {
    /// Initializes a new session with the given name on local and remote.
    SessionInit {
        /// Name for this JIT session.
        session_name: String,
        /// Skip the interactive private session repository warning.
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },
    /// Show JIT session status.
    Status,
    /// Push working tree changes into a JIT session branch.
    Push {
        /// Session name to push. Inferred when exactly one session exists.
        session_name: Option<String>,
    },
    /// Commit a JIT session branch to the original repository and remove the private session branch.
    Commit {
        /// Session name to commit. Inferred when exactly one session exists.
        session_name: Option<String>,
    },
    /// Load the commit from a JIT session branch into the working tree.
    Pull {
        /// Session name to pull. Inferred when exactly one session exists.
        session_name: Option<String>,
    },
    /// Reset the working tree to the base commit of a JIT session.
    Reset {
        /// Session name to reset. Inferred when exactly one session exists.
        session_name: Option<String>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        CommandArgs::SessionInit {
            session_name,
            quiet,
        } => session_init(&session_name, quiet),
        CommandArgs::Status => status(),
        CommandArgs::Push { session_name } => push(session_name.as_deref()),
        CommandArgs::Commit { session_name } => commit(session_name.as_deref()),
        CommandArgs::Pull { session_name } => pull(session_name.as_deref()),
        CommandArgs::Reset { session_name } => reset(session_name.as_deref()),
    }
}

fn session_init(session_name: &str, quiet: bool) -> ExitCode {
    match create_session_repo(session_name, quiet) {
        Ok(session_repo) => {
            println!("Initialized JIT session repository `{session_repo}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn push(session_name: Option<&str>) -> ExitCode {
    match push_session(session_name) {
        Ok(session_name) => {
            println!("Pushed JIT session `{session_name}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn pull(session_name: Option<&str>) -> ExitCode {
    match pull_session(session_name) {
        Ok(session_name) => {
            println!("Pulled JIT session `{session_name}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn commit(session_name: Option<&str>) -> ExitCode {
    match commit_session(session_name) {
        Ok(session_name) => {
            println!("Committed JIT session `{session_name}` to origin.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn reset(session_name: Option<&str>) -> ExitCode {
    match reset_session(session_name) {
        Ok(session_name) => {
            println!("Reset working tree to base of JIT session `{session_name}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn status() -> ExitCode {
    match session_statuses() {
        Ok(sessions) => {
            match sessions.iter().find(|session| session.active) {
                Some(session) => {
                    println!("Current session: \x1b[32m*{}\x1b[0m", session.name);

                    match session_banner_details(&session.name) {
                        Ok(details) => {
                            if let Err(error) = print_session_banner_details(&details) {
                                eprintln!("{error}");
                                return ExitCode::FAILURE;
                            }
                        }
                        Err(error) => {
                            eprintln!("{error}");
                            return ExitCode::FAILURE;
                        }
                    }
                }
                None => println!("Current session: \x1b[31mNo session\x1b[0m"),
            }

            println!();
            println!("Sessions:");

            if sessions.is_empty() {
                println!("<no sessions>");
            } else {
                let visible_sessions = visible_session_statuses(&sessions);

                for session in &visible_sessions {
                    let age = format_session_age(session.modified_at);
                    if session.active {
                        println!(
                            "- \x1b[32m*{}\x1b[0m (\x1b[32m*active\x1b[0m, last modified {age})",
                            session.name
                        );
                    } else {
                        println!("-  {} ({age})", session.name);
                    }
                }

                let remaining = sessions.len().saturating_sub(visible_sessions.len());
                if remaining > 0 {
                    println!("... (+ {remaining} with olderchanges)");
                }
            }

            println!();
            println!(
                "Run \x1b[34mjit session-init\x1b[0m with your base commit checked out to start a new session."
            );

            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

struct SessionStatus {
    name: String,
    active: bool,
    modified_at: i64,
}

struct SessionInfo {
    name: String,
    modified_at: i64,
}

fn visible_session_statuses(sessions: &[SessionStatus]) -> Vec<&SessionStatus> {
    let mut visible = Vec::with_capacity(SESSION_STATUS_LIMIT.min(sessions.len()));

    if let Some(active_session) = sessions.iter().find(|session| session.active) {
        visible.push(active_session);
    }

    visible.extend(
        sessions
            .iter()
            .filter(|session| !session.active)
            .take(SESSION_STATUS_LIMIT.saturating_sub(visible.len())),
    );

    visible
}

struct SessionBannerDetails {
    original_repo: String,
    session_repo: String,
    diff_base: String,
    working_commit: String,
}

fn create_session_repo(session_name: &str, quiet: bool) -> Result<String, String> {
    ensure_gh_installed()?;
    let session_name = normalize_ref_part(session_name, "session name")?;
    let session_branch = session_branch_name(&session_name)?;

    let session_repo = session_repository()?;

    confirm_private_session_repo(&session_repo, quiet)?;

    ensure_private_session_repo(&session_repo)?;
    push_session_ref(&session_repo, &session_branch)?;
    ensure_local_session_branch(&session_branch)?;

    Ok(session_repo)
}

fn session_repository() -> Result<String, String> {
    let repository_name = repository_name()?;
    let session_repo_name = format!("jit-{repository_name}");
    let owner = gh_output(["api", "user", "--jq", ".login"])?;

    Ok(format!("{owner}/{session_repo_name}"))
}

fn original_repository() -> Result<String, String> {
    gh_output([
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ])
}

fn ensure_private_session_repo(session_repo: &str) -> Result<(), String> {
    if gh_success(["repo", "view", session_repo])? {
        return Ok(());
    }

    gh_status(["repo", "create", session_repo, "--private"])
}

fn push_session_ref(session_repo: &str, session_branch: &str) -> Result<(), String> {
    ensure_session_remote(session_repo)?;

    let diff_base = git_output(["rev-parse", "HEAD"])?;

    git_status([
        "push",
        SESSION_REMOTE,
        &format!("{diff_base}:refs/heads/{session_branch}"),
    ])
}

fn ensure_local_session_branch(session_branch: &str) -> Result<(), String> {
    if local_branch_exists(session_branch)? {
        git_status(["switch", session_branch])
    } else {
        git_status(["switch", "-c", session_branch])
    }
}

fn local_branch_exists(branch_name: &str) -> Result<bool, String> {
    git_success([
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch_name}"),
    ])
}

fn confirm_private_session_repo(session_repo: &str, quiet: bool) -> Result<(), String> {
    if quiet {
        return Ok(());
    }

    eprintln!(
        "JIT will use GitHub CLI (`gh`) to create a PRIVATE session repository: {session_repo}"
    );
    eprintln!("This repository is intended to be PRIVATE. Continue? [y/N]");

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("Failed to read confirmation: {error}"))?;

    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        Ok(())
    } else {
        Err("Aborted before creating the private session repository.".to_owned())
    }
}

fn repository_name() -> Result<String, String> {
    let repo = gh_output(["repo", "view", "--json", "name", "--jq", ".name"])?;
    normalize_ref_part(&repo, "repository name")
}

fn ensure_gh_installed() -> Result<(), String> {
    Command::new("gh")
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|_| {
            "GitHub CLI (`gh`) is required for `jit session-init`. Install it from TODO: add install link."
                .to_owned()
        })
}

fn list_sessions() -> Result<Vec<SessionInfo>, String> {
    ensure_gh_installed()?;
    let session_repo = session_repository()?;
    ensure_session_remote(&session_repo)?;
    fetch_all_session_branches()?;

    let git_user_name = git_output(["config", "user.name"])?;
    let username = normalize_git_username(&git_user_name)?;
    let refs = git_output([
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname:short)%09%(committerdate:unix)",
        &format!("refs/remotes/{SESSION_REMOTE}/jit__{username}_*"),
    ])?;

    Ok(refs
        .lines()
        .filter_map(|line| session_from_remote_tracking_ref(line, &username))
        .collect())
}

fn session_statuses() -> Result<Vec<SessionStatus>, String> {
    let active_session = active_session()?;

    Ok(list_sessions()?
        .into_iter()
        .map(|session| SessionStatus {
            active: active_session.as_deref() == Some(session.name.as_str()),
            name: session.name,
            modified_at: session.modified_at,
        })
        .collect())
}

fn session_banner_details(session_name: &str) -> Result<SessionBannerDetails, String> {
    let original_repo = original_repository()?;
    let session_repo = session_repository()?;
    ensure_session_remote(&session_repo)?;

    let branch_name = session_branch_name(session_name)?;
    fetch_session_branch(&branch_name)?;

    let session_ref = format!("refs/remotes/{SESSION_REMOTE}/{branch_name}");
    let working_commit = remote_branch_commit(SESSION_REMOTE, &branch_name)?;
    let subject = git_output(["log", "-1", "--format=%s", &session_ref])?;
    let diff_base = if subject == "JIT sync" {
        git_output(["rev-parse", &format!("{session_ref}^")])?
    } else {
        working_commit.clone()
    };

    Ok(SessionBannerDetails {
        original_repo,
        session_repo,
        diff_base,
        working_commit,
    })
}

fn remote_branch_commit(remote: &str, branch_name: &str) -> Result<String, String> {
    let output = git_output(["ls-remote", "--heads", remote, branch_name])?;
    let (commit, _) = output
        .split_once('\t')
        .ok_or_else(|| format!("Branch `{branch_name}` was not found on `{remote}`."))?;

    Ok(commit.to_owned())
}

fn print_session_banner_details(details: &SessionBannerDetails) -> Result<(), String> {
    println!(
        "Diffbase: {}",
        commit_url_link(&details.original_repo, &details.diff_base)?
    );
    println!(
        "Working commit: {}",
        commit_url_link(&details.session_repo, &details.working_commit)?
    );

    Ok(())
}

fn commit_url_link(repo: &str, commit: &str) -> Result<String, String> {
    let short_commit = short_commit_hash(commit)?;
    let url = commit_url(repo, &short_commit);

    Ok(terminal_hyperlink(&url, &url))
}

fn commit_url(repo: &str, commit: &str) -> String {
    format!("https://github.com/{repo}/commit/{commit}")
}

fn short_commit_hash(commit: &str) -> Result<String, String> {
    git_output(["rev-parse", "--short", commit])
}

fn terminal_hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

fn active_session() -> Result<Option<String>, String> {
    let current_branch = git_output(["branch", "--show-current"])?;

    if current_branch.is_empty() {
        return Ok(None);
    }

    let git_user_name = git_output(["config", "user.name"])?;
    let username = normalize_git_username(&git_user_name)?;

    Ok(current_branch
        .strip_prefix(&format!("jit__{username}_"))
        .map(|session_name| session_name.to_owned()))
}

fn push_session(session_name: Option<&str>) -> Result<String, String> {
    let session_repo = session_repository()?;
    ensure_session_remote(&session_repo)?;

    let session_name = resolve_session_name(session_name, "push")?;
    let session_name = normalize_ref_part(&session_name, "session name")?;
    let branch_name = session_branch_name(&session_name)?;

    fetch_session_branch(&branch_name)?;

    let base_ref = format!("refs/remotes/{SESSION_REMOTE}/{branch_name}");
    let base_commit = git_output(["merge-base", "HEAD", &base_ref])?;
    let tree = write_worktree_tree()?;
    let commit = git_output(["commit-tree", &tree, "-p", &base_commit, "-m", "JIT sync"])?;

    git_status([
        "push",
        SESSION_REMOTE,
        &format!("+{commit}:refs/heads/{branch_name}"),
    ])?;

    Ok(session_name)
}

fn reset_session(session_name: Option<&str>) -> Result<String, String> {
    let branch_name = resolve_and_fetch_session_branch(session_name, "reset")?;
    let session_ref = format!("refs/remotes/{SESSION_REMOTE}/{branch_name}");
    let base_commit = git_output(["rev-parse", &format!("{session_ref}^")])?;

    git_status([
        "restore",
        "--source",
        &base_commit,
        "--staged",
        "--worktree",
        ".",
    ])?;

    session_display_name_from_branch_name(&branch_name)
}

fn pull_session(session_name: Option<&str>) -> Result<String, String> {
    let branch_name = resolve_and_fetch_session_branch(session_name, "pull")?;
    let session_ref = format!("refs/remotes/{SESSION_REMOTE}/{branch_name}");

    git_status([
        "restore",
        "--source",
        &session_ref,
        "--staged",
        "--worktree",
        ".",
    ])?;

    session_display_name_from_branch_name(&branch_name)
}

fn commit_session(session_name: Option<&str>) -> Result<String, String> {
    let branch_name = resolve_and_fetch_session_branch(session_name, "commit")?;
    let session_ref = format!("refs/remotes/{SESSION_REMOTE}/{branch_name}");
    let session_name = session_display_name_from_branch_name(&branch_name)?;

    ensure_origin_remote()?;

    git_status([
        "push",
        "origin",
        &format!("{session_ref}:refs/heads/{branch_name}"),
    ])?;
    git_status([
        "push",
        SESSION_REMOTE,
        &format!(":refs/heads/{branch_name}"),
    ])?;
    delete_remote_tracking_branch(&branch_name)?;

    Ok(session_name)
}

fn fetch_session_branch(branch_name: &str) -> Result<(), String> {
    git_status([
        "fetch",
        SESSION_REMOTE,
        &format!("+refs/heads/{branch_name}:refs/remotes/{SESSION_REMOTE}/{branch_name}"),
    ])
}

fn fetch_all_session_branches() -> Result<(), String> {
    git_status([
        "fetch",
        "--prune",
        SESSION_REMOTE,
        &format!("+refs/heads/jit__*:refs/remotes/{SESSION_REMOTE}/jit__*"),
    ])
}

fn resolve_and_fetch_session_branch(
    session_name: Option<&str>,
    command_name: &str,
) -> Result<String, String> {
    let session_repo = session_repository()?;
    ensure_session_remote(&session_repo)?;

    let session_name = resolve_session_name(session_name, command_name)?;
    let session_name = normalize_ref_part(&session_name, "session name")?;
    let branch_name = session_branch_name(&session_name)?;

    fetch_session_branch(&branch_name)?;

    Ok(branch_name)
}

fn session_display_name_from_branch_name(branch_name: &str) -> Result<String, String> {
    let git_user_name = git_output(["config", "user.name"])?;
    let username = normalize_git_username(&git_user_name)?;

    branch_name
        .strip_prefix(&format!("jit__{username}_"))
        .map(|session_name| session_name.to_owned())
        .ok_or_else(|| format!("Session branch `{branch_name}` does not belong to `{username}`."))
}

fn resolve_session_name(session_name: Option<&str>, command_name: &str) -> Result<String, String> {
    if let Some(session_name) = session_name {
        return Ok(session_name.to_owned());
    }

    let sessions = list_sessions()?;

    match sessions.as_slice() {
        [session] => Ok(session.name.to_owned()),
        [] => Err("No JIT sessions found. Run `jit session-init <name>` first.".to_owned()),
        _ => Err(format!(
            "Multiple JIT sessions found. Run `jit {command_name} <session-name>`."
        )),
    }
}

fn session_branch_name(session_name: &str) -> Result<String, String> {
    let git_user_name = git_output(["config", "user.name"])?;
    let username = normalize_git_username(&git_user_name)?;

    Ok(format!("jit__{username}_{session_name}"))
}

fn session_from_remote_tracking_ref(line: &str, username: &str) -> Option<SessionInfo> {
    let (ref_name, modified_at) = line.split_once('\t')?;
    let name = ref_name
        .strip_prefix(&format!("{SESSION_REMOTE}/jit__{username}_"))
        .map(|session_name| session_name.to_owned())?;
    let modified_at = modified_at.parse().ok()?;

    Some(SessionInfo { name, modified_at })
}

fn format_session_age(modified_at: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let seconds = now.saturating_sub(modified_at);
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;

    format!("{days}d, {hours}h, {minutes}m ago")
}

fn write_worktree_tree() -> Result<String, String> {
    let index_path = env::temp_dir().join(format!("jit-index-{}", std::process::id()));
    let index_path_string = index_path.to_string_lossy().to_string();

    let result = (|| {
        git_status_with_index(["read-tree", "HEAD"], &index_path_string)?;
        git_status_with_index(["add", "-A"], &index_path_string)?;
        git_output_with_index(["write-tree"], &index_path_string)
    })();

    let _ = fs::remove_file(index_path);

    result
}

fn ensure_origin_remote() -> Result<(), String> {
    git_output(["remote", "get-url", "origin"])
        .map(|_| ())
        .map_err(|_| "Original remote `origin` is required to commit a JIT session.".to_owned())
}

fn ensure_session_remote(session_repo: &str) -> Result<(), String> {
    let remote_url = format!("https://github.com/{session_repo}.git");

    match git_output(["remote", "get-url", SESSION_REMOTE]) {
        Ok(current_url) if current_url == remote_url => Ok(()),
        Ok(_) => git_status(["remote", "set-url", SESSION_REMOTE, &remote_url]),
        Err(_) => git_status(["remote", "add", SESSION_REMOTE, &remote_url]),
    }
}

fn delete_remote_tracking_branch(branch_name: &str) -> Result<(), String> {
    match git_status(["branch", "-dr", &format!("{SESSION_REMOTE}/{branch_name}")]) {
        Ok(()) => Ok(()),
        Err(_) => Ok(()),
    }
}

fn gh_status<I, S>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let status = Command::new("gh")
        .args(args)
        .status()
        .map_err(|error| format!("Failed to run gh: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("GitHub CLI command failed with status {status}."))
    }
}

fn gh_success<I, S>(args: I) -> Result<bool, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("gh")
        .args(args)
        .status()
        .map(|status| status.success())
        .map_err(|error| format!("Failed to run gh: {error}"))
}

fn gh_output<I, S>(args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(|error| format!("Failed to run gh: {error}"))?;

    if !output.status.success() {
        return Err(command_error_message(
            "GitHub CLI command failed.",
            output.stderr.as_slice(),
        ));
    }

    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_owned())
        .map_err(|error| format!("GitHub CLI returned non-UTF-8 output: {error}"))
}

fn git_output<I, S>(args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("Failed to run git: {error}"))?;

    if !output.status.success() {
        return Err(command_error_message(
            "Git command failed.",
            output.stderr.as_slice(),
        ));
    }

    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_owned())
        .map_err(|error| format!("Git returned non-UTF-8 output: {error}"))
}

fn git_output_with_index<I, S>(args: I, index_path: &str) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .env("GIT_INDEX_FILE", index_path)
        .args(args)
        .output()
        .map_err(|error| format!("Failed to run git: {error}"))?;

    if !output.status.success() {
        return Err(command_error_message(
            "Git command failed.",
            output.stderr.as_slice(),
        ));
    }

    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_owned())
        .map_err(|error| format!("Git returned non-UTF-8 output: {error}"))
}

fn git_status<I, S>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let status = Command::new("git")
        .args(args)
        .status()
        .map_err(|error| format!("Failed to run git: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("Git command failed with status {status}."))
    }
}

fn git_success<I, S>(args: I) -> Result<bool, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .args(args)
        .status()
        .map(|status| status.success())
        .map_err(|error| format!("Failed to run git: {error}"))
}

fn git_status_with_index<I, S>(args: I, index_path: &str) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let status = Command::new("git")
        .env("GIT_INDEX_FILE", index_path)
        .args(args)
        .status()
        .map_err(|error| format!("Failed to run git: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("Git command failed with status {status}."))
    }
}

fn command_error_message(default_message: &str, stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();

    if message.is_empty() {
        default_message.to_owned()
    } else {
        message
    }
}

fn normalize_git_username(username: &str) -> Result<String, String> {
    normalize_ref_part(username, "Git user.name").map_err(|_| {
        "Git user.name is empty; set it with `git config user.name <name>`.".to_owned()
    })
}

fn normalize_ref_part(value: &str, label: &str) -> Result<String, String> {
    let normalized = value
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches(&['.', '_', '-'])
        .to_owned();

    if normalized.is_empty() {
        Err(format!("{label} cannot be empty."))
    } else {
        Ok(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_git_username, normalize_ref_part, session_from_remote_tracking_ref};

    #[test]
    fn normalizes_git_usernames_for_ref_names() {
        assert_eq!(normalize_git_username("veryjos").unwrap(), "veryjos");
        assert_eq!(
            normalize_git_username("Joe's Intermediate Tracker").unwrap(),
            "Joe_s_Intermediate_Tracker"
        );
        assert_eq!(
            normalize_git_username("  joe.smith  ").unwrap(),
            "joe.smith"
        );
    }

    #[test]
    fn rejects_empty_git_usernames() {
        assert!(normalize_git_username("   ").is_err());
    }

    #[test]
    fn normalizes_session_names_for_ref_names() {
        assert_eq!(
            normalize_ref_part("session-1", "session name").unwrap(),
            "session-1"
        );
        assert_eq!(
            normalize_ref_part("  daily sync/session  ", "session name").unwrap(),
            "daily_sync_session"
        );
    }

    #[test]
    fn rejects_empty_session_names() {
        assert!(normalize_ref_part(" ... ", "session name").is_err());
    }

    #[test]
    fn formats_remote_tracking_session_names_for_current_user() {
        let session =
            session_from_remote_tracking_ref("jit/jit__veryjos_feature_test1\t123", "veryjos")
                .unwrap();

        assert_eq!(session.name, "feature_test1");
        assert_eq!(session.modified_at, 123);
        assert_eq!(
            session_from_remote_tracking_ref("jit/jit__other_user_feature_test1\t123", "veryjos")
                .map(|session| session.name),
            None
        );
    }
}
