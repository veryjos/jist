use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "Joe's Intermediate Tracker";
const JIT_REMOTE: &str = "jit";
const STATUS_SESSION_LIMIT: usize = 3;

#[derive(Debug, Parser)]
#[command(name = "jit", version, about = APP_NAME)]
struct Cli {
    #[command(subcommand)]
    command: CommandArgs,
}

#[derive(Debug, Subcommand)]
enum CommandArgs {
    /// Claim the current JIT session for this machine.
    Claim,
    /// Push the current JIT session changes if this machine owns the claim.
    Push,
    /// Create or enter a JIT session.
    Session {
        /// Name for this JIT session.
        session_name: String,
    },
    /// List JIT sessions on the private remote.
    Status,
    /// Remove this machine's claim from the current JIT session.
    Unclaim,
    /// Print the JIT version.
    Version,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        CommandArgs::Claim => claim(),
        CommandArgs::Push => push(),
        CommandArgs::Session { session_name } => session(&session_name),
        CommandArgs::Status => status(),
        CommandArgs::Unclaim => unclaim(),
        CommandArgs::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}

fn unclaim() -> ExitCode {
    match unclaim_session() {
        Ok(session) => {
            println!("Unclaimed JIT session `{session}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn push() -> ExitCode {
    match push_session() {
        Ok(session) => {
            println!("Pushed JIT session `{session}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn claim() -> ExitCode {
    match claim_session() {
        Ok(session) => {
            println!("Claimed JIT session `{session}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn status() -> ExitCode {
    match list_sessions() {
        Ok(sessions) => {
            let current_session = match selected_session() {
                Ok(session) => session,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
            };

            match &current_session {
                Some(session) => {
                    println!("Current session: \x1b[32m*{session}\x1b[0m");

                    match session_links(session) {
                        Ok(links) => {
                            println!("Working commit: {}", links.session_commit_url);
                            println!("Diffbase: {}", links.diffbase_url);
                            println!("Sync status: {}", format_sync_status(&links.sync_status));
                            println!("Claim: {}", format_claim_state(&links.claim_state));
                        }
                        Err(error) => {
                            eprintln!("{error}");
                            return ExitCode::FAILURE;
                        }
                    }
                }
                None => println!("Current session: <none>"),
            }

            println!();
            println!("Sessions:");
            if sessions.is_empty() {
                println!("<no sessions>");
            } else {
                for session in visible_sessions(&sessions, current_session.as_deref()) {
                    if current_session.as_deref() == Some(session.name.as_str()) {
                        println!("- \x1b[32m*{}\x1b[0m", session.name);
                    } else {
                        println!("-  {}", session.name);
                    }
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

fn session(session_name: &str) -> ExitCode {
    match start_jit_session(session_name) {
        Ok(commit) => {
            println!("Checked out JIT session `{session_name}` at `{commit}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn start_jit_session(session_name: &str) -> Result<String, String> {
    ensure_gh_installed()?;

    let source_repo_name = repository_name()?;
    let repository = jit_repository_name(&source_repo_name)?;
    ensure_private_repository(&repository)?;
    ensure_jit_remote(&repository)?;

    if git_output(["branch", "--show-current"], None)?.is_empty() {
        return Err("Cannot initialize JIT from a detached HEAD.".to_owned());
    }

    let jit_branch = jit_branch_name(session_name);
    let commit = match remote_branch_commit(&jit_branch)? {
        Some(commit) => commit,
        None => {
            let commit = create_jit_meta_commit(session_name)?;
            git_status([
                "push",
                JIT_REMOTE,
                &format!("{commit}:refs/heads/{jit_branch}"),
            ])?;
            commit
        }
    };

    checkout_jit_branch(&jit_branch)?;

    Ok(commit)
}

fn checkout_jit_branch(jit_branch: &str) -> Result<(), String> {
    fetch_jit_branch(jit_branch)?;

    if local_branch_exists(jit_branch)? {
        git_status(["checkout", jit_branch])
    } else {
        git_status([
            "checkout",
            "--track",
            "-b",
            jit_branch,
            &format!("{JIT_REMOTE}/{jit_branch}"),
        ])
    }
}

fn claim_session() -> Result<String, String> {
    ensure_gh_installed()?;

    let current_branch = git_output(["branch", "--show-current"], None)?;
    let session_name = session_name_from_jit_branch(&current_branch)
        .ok_or_else(|| "Check out a JIT session branch before running `jit claim`.".to_owned())?
        .to_owned();

    let source_repo_name = repository_name()?;
    let repository = jit_repository_name(&source_repo_name)?;
    ensure_jit_remote(&repository)?;
    fetch_jit_branch(&current_branch)?;
    ensure_unclaimed(&current_branch)?;

    fs::write(".jit-meta", claim_metadata()?)
        .map_err(|error| format!("Failed to write .jit-meta: {error}"))?;
    git_status(["add", ".jit-meta"])?;
    git_status(["commit", "--amend", "--no-edit"])?;
    git_status([
        "push",
        "--force",
        JIT_REMOTE,
        &format!("HEAD:refs/heads/{current_branch}"),
    ])?;

    Ok(session_name)
}

fn push_session() -> Result<String, String> {
    ensure_gh_installed()?;

    let current_branch = git_output(["branch", "--show-current"], None)?;
    let session_name = session_name_from_jit_branch(&current_branch)
        .ok_or_else(|| "Check out a JIT session branch before running `jit push`.".to_owned())?
        .to_owned();

    let source_repo_name = repository_name()?;
    let repository = jit_repository_name(&source_repo_name)?;
    ensure_jit_remote(&repository)?;
    fetch_jit_branch(&current_branch)?;
    ensure_claimed_by_this_machine(&current_branch)?;

    git_status(["add", "-A"])?;
    if git_success(["diff", "--cached", "--quiet"])? {
        git_status([
            "push",
            "--force",
            JIT_REMOTE,
            &format!("HEAD:refs/heads/{current_branch}"),
        ])?;
    } else {
        git_status(["commit", "--amend", "--no-edit"])?;
        git_status([
            "push",
            "--force",
            JIT_REMOTE,
            &format!("HEAD:refs/heads/{current_branch}"),
        ])?;
    }

    Ok(session_name)
}

fn unclaim_session() -> Result<String, String> {
    ensure_gh_installed()?;

    let current_branch = git_output(["branch", "--show-current"], None)?;
    let session_name = session_name_from_jit_branch(&current_branch)
        .ok_or_else(|| "Check out a JIT session branch before running `jit unclaim`.".to_owned())?
        .to_owned();

    let source_repo_name = repository_name()?;
    let repository = jit_repository_name(&source_repo_name)?;
    ensure_jit_remote(&repository)?;
    fetch_jit_branch(&current_branch)?;
    ensure_claimed_by_this_machine(&current_branch)?;

    fs::write(".jit-meta", unclaimed_metadata())
        .map_err(|error| format!("Failed to write .jit-meta: {error}"))?;
    git_status(["add", ".jit-meta"])?;
    git_status(["commit", "--amend", "--no-edit"])?;
    git_status([
        "push",
        "--force",
        JIT_REMOTE,
        &format!("HEAD:refs/heads/{current_branch}"),
    ])?;

    Ok(session_name)
}

fn ensure_claimed_by_this_machine(jit_branch: &str) -> Result<(), String> {
    match remote_claim_owner(jit_branch)? {
        Some(claim) if claim.is_current_setup()? => Ok(()),
        Some(claim) => Err(claim.wrong_owner_message()),
        None => Err("This JIT session has not been claimed yet.".to_owned()),
    }
}

fn ensure_unclaimed(jit_branch: &str) -> Result<(), String> {
    match remote_claim_owner(jit_branch)? {
        Some(claim) if claim.is_current_setup()? => {
            Err("This JIT session is already claimed by this machine and user.".to_owned())
        }
        Some(claim) => Err(claim.wrong_owner_message()),
        None => Ok(()),
    }
}

fn remote_claim_owner(jit_branch: &str) -> Result<Option<ClaimOwner>, String> {
    let metadata = git_output(
        [
            "show",
            &format!("refs/remotes/{JIT_REMOTE}/{jit_branch}:.jit-meta"),
        ],
        None,
    )?;

    match (
        metadata_value(&metadata, "machine_id"),
        metadata_value(&metadata, "local_user"),
    ) {
        (Some(machine_id), Some(local_user)) => Ok(Some(ClaimOwner {
            machine_id: machine_id.to_owned(),
            local_user: local_user.to_owned(),
        })),
        _ => Ok(None),
    }
}

struct ClaimOwner {
    machine_id: String,
    local_user: String,
}

impl ClaimOwner {
    fn is_current_setup(&self) -> Result<bool, String> {
        Ok(self.machine_id == machine_id()? && self.local_user == local_machine_user()?)
    }

    fn wrong_owner_message(&self) -> String {
        match (local_machine_user(), machine_id()) {
            (Ok(current_local_user), Ok(current_machine_id)) => format!(
                "This JIT session is claimed by `{}` on `{}`, not `{current_local_user}` on `{current_machine_id}`.",
                self.local_user, self.machine_id
            ),
            _ => format!(
                "This JIT session is claimed by `{}` on `{}`.",
                self.local_user, self.machine_id
            ),
        }
    }
}

fn metadata_value<'a>(metadata: &'a str, key: &str) -> Option<&'a str> {
    metadata
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(metadata_key, value)| (metadata_key == key).then_some(value))
}

fn claim_metadata() -> Result<String, String> {
    Ok(format!(
        "machine_id={}\nlocal_user={}\ngit_user_name={}\ngit_user_email={}\nclaimed_at_utc_unix_seconds={}\n",
        machine_id()?,
        local_machine_user()?,
        git_output(["config", "user.name"], None)?,
        git_output(["config", "user.email"], None)?,
        utc_unix_timestamp()?
    ))
}

fn unclaimed_metadata() -> &'static str {
    "initialized=true\n"
}

fn machine_id() -> Result<String, String> {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .or_else(|_| command_stdout("hostname"))
        .map(|value| value.trim().to_owned())
        .map_err(|_| "Could not determine machine id.".to_owned())
}

fn local_machine_user() -> Result<String, String> {
    env::var("USERNAME")
        .or_else(|_| env::var("USER"))
        .map_err(|_| "Could not determine local machine user.".to_owned())
}

fn utc_unix_timestamp() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| format!("System clock is before Unix epoch: {error}"))
}

fn command_stdout(program: &str) -> Result<String, env::VarError> {
    Command::new(program)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .ok_or(env::VarError::NotPresent)
}

struct SessionInfo {
    name: String,
}

struct SessionLinks {
    session_commit_url: String,
    diffbase_url: String,
    sync_status: SyncStatus,
    claim_state: ClaimState,
}

struct SyncStatus {
    unsynced_files: usize,
    total_files: usize,
}

enum ClaimState {
    Unclaimed,
    ClaimedByCurrentSetup,
    ClaimedByOther {
        machine_id: String,
        local_user: String,
    },
}

fn session_links(session_name: &str) -> Result<SessionLinks, String> {
    let source_repo_name = repository_name()?;
    let jit_repository = jit_repository_name(&source_repo_name)?;
    let original_repository = original_repository_name()?;
    let jit_branch = jit_branch_name(session_name);

    fetch_jit_branch(&jit_branch)?;

    let diffbase = git_output(
        [
            "rev-parse",
            &format!("refs/remotes/{JIT_REMOTE}/{jit_branch}^"),
        ],
        None,
    )?;

    Ok(SessionLinks {
        session_commit_url: github_branch_commit_url(&jit_repository, &jit_branch),
        diffbase_url: github_short_commit_url(&original_repository, &diffbase)?,
        sync_status: sync_status_for_branch(&jit_branch)?,
        claim_state: claim_state_for_branch(&jit_branch)?,
    })
}

fn claim_state_for_branch(jit_branch: &str) -> Result<ClaimState, String> {
    match remote_claim_owner(jit_branch)? {
        Some(claim) if claim.is_current_setup()? => Ok(ClaimState::ClaimedByCurrentSetup),
        Some(claim) => Ok(ClaimState::ClaimedByOther {
            machine_id: claim.machine_id,
            local_user: claim.local_user,
        }),
        None => Ok(ClaimState::Unclaimed),
    }
}

fn sync_status_for_branch(jit_branch: &str) -> Result<SyncStatus, String> {
    let remote_ref = format!("refs/remotes/{JIT_REMOTE}/{jit_branch}");
    let changed_files = git_output(["diff", "--name-only", &remote_ref, "--"], None)?;
    let untracked_files = git_output(["ls-files", "--others", "--exclude-standard"], None)?;
    let remote_files = git_output(["ls-tree", "-r", "--name-only", &remote_ref], None)?;

    let mut unsynced_files = BTreeSet::new();
    unsynced_files.extend(non_empty_lines(&changed_files).map(str::to_owned));
    unsynced_files.extend(non_empty_lines(&untracked_files).map(str::to_owned));

    let mut total_files = BTreeSet::new();
    total_files.extend(non_empty_lines(&remote_files).map(str::to_owned));
    total_files.extend(unsynced_files.iter().cloned());

    Ok(SyncStatus {
        unsynced_files: unsynced_files.len(),
        total_files: total_files.len(),
    })
}

fn non_empty_lines(value: &str) -> impl Iterator<Item = &str> {
    value.lines().filter(|line| !line.trim().is_empty())
}

fn format_sync_status(sync_status: &SyncStatus) -> String {
    if sync_status.unsynced_files == 0 {
        format!(
            "\x1b[32mFully synced ({} {})\x1b[0m",
            sync_status.total_files,
            pluralize("file", sync_status.total_files)
        )
    } else {
        format!(
            "\x1b[31m{} {} unsynced (out of {} {})\x1b[0m",
            sync_status.unsynced_files,
            pluralize("file", sync_status.unsynced_files),
            sync_status.total_files,
            pluralize("file", sync_status.total_files)
        )
    }
}

fn format_claim_state(claim_state: &ClaimState) -> String {
    match claim_state {
        ClaimState::Unclaimed => "\x1b[31mUnclaimed\x1b[0m".to_owned(),
        ClaimState::ClaimedByCurrentSetup => {
            "\x1b[32mClaimed by this machine and user\x1b[0m".to_owned()
        }
        ClaimState::ClaimedByOther {
            machine_id,
            local_user,
        } => format!("\x1b[31mClaimed by `{local_user}` on `{machine_id}`\x1b[0m"),
    }
}

fn pluralize(word: &str, count: usize) -> &str {
    if count == 1 { word } else { "files" }
}

fn visible_sessions<'a>(
    sessions: &'a [SessionInfo],
    current_session: Option<&str>,
) -> Vec<&'a SessionInfo> {
    let mut visible = Vec::with_capacity(STATUS_SESSION_LIMIT.min(sessions.len()));

    if let Some(current_session) = current_session {
        if let Some(session) = sessions
            .iter()
            .find(|session| session.name == current_session)
        {
            visible.push(session);
        }
    }

    visible.extend(
        sessions
            .iter()
            .filter(|session| Some(session.name.as_str()) != current_session)
            .take(STATUS_SESSION_LIMIT.saturating_sub(visible.len())),
    );

    visible
}

fn list_sessions() -> Result<Vec<SessionInfo>, String> {
    ensure_gh_installed()?;

    let source_repo_name = repository_name()?;
    let repository = jit_repository_name(&source_repo_name)?;
    ensure_jit_remote(&repository)?;
    fetch_jit_sessions()?;

    let refs = git_output(
        [
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)%09%(committerdate:unix)",
            &format!("refs/remotes/{JIT_REMOTE}/jit-*"),
        ],
        None,
    )?;

    Ok(refs.lines().filter_map(session_from_ref_line).collect())
}

fn selected_session() -> Result<Option<String>, String> {
    let current_branch = git_output(["branch", "--show-current"], None)?;

    Ok(session_name_from_jit_branch(&current_branch).map(str::to_owned))
}

fn fetch_jit_sessions() -> Result<(), String> {
    git_status([
        "fetch",
        "--prune",
        JIT_REMOTE,
        &format!("+refs/heads/jit-*:refs/remotes/{JIT_REMOTE}/jit-*"),
    ])
}

fn fetch_jit_branch(jit_branch: &str) -> Result<(), String> {
    git_status([
        "fetch",
        JIT_REMOTE,
        &format!("+refs/heads/{jit_branch}:refs/remotes/{JIT_REMOTE}/{jit_branch}"),
    ])
}

fn local_branch_exists(branch_name: &str) -> Result<bool, String> {
    git_success([
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch_name}"),
    ])
}

fn remote_branch_commit(branch_name: &str) -> Result<Option<String>, String> {
    let output = git_output(["ls-remote", "--heads", JIT_REMOTE, branch_name], None)?;
    parse_ls_remote_commit(branch_name, &output)
}

fn parse_ls_remote_commit(branch_name: &str, output: &str) -> Result<Option<String>, String> {
    if output.is_empty() {
        return Ok(None);
    }

    output
        .lines()
        .next()
        .and_then(|line| line.split_once('\t'))
        .map(|(commit, _)| Some(commit.to_owned()))
        .ok_or_else(|| format!("Could not parse remote branch `{branch_name}`."))
}

fn session_from_ref_line(line: &str) -> Option<SessionInfo> {
    let (ref_name, modified_at) = line.split_once('\t')?;
    let name = ref_name.strip_prefix(&format!("{JIT_REMOTE}/jit-"))?;
    let _modified_at: i64 = modified_at.parse().ok()?;

    Some(SessionInfo {
        name: name.to_owned(),
    })
}

fn session_name_from_jit_branch(branch_name: &str) -> Option<&str> {
    branch_name
        .strip_prefix("jit-")
        .filter(|session| !session.is_empty())
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

fn original_repository_name() -> Result<String, String> {
    gh_output([
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ])
}

fn jit_repository_name(source_repo_name: &str) -> Result<String, String> {
    let owner = gh_output(["api", "user", "--jq", ".login"])?;

    Ok(format!("{owner}/jit-{source_repo_name}"))
}

fn github_commit_url(repository: &str, commit: &str) -> String {
    format!("https://github.com/{repository}/commit/{commit}")
}

fn github_branch_commit_url(repository: &str, branch_name: &str) -> String {
    github_commit_url(repository, branch_name)
}

fn github_short_commit_url(repository: &str, commit: &str) -> Result<String, String> {
    let short_commit = git_output(["rev-parse", "--short", commit], None)?;

    Ok(github_commit_url(repository, &short_commit))
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
        .map_err(|_| "GitHub CLI (`gh`) is required for `jit session`.".to_owned())
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

    #[test]
    fn parses_session_from_ref_line() {
        let session = super::session_from_ref_line("jit/jit-test-session-2\t123").unwrap();

        assert_eq!(session.name, "test-session-2");
        assert!(super::session_from_ref_line("origin/main\t123").is_none());
    }

    #[test]
    fn parses_remote_branch_commit() {
        assert_eq!(
            super::parse_ls_remote_commit("jit-test", "abc123\trefs/heads/jit-test\n").unwrap(),
            Some("abc123".to_owned())
        );
        assert_eq!(super::parse_ls_remote_commit("jit-test", "").unwrap(), None);
    }

    #[test]
    fn parses_selected_session_from_branch() {
        assert_eq!(
            super::session_name_from_jit_branch("jit-test-session-2"),
            Some("test-session-2")
        );
        assert_eq!(super::session_name_from_jit_branch("main"), None);
    }

    #[test]
    fn formats_github_commit_url() {
        assert_eq!(
            super::github_commit_url("veryjos/jit-test", "abc123"),
            "https://github.com/veryjos/jit-test/commit/abc123"
        );
        assert_eq!(
            super::github_branch_commit_url("veryjos/jit-jit-test", "jit-test"),
            "https://github.com/veryjos/jit-jit-test/commit/jit-test"
        );
    }

    #[test]
    fn formats_sync_status() {
        assert_eq!(
            super::format_sync_status(&super::SyncStatus {
                unsynced_files: 0,
                total_files: 3,
            }),
            "\x1b[32mFully synced (3 files)\x1b[0m"
        );
        assert_eq!(
            super::format_sync_status(&super::SyncStatus {
                unsynced_files: 1,
                total_files: 3,
            }),
            "\x1b[31m1 file unsynced (out of 3 files)\x1b[0m"
        );
    }

    #[test]
    fn formats_claim_state() {
        assert_eq!(
            super::format_claim_state(&super::ClaimState::Unclaimed),
            "\x1b[31mUnclaimed\x1b[0m"
        );
        assert_eq!(
            super::format_claim_state(&super::ClaimState::ClaimedByCurrentSetup),
            "\x1b[32mClaimed by this machine and user\x1b[0m"
        );
        assert_eq!(
            super::format_claim_state(&super::ClaimState::ClaimedByOther {
                machine_id: "desktop".to_owned(),
                local_user: "jos".to_owned(),
            }),
            "\x1b[31mClaimed by `jos` on `desktop`\x1b[0m"
        );
    }

    #[test]
    fn reads_metadata_values() {
        let metadata = "machine_id=desktop\nlocal_user=jos\nclaimed_at_utc_unix_seconds=42\n";

        assert_eq!(
            super::metadata_value(metadata, "machine_id"),
            Some("desktop")
        );
        assert_eq!(super::metadata_value(metadata, "local_user"), Some("jos"));
        assert_eq!(super::metadata_value(metadata, "missing"), None);
    }

    #[test]
    fn unclaimed_metadata_has_no_claim_owner() {
        let metadata = super::unclaimed_metadata();

        assert_eq!(super::metadata_value(metadata, "machine_id"), None);
        assert_eq!(super::metadata_value(metadata, "local_user"), None);
    }

    #[test]
    fn pins_current_session_in_visible_list() {
        let sessions = vec![
            super::SessionInfo {
                name: "newest".to_owned(),
            },
            super::SessionInfo {
                name: "current".to_owned(),
            },
            super::SessionInfo {
                name: "older".to_owned(),
            },
            super::SessionInfo {
                name: "oldest".to_owned(),
            },
        ];
        let visible = super::visible_sessions(&sessions, Some("current"));

        assert_eq!(visible[0].name, "current");
        assert_eq!(visible.len(), super::STATUS_SESSION_LIMIT);
    }
}
