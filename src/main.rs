use clap::{Parser, Subcommand};
use std::ffi::OsStr;
use std::process::{Command, ExitCode};

const APP_NAME: &str = "Joe's Intermediate Tracker";

#[derive(Debug, Parser)]
#[command(name = "jit", version, about = APP_NAME)]
struct Cli {
    #[command(subcommand)]
    command: CommandArgs,
}

#[derive(Debug, Subcommand)]
enum CommandArgs {
    /// Create the local and remote JIT ref.
    Init,
    /// Sync tracker data.
    Sync,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        CommandArgs::Init => init(),
        CommandArgs::Sync => sync(),
    }
}

fn init() -> ExitCode {
    match init_ref() {
        Ok(InitRefResult::Created(ref_name)) => {
            println!("Initialized JIT ref `{ref_name}` locally and on `origin`.");
            ExitCode::SUCCESS
        }
        Ok(InitRefResult::AlreadyExists(ref_name)) => {
            println!("JIT was already initialized at `{ref_name}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn sync() -> ExitCode {
    println!("Syncing {APP_NAME}...");
    println!("Nothing to sync yet.");
    ExitCode::SUCCESS
}

enum InitRefResult {
    Created(String),
    AlreadyExists(String),
}

fn init_ref() -> Result<InitRefResult, String> {
    ensure_origin_remote()?;

    let git_user_name = git_output(["config", "user.name"])?;
    let username = normalize_git_username(&git_user_name)?;
    let ref_name = format!("refs/jit/{username}");

    if local_ref_exists(&ref_name)? {
        return Ok(InitRefResult::AlreadyExists(ref_name));
    }

    git_status(["update-ref", &ref_name, "HEAD"])?;
    git_status(["push", "origin", &format!("{ref_name}:{ref_name}")])?;

    Ok(InitRefResult::Created(ref_name))
}

fn ensure_origin_remote() -> Result<(), String> {
    git_output(["remote", "get-url", "origin"])
        .map(|_| ())
        .map_err(|_| {
            "Remote `origin` is not configured; run `git remote add origin <url>` before `jit init`."
                .to_owned()
        })
}

fn local_ref_exists(ref_name: &str) -> Result<bool, String> {
    git_success(["show-ref", "--verify", "--quiet", ref_name])
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
        return Err(git_error_message(output.stderr.as_slice()));
    }

    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_owned())
        .map_err(|error| format!("Git returned non-UTF-8 output: {error}"))
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

fn git_error_message(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();

    if message.is_empty() {
        "Git command failed.".to_owned()
    } else {
        message
    }
}

fn normalize_git_username(username: &str) -> Result<String, String> {
    let normalized = username
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
        Err("Git user.name is empty; set it with `git config user.name <name>`.".to_owned())
    } else {
        Ok(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_git_username;

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
}
