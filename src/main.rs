use clap::{Parser, Subcommand};
use std::ffi::OsStr;
use std::io;
use std::process::{Command, ExitCode};

const APP_NAME: &str = "Joe's Intermediate Tracker";
const SESSION_REMOTE: &str = "jit";

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
    /// Sync tracker data.
    Session,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        CommandArgs::SessionInit {
            session_name,
            quiet,
        } => session_init(&session_name, quiet),
        CommandArgs::Status => status(),
        CommandArgs::Session => session(),
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

fn session() -> ExitCode {
    println!("Syncing {APP_NAME}...");
    println!("Nothing to sync yet.");
    ExitCode::SUCCESS
}

fn status() -> ExitCode {
    match list_sessions() {
        Ok(sessions) => {
            if sessions.is_empty() {
                println!("No JIT sessions found.");
            } else {
                for session in sessions {
                    println!("{session}");
                }
            }

            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn create_session_repo(session_name: &str, quiet: bool) -> Result<String, String> {
    ensure_gh_installed()?;
    let session_name = normalize_ref_part(session_name, "session name")?;

    let session_repo = session_repository()?;

    confirm_private_session_repo(&session_repo, quiet)?;

    ensure_private_session_repo(&session_repo)?;
    push_session_ref(&session_repo, &session_name)?;

    Ok(session_repo)
}

fn session_repository() -> Result<String, String> {
    let repository_name = repository_name()?;
    let session_repo_name = format!("jit-{repository_name}");
    let owner = gh_output(["api", "user", "--jq", ".login"])?;

    Ok(format!("{owner}/{session_repo_name}"))
}

fn ensure_private_session_repo(session_repo: &str) -> Result<(), String> {
    if gh_success(["repo", "view", session_repo])? {
        return Ok(());
    }

    gh_status(["repo", "create", session_repo, "--private"])
}

fn push_session_ref(session_repo: &str, session_name: &str) -> Result<(), String> {
    ensure_session_remote(session_repo)?;

    let git_user_name = git_output(["config", "user.name"])?;
    let username = normalize_git_username(&git_user_name)?;
    let session_branch = format!("jit__{username}_{session_name}");
    let diff_base = git_output(["rev-parse", "HEAD"])?;

    git_status([
        "push",
        SESSION_REMOTE,
        &format!("{diff_base}:refs/heads/{session_branch}"),
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

fn list_sessions() -> Result<Vec<String>, String> {
    ensure_gh_installed()?;
    let session_repo = session_repository()?;
    ensure_session_remote(&session_repo)?;

    let git_user_name = git_output(["config", "user.name"])?;
    let username = normalize_git_username(&git_user_name)?;
    let refs = git_output(["ls-remote", "--heads", SESSION_REMOTE, "jit__*"])?;

    Ok(refs
        .lines()
        .filter_map(|line| session_name_from_ls_remote_line(line, &username))
        .collect())
}

fn session_name_from_ls_remote_line(line: &str, username: &str) -> Option<String> {
    let (_, ref_name) = line.split_once('\t')?;
    ref_name
        .strip_prefix("refs/heads/")
        .and_then(|branch| branch.strip_prefix(&format!("jit__{username}_")))
        .map(|session_name| session_name.to_owned())
}

fn ensure_session_remote(session_repo: &str) -> Result<(), String> {
    let remote_url = format!("https://github.com/{session_repo}.git");

    match git_output(["remote", "get-url", SESSION_REMOTE]) {
        Ok(current_url) if current_url == remote_url => Ok(()),
        Ok(_) => git_status(["remote", "set-url", SESSION_REMOTE, &remote_url]),
        Err(_) => git_status(["remote", "add", SESSION_REMOTE, &remote_url]),
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
    use super::{normalize_git_username, normalize_ref_part, session_name_from_ls_remote_line};

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
    fn formats_remote_session_names_for_current_user() {
        assert_eq!(
            session_name_from_ls_remote_line(
                "abc123\trefs/heads/jit__veryjos_feature_test1",
                "veryjos"
            )
            .unwrap(),
            "feature_test1"
        );
        assert_eq!(
            session_name_from_ls_remote_line(
                "abc123\trefs/heads/jit__other_user_feature_test1",
                "veryjos"
            ),
            None
        );
    }
}
