use clap::{Parser, Subcommand};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::process::{Command, ExitCode};

const APP_NAME: &str = "Joe's Intermediate Tracker";
const JIT_REMOTE: &str = "jit";

#[derive(Debug, Parser)]
#[command(name = "jit", version, about = APP_NAME)]
struct Cli {
    #[command(subcommand)]
    command: CommandArgs,
}

#[derive(Debug, Subcommand)]
enum CommandArgs {
    /// Initialize a private JIT repository for this repo.
    Init {
        /// Name for this JIT session.
        session_name: String,
    },
    /// Print the JIT version.
    Version,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        CommandArgs::Init { session_name } => init(&session_name),
        CommandArgs::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}

fn init(session_name: &str) -> ExitCode {
    match init_jit_repository(session_name) {
        Ok(repository) => {
            println!("Initialized private JIT repository `{repository}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn init_jit_repository(session_name: &str) -> Result<String, String> {
    ensure_gh_installed()?;

    let source_repo_name = repository_name()?;
    let repository = jit_repository_name(&source_repo_name)?;
    ensure_private_repository(&repository)?;
    ensure_jit_remote(&repository)?;

    if git_output(["branch", "--show-current"], None)?.is_empty() {
        return Err("Cannot initialize JIT from a detached HEAD.".to_owned());
    }

    let commit = create_jit_meta_commit(session_name)?;
    let jit_branch = jit_branch_name(session_name);
    git_status([
        "push",
        JIT_REMOTE,
        &format!("{commit}:refs/heads/{jit_branch}"),
    ])?;

    Ok(repository)
}

fn jit_branch_name(session_name: &str) -> String {
    format!("jit-{}", escape_ref_part(session_name))
}

fn escape_ref_part(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(&['.', '-', '/'])
        .to_owned()
}

fn repository_name() -> Result<String, String> {
    gh_output(["repo", "view", "--json", "name", "--jq", ".name"])
}

fn jit_repository_name(source_repo_name: &str) -> Result<String, String> {
    let owner = gh_output(["api", "user", "--jq", ".login"])?;

    Ok(format!("{owner}/jit-{source_repo_name}"))
}

fn ensure_private_repository(repository: &str) -> Result<(), String> {
    if gh_success(["repo", "view", repository])? {
        return Ok(());
    }

    gh_status(["repo", "create", repository, "--private"])
}

fn ensure_jit_remote(repository: &str) -> Result<(), String> {
    let remote_url = format!("https://github.com/{repository}.git");

    match git_output(["remote", "get-url", JIT_REMOTE], None) {
        Ok(current_url) if current_url == remote_url => Ok(()),
        Ok(_) => git_status(["remote", "set-url", JIT_REMOTE, &remote_url]),
        Err(_) => git_status(["remote", "add", JIT_REMOTE, &remote_url]),
    }
}

fn create_jit_meta_commit(session_name: &str) -> Result<String, String> {
    let head = git_output(["rev-parse", "HEAD"], None)?;
    let index_path = env::temp_dir().join(format!("jit-init-index-{}", std::process::id()));
    let index_path_string = index_path.to_string_lossy().to_string();

    let result = (|| {
        git_status_with_index(["read-tree", "HEAD"], &index_path_string)?;
        let meta_blob = git_output(["hash-object", "-w", "--stdin"], Some("initialized=true\n"))?;
        git_status_with_index(
            [
                "update-index",
                "--add",
                "--cacheinfo",
                "100644",
                &meta_blob,
                ".jit-meta",
            ],
            &index_path_string,
        )?;
        let tree = git_output_with_index(["write-tree"], &index_path_string)?;

        git_output(
            [
                "commit-tree",
                &tree,
                "-p",
                &head,
                "-m",
                &format!("WIP: {session_name}"),
            ],
            None,
        )
    })();

    let _ = fs::remove_file(index_path);

    result
}

fn ensure_gh_installed() -> Result<(), String> {
    Command::new("gh")
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|_| "GitHub CLI (`gh`) is required for `jit init`.".to_owned())
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

    command_output("GitHub CLI command failed.", output)
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

fn git_output<I, S>(args: I, stdin: Option<&str>) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.args(args);

    let output = if let Some(stdin) = stdin {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;

                child
                    .stdin
                    .as_mut()
                    .expect("stdin is piped")
                    .write_all(stdin.as_bytes())?;
                child.wait_with_output()
            })
    } else {
        command.output()
    }
    .map_err(|error| format!("Failed to run git: {error}"))?;

    command_output("Git command failed.", output)
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

    command_output("Git command failed.", output)
}

fn command_output(default_message: &str, output: std::process::Output) -> Result<String, String> {
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if message.is_empty() {
            default_message.to_owned()
        } else {
            message
        });
    }

    String::from_utf8(output.stdout)
        .map(|stdout| stdout.trim().to_owned())
        .map_err(|error| format!("Command returned non-UTF-8 output: {error}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_jit_branch_name() {
        assert_eq!(super::jit_branch_name("feature1"), "jit-feature1");
        assert_eq!(
            super::jit_branch_name("test session 2"),
            "jit-test-session-2"
        );
    }
}
