use clap::{Parser, Subcommand};
use notify::{RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const APP_NAME: &str = "Joe's Intermediate Session Tracker";
const JIST_REMOTE: &str = "jist";
const STATUS_SESSION_LIMIT: usize = 3;
const WORK_PUSH_DEBOUNCE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Parser)]
#[command(name = "jist", version, about = APP_NAME)]
struct Cli {
    #[command(subcommand)]
    command: CommandArgs,
}

#[derive(Debug, Subcommand)]
enum CommandArgs {
    /// Claim the current JIST session for this machine.
    Claim,
    /// Push the current JIST session changes if this machine owns the claim.
    Push,
    /// Create or enter a JIST session.
    Session {
        /// Name for this JIST session.
        session_name: String,
    },
    /// List JIST sessions on the private remote.
    Status,
    /// Remove this machine's claim from the current JIST session.
    Unclaim,
    /// Print the JIST version.
    Version,
    /// Start a claimed JIST session and automatically push changes after a quiet period.
    Work {
        /// Name for this JIST session.
        session_name: String,
    },
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
        CommandArgs::Work { session_name } => work(&session_name),
    }
}

fn work(session_name: &str) -> ExitCode {
    match work_session(session_name) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn unclaim() -> ExitCode {
    match unclaim_session() {
        Ok(session) => {
            println!("Unclaimed session {session}.");
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
            println!("Pushed JIST session `{session}`.");
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
            println!("Claimed JIST session `{session}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn status() -> ExitCode {
    match render_status() {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn render_status() -> Result<String, String> {
    let sessions = list_sessions()?;
    let current_session = selected_session()?;
    let mut output = String::new();

    match &current_session {
        Some(session) => {
            let links = session_links(session)?;
            output.push_str(&format_session_state_message(
                session,
                &links.claim_state,
                links.sync_status.as_ref(),
                &links.sessionbase_url,
                &links.sessionbase_commit_url,
            ));
        }
        None => output.push_str("Session:\n  <none>\n"),
    }

    output.push('\n');
    output.push_str("Your sessions:\n");
    if sessions.is_empty() {
        output.push_str("<no sessions>\n");
    } else {
        for session in visible_sessions(&sessions, current_session.as_deref()) {
            if current_session.as_deref() == Some(session.name.as_str()) {
                output.push_str(&format!(
                    "- {}\n",
                    format_session_list_name(&session.name, true, session.claimed)
                ));
            } else {
                output.push_str(&format!(
                    "- {}\n",
                    format_session_list_name(&session.name, false, session.claimed)
                ));
            }
        }
    }

    Ok(output)
}

fn session(session_name: &str) -> ExitCode {
    match start_jist_session(session_name) {
        Ok(commit) => {
            println!("Checked out JIST session `{session_name}` at `{commit}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn work_session(session_name: &str) -> Result<(), String> {
    ensure_clean_worktree()?;
    start_jist_session(session_name)?;
    claim_session()?;

    let result = run_work_loop();
    let unclaim_result = unclaim_session();

    match (result, unclaim_result) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(error)) => Err(format!("Failed to unclaim JIST session: {error}")),
        (Err(error), Err(unclaim_error)) => Err(format!(
            "{error}\nFailed to unclaim JIST session: {unclaim_error}"
        )),
    }
}

fn ensure_clean_worktree() -> Result<(), String> {
    let status = git_output(["status", "--porcelain"], None)?;

    if status.is_empty() {
        Ok(())
    } else {
        Err("`jist work` requires a clean git working tree.".to_owned())
    }
}

fn run_work_loop() -> Result<(), String> {
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
    ctrlc::set_handler(move || {
        signal_running.store(false, Ordering::SeqCst);
    })
    .map_err(|error| format!("Failed to install Ctrl+C handler: {error}"))?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |result| {
        let _ = tx.send(result);
    })
    .map_err(|error| format!("Failed to create filesystem watcher: {error}"))?;
    watcher
        .watch(
            &env::current_dir().map_err(|error| format!("Failed to read cwd: {error}"))?,
            RecursiveMode::Recursive,
        )
        .map_err(|error| format!("Failed to watch working directory: {error}"))?;

    let mut push_deadline: Option<Instant> = None;
    let mut message = "Watching for changes. Press Ctrl+C to unclaim and exit.".to_owned();
    render_work_status(&message)?;

    while running.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => {
                if event.paths.iter().any(|path| !is_git_internal_path(path)) {
                    push_deadline = Some(Instant::now() + WORK_PUSH_DEBOUNCE);
                    message = format!(
                        "Change detected. Next push after {} minutes of quiet.",
                        WORK_PUSH_DEBOUNCE.as_secs() / 60
                    );
                    render_work_status(&message)?;
                }
            }
            Ok(Err(error)) => {
                message = format!("Filesystem watcher warning: {error}");
                render_work_status(&message)?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Filesystem watcher stopped unexpectedly.".to_owned());
            }
        }

        if push_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            message = "Pushing changes...".to_owned();
            render_work_status(&message)?;

            match push_session() {
                Ok(session) => {
                    push_deadline = None;
                    message = format!("Pushed `{session}`. Watching for changes.");
                }
                Err(error) => {
                    push_deadline = Some(Instant::now() + WORK_PUSH_DEBOUNCE);
                    message = format!("Push failed: {error}");
                }
            }

            render_work_status(&message)?;
        }
    }

    render_work_status("Stopping. Unclaiming session...")?;
    Ok(())
}

fn render_work_status(message: &str) -> Result<(), String> {
    print!("\x1b[2J\x1b[H");
    print!("{}", render_status()?);
    println!();
    println!("Work: {message}");

    Ok(())
}

fn is_git_internal_path(path: &std::path::Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == OsStr::new(".git"))
}

fn start_jist_session(session_name: &str) -> Result<String, String> {
    ensure_gh_installed()?;

    let source_repo_name = repository_name()?;
    let repository = jist_repository_name(&source_repo_name)?;
    ensure_private_repository(&repository)?;
    ensure_jist_remote(&repository)?;

    if git_output(["branch", "--show-current"], None)?.is_empty() {
        return Err("Cannot initialize JIST from a detached HEAD.".to_owned());
    }

    let jist_branch = jist_branch_name(session_name);
    let commit = match remote_branch_commit(&jist_branch)? {
        Some(commit) => commit,
        None => {
            let commit = create_jist_meta_commit(session_name)?;
            git_status([
                "push",
                JIST_REMOTE,
                &format!("{commit}:refs/heads/{jist_branch}"),
            ])?;
            commit
        }
    };

    checkout_jist_branch(&jist_branch)?;

    Ok(commit)
}

fn checkout_jist_branch(jist_branch: &str) -> Result<(), String> {
    fetch_jist_branch(jist_branch)?;

    if local_branch_exists(jist_branch)? {
        git_status(["checkout", jist_branch])
    } else {
        git_status([
            "checkout",
            "--track",
            "-b",
            jist_branch,
            &format!("{JIST_REMOTE}/{jist_branch}"),
        ])
    }
}

fn claim_session() -> Result<String, String> {
    ensure_gh_installed()?;

    let current_branch = git_output(["branch", "--show-current"], None)?;
    let session_name = session_name_from_jist_branch(&current_branch)
        .ok_or_else(|| "Check out a JIST session branch before running `jist claim`.".to_owned())?
        .to_owned();

    let source_repo_name = repository_name()?;
    let repository = jist_repository_name(&source_repo_name)?;
    ensure_jist_remote(&repository)?;
    fetch_jist_branch(&current_branch)?;
    ensure_unclaimed(&current_branch)?;

    fs::write(".jist-meta", claim_metadata()?)
        .map_err(|error| format!("Failed to write .jist-meta: {error}"))?;
    git_status(["add", ".jist-meta"])?;
    git_status(["commit", "--amend", "--no-edit"])?;
    git_status([
        "push",
        "--force",
        JIST_REMOTE,
        &format!("HEAD:refs/heads/{current_branch}"),
    ])?;

    Ok(session_name)
}

fn push_session() -> Result<String, String> {
    ensure_gh_installed()?;

    let current_branch = git_output(["branch", "--show-current"], None)?;
    let session_name = session_name_from_jist_branch(&current_branch)
        .ok_or_else(|| "Check out a JIST session branch before running `jist push`.".to_owned())?
        .to_owned();

    let source_repo_name = repository_name()?;
    let repository = jist_repository_name(&source_repo_name)?;
    ensure_jist_remote(&repository)?;
    fetch_jist_branch(&current_branch)?;
    ensure_claimed_by_this_machine(&current_branch)?;

    git_status(["add", "-A"])?;
    if git_success(["diff", "--cached", "--quiet"])? {
        git_status([
            "push",
            "--force",
            JIST_REMOTE,
            &format!("HEAD:refs/heads/{current_branch}"),
        ])?;
    } else {
        git_status(["commit", "--amend", "--no-edit"])?;
        git_status([
            "push",
            "--force",
            JIST_REMOTE,
            &format!("HEAD:refs/heads/{current_branch}"),
        ])?;
    }

    Ok(session_name)
}

fn unclaim_session() -> Result<String, String> {
    ensure_gh_installed()?;

    let current_branch = git_output(["branch", "--show-current"], None)?;
    let session_name = session_name_from_jist_branch(&current_branch)
        .ok_or_else(|| "Check out a JIST session branch before running `jist unclaim`.".to_owned())?
        .to_owned();

    let source_repo_name = repository_name()?;
    let repository = jist_repository_name(&source_repo_name)?;
    ensure_jist_remote(&repository)?;
    fetch_jist_branch(&current_branch)?;
    ensure_claimed_by_this_machine(&current_branch)?;

    fs::write(".jist-meta", unclaimed_metadata())
        .map_err(|error| format!("Failed to write .jist-meta: {error}"))?;
    git_status(["add", ".jist-meta"])?;
    git_status(["commit", "--amend", "--no-edit"])?;
    git_status([
        "push",
        "--force",
        JIST_REMOTE,
        &format!("HEAD:refs/heads/{current_branch}"),
    ])?;

    Ok(session_name)
}

fn ensure_claimed_by_this_machine(jist_branch: &str) -> Result<(), String> {
    match remote_claim_owner(jist_branch)? {
        Some(claim) if claim.is_current_setup()? => Ok(()),
        Some(claim) => Err(claim.wrong_owner_message()),
        None => Err("This JIST session has not been claimed yet.".to_owned()),
    }
}

fn ensure_unclaimed(jist_branch: &str) -> Result<(), String> {
    match remote_claim_owner(jist_branch)? {
        Some(claim) if claim.is_current_setup()? => {
            Err("This JIST session is already claimed by this machine and user.".to_owned())
        }
        Some(claim) => Err(claim.wrong_owner_message()),
        None => Ok(()),
    }
}

fn remote_claim_owner(jist_branch: &str) -> Result<Option<ClaimeeId>, String> {
    let metadata = git_output(
        [
            "show",
            &format!("refs/remotes/{JIST_REMOTE}/{jist_branch}:.jist-meta"),
        ],
        None,
    )?;

    if let Some(claimee_id) = metadata_value(&metadata, "claimee_id") {
        return Ok(Some(ClaimeeId::parse(claimee_id)?));
    }

    match (
        metadata_value(&metadata, "machine_id"),
        metadata_value(&metadata, "local_user"),
    ) {
        (Some(machine_id), Some(local_user)) => Ok(Some(ClaimeeId {
            user: local_user.to_owned(),
            machine: machine_id.to_owned(),
        })),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaimeeId {
    user: String,
    machine: String,
}

impl ClaimeeId {
    fn current() -> Result<Self, String> {
        Ok(Self {
            user: local_machine_user()?,
            machine: machine_id()?,
        })
    }

    fn parse(value: &str) -> Result<Self, String> {
        let (user, machine) = value
            .split_once('@')
            .ok_or_else(|| format!("Invalid claimee id `{value}`."))?;

        if user.is_empty() || machine.is_empty() {
            return Err(format!("Invalid claimee id `{value}`."));
        }

        Ok(Self {
            user: user.to_owned(),
            machine: machine.to_owned(),
        })
    }

    fn is_current_setup(&self) -> Result<bool, String> {
        Ok(self == &Self::current()?)
    }

    fn wrong_owner_message(&self) -> String {
        match Self::current() {
            Ok(current) => {
                format!("This JIST session is claimed by `{self}`, not `{current}`.")
            }
            Err(_) => format!("This JIST session is claimed by `{self}`."),
        }
    }
}

impl fmt::Display for ClaimeeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.user, self.machine)
    }
}

fn metadata_value<'a>(metadata: &'a str, key: &str) -> Option<&'a str> {
    metadata
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(metadata_key, value)| (metadata_key == key).then_some(value))
}

fn claim_metadata() -> Result<String, String> {
    let claimee_id = ClaimeeId::current()?;

    Ok(format!(
        "claimee_id={claimee_id}\ngit_user_name={}\ngit_user_email={}\nclaimed_at_utc_unix_seconds={}\n",
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
    claimed: bool,
}

struct SessionLinks {
    sessionbase_url: String,
    sessionbase_commit_url: String,
    sync_status: Option<SyncStatus>,
    claim_state: ClaimState,
}

struct SyncStatus {
    unsynced_files: usize,
    total_files: usize,
}

enum ClaimState {
    Unclaimed,
    ClaimedByCurrentSetup,
    ClaimedByOther(ClaimeeId),
}

impl ClaimState {
    fn is_claimed(&self) -> bool {
        !matches!(self, ClaimState::Unclaimed)
    }
}

fn session_links(session_name: &str) -> Result<SessionLinks, String> {
    let original_repository = original_repository_name()?;
    let jist_branch = jist_branch_name(session_name);

    fetch_jist_branch(&jist_branch)?;
    let diffbase = git_output(
        [
            "rev-parse",
            &format!("refs/remotes/{JIST_REMOTE}/{jist_branch}^"),
        ],
        None,
    )?;

    let claim_state = claim_state_for_branch(&jist_branch)?;
    let sync_status = match claim_state {
        ClaimState::Unclaimed => None,
        _ => Some(sync_status_for_branch(&jist_branch)?),
    };

    Ok(SessionLinks {
        sessionbase_url: jisthub_session_url(&original_repository, session_name)?,
        sessionbase_commit_url: github_short_commit_url(&original_repository, &diffbase)?,
        sync_status,
        claim_state,
    })
}

fn claim_state_for_branch(jist_branch: &str) -> Result<ClaimState, String> {
    match remote_claim_owner(jist_branch)? {
        Some(claim) if claim.is_current_setup()? => Ok(ClaimState::ClaimedByCurrentSetup),
        Some(claim) => Ok(ClaimState::ClaimedByOther(claim)),
        None => Ok(ClaimState::Unclaimed),
    }
}

fn sync_status_for_branch(jist_branch: &str) -> Result<SyncStatus, String> {
    let remote_ref = format!("refs/remotes/{JIST_REMOTE}/{jist_branch}");
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

#[cfg(test)]
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

fn format_session_state_message(
    session_name: &str,
    claim_state: &ClaimState,
    sync_status: Option<&SyncStatus>,
    sessionbase_url: &str,
    sessionbase_commit_url: &str,
) -> String {
    match claim_state {
        ClaimState::Unclaimed => format!(
            "You are on the working commit for session {}.\n{}\n\nThis session is based on:\n{}\n\nTo work on this session, run {}.\n",
            format_current_session_name(session_name, false),
            format_url(sessionbase_url),
            format_url(sessionbase_commit_url),
            format_command("jist claim")
        ),
        ClaimState::ClaimedByCurrentSetup => match sync_status {
            Some(sync_status) if sync_status.unsynced_files == 0 => format!(
                "You have \x1b[32mclaimed\x1b[0m the working commit for session {}.\n{}\n\nThis session is based on:\n{}\n\nAll {} files are \x1b[32mfully synced\x1b[0m.\n",
                format_current_session_name(session_name, true),
                format_url(sessionbase_url),
                format_url(sessionbase_commit_url),
                sync_status.total_files
            ),
            Some(sync_status) => {
                let synchronized_files = sync_status
                    .total_files
                    .saturating_sub(sync_status.unsynced_files);
                format!(
                    "You have \x1b[32mclaimed\x1b[0m the working commit for session {}.\n{}\n\nThis session is based on:\n{}\n\n{} files are \x1b[31munsynchronized\x1b[0m.\n{} files are \x1b[32msynchronized\x1b[0m.\n",
                    format_current_session_name(session_name, true),
                    format_url(sessionbase_url),
                    format_url(sessionbase_commit_url),
                    sync_status.unsynced_files,
                    synchronized_files
                )
            }
            None => format!(
                "You have \x1b[32mclaimed\x1b[0m the working commit for session {}.\n{}\n\nThis session is based on:\n{}\n",
                format_current_session_name(session_name, true),
                format_url(sessionbase_url),
                format_url(sessionbase_commit_url)
            ),
        },
        ClaimState::ClaimedByOther(claimee_id) => {
            format!("Session {session_name} is \x1b[32mclaimed\x1b[0m by `{claimee_id}`.\n")
        }
    }
}

#[cfg(test)]
fn format_claim_state(claim_state: &ClaimState) -> String {
    match claim_state {
        ClaimState::Unclaimed => "\x1b[31mUnclaimed\x1b[0m".to_owned(),
        ClaimState::ClaimedByCurrentSetup => {
            "\x1b[32mClaimed by this machine and user\x1b[0m".to_owned()
        }
        ClaimState::ClaimedByOther(claimee_id) => {
            format!("\x1b[31mClaimed by `{claimee_id}`\x1b[0m")
        }
    }
}

fn format_current_session_name(session_name: &str, claimed: bool) -> String {
    if claimed {
        format!("\x1b[32m*\x1b[1;4m{session_name}\x1b[0m")
    } else {
        format!("\x1b[32m*{session_name}\x1b[0m")
    }
}

fn format_session_list_name(session_name: &str, current: bool, claimed: bool) -> String {
    match (current, claimed) {
        (true, true) => format!("\x1b[32m*\x1b[1;4m{session_name}\x1b[0m"),
        (true, false) => format!("\x1b[32m*{session_name}\x1b[0m"),
        (false, true) => format!("\x1b[1;4m{session_name}\x1b[0m"),
        (false, false) => session_name.to_owned(),
    }
}

#[cfg(test)]
fn session_banner_hint(claimed: bool) -> String {
    if claimed {
        "(\x1b[32;1;4mclaimed\x1b[0m, \x1b[32m*current session\x1b[0m)".to_owned()
    } else {
        "(\x1b[32m*current session\x1b[0m)".to_owned()
    }
}

fn format_url(url: &str) -> String {
    format!("\x1b[34;4m{url}\x1b[0m")
}

fn format_command(command: &str) -> String {
    format!("\x1b[32m{command}\x1b[0m")
}

#[cfg(test)]
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
    let repository = jist_repository_name(&source_repo_name)?;
    ensure_jist_remote(&repository)?;
    fetch_jist_sessions()?;

    let refs = git_output(
        [
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)%09%(committerdate:unix)",
            &format!("refs/remotes/{JIST_REMOTE}/jist-*"),
        ],
        None,
    )?;

    refs.lines()
        .filter_map(session_name_from_ref_line)
        .map(|name| {
            let claimed = claim_state_for_branch(&jist_branch_name(&name))?.is_claimed();

            Ok(SessionInfo { name, claimed })
        })
        .collect()
}

fn selected_session() -> Result<Option<String>, String> {
    let current_branch = git_output(["branch", "--show-current"], None)?;

    Ok(session_name_from_jist_branch(&current_branch).map(str::to_owned))
}

fn fetch_jist_sessions() -> Result<(), String> {
    git_status([
        "fetch",
        "--prune",
        JIST_REMOTE,
        &format!("+refs/heads/jist-*:refs/remotes/{JIST_REMOTE}/jist-*"),
    ])
}

fn fetch_jist_branch(jist_branch: &str) -> Result<(), String> {
    git_status([
        "fetch",
        JIST_REMOTE,
        &format!("+refs/heads/{jist_branch}:refs/remotes/{JIST_REMOTE}/{jist_branch}"),
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
    let output = git_output(["ls-remote", "--heads", JIST_REMOTE, branch_name], None)?;
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

fn session_name_from_ref_line(line: &str) -> Option<String> {
    let (ref_name, modified_at) = line.split_once('\t')?;
    let name = ref_name.strip_prefix(&format!("{JIST_REMOTE}/jist-"))?;
    let _modified_at: i64 = modified_at.parse().ok()?;

    Some(name.to_owned())
}

fn session_name_from_jist_branch(branch_name: &str) -> Option<&str> {
    branch_name
        .strip_prefix("jist-")
        .filter(|session| !session.is_empty())
}

fn jist_branch_name(session_name: &str) -> String {
    format!("jist-{}", escape_ref_part(session_name))
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

fn jist_repository_name(source_repo_name: &str) -> Result<String, String> {
    let owner = gh_output(["api", "user", "--jq", ".login"])?;

    Ok(format!("{owner}/jist-{source_repo_name}"))
}

fn github_commit_url(repository: &str, commit: &str) -> String {
    format!("https://github.com/{repository}/commit/{commit}")
}

fn jisthub_session_url(original_repository: &str, session_name: &str) -> Result<String, String> {
    let (owner, repository_name) = original_repository
        .split_once('/')
        .ok_or_else(|| format!("Could not parse repository `{original_repository}`."))?;

    Ok(format!(
        "https://jisthub.com/{owner}/{repository_name}/session/{}",
        escape_ref_part(session_name)
    ))
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

fn ensure_jist_remote(repository: &str) -> Result<(), String> {
    let remote_url = format!("https://github.com/{repository}.git");

    match git_output(["remote", "get-url", JIST_REMOTE], None) {
        Ok(current_url) if current_url == remote_url => Ok(()),
        Ok(_) => git_status(["remote", "set-url", JIST_REMOTE, &remote_url]),
        Err(_) => git_status(["remote", "add", JIST_REMOTE, &remote_url]),
    }
}

fn create_jist_meta_commit(session_name: &str) -> Result<String, String> {
    let head = git_output(["rev-parse", "HEAD"], None)?;
    let index_path = env::temp_dir().join(format!("jist-init-index-{}", std::process::id()));
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
                ".jist-meta",
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
        .map_err(|_| "GitHub CLI (`gh`) is required for `jist session`.".to_owned())
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
    fn formats_jist_branch_name() {
        assert_eq!(super::jist_branch_name("feature1"), "jist-feature1");
        assert_eq!(
            super::jist_branch_name("test session 2"),
            "jist-test-session-2"
        );
    }

    #[test]
    fn parses_session_from_ref_line() {
        let session = super::session_name_from_ref_line("jist/jist-test-session-2\t123").unwrap();

        assert_eq!(session, "test-session-2");
        assert!(super::session_name_from_ref_line("origin/main\t123").is_none());
    }

    #[test]
    fn parses_remote_branch_commit() {
        assert_eq!(
            super::parse_ls_remote_commit("jist-test", "abc123\trefs/heads/jist-test\n").unwrap(),
            Some("abc123".to_owned())
        );
        assert_eq!(
            super::parse_ls_remote_commit("jist-test", "").unwrap(),
            None
        );
    }

    #[test]
    fn parses_selected_session_from_branch() {
        assert_eq!(
            super::session_name_from_jist_branch("jist-test-session-2"),
            Some("test-session-2")
        );
        assert_eq!(super::session_name_from_jist_branch("main"), None);
    }

    #[test]
    fn formats_urls() {
        assert_eq!(
            super::github_commit_url("veryjos/source-repo", "abc123"),
            "https://github.com/veryjos/source-repo/commit/abc123"
        );
        assert_eq!(
            super::jisthub_session_url("veryjos/source-repo", "test session 2").unwrap(),
            "https://jisthub.com/veryjos/source-repo/session/test-session-2"
        );
        assert_eq!(
            super::format_url("https://jisthub.com/veryjos/source-repo/session/test"),
            "\x1b[34;4mhttps://jisthub.com/veryjos/source-repo/session/test\x1b[0m"
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
    fn formats_session_state_messages() {
        assert_eq!(
            super::format_session_state_message(
                "moop",
                &super::ClaimState::Unclaimed,
                None,
                "https://jisthub.com/veryjos/jist-test/session/moop",
                "https://github.com/veryjos/jist-test/commit/c5b8bce",
            ),
            "You are on the working commit for session \x1b[32m*moop\x1b[0m.\n\x1b[34;4mhttps://jisthub.com/veryjos/jist-test/session/moop\x1b[0m\n\nThis session is based on:\n\x1b[34;4mhttps://github.com/veryjos/jist-test/commit/c5b8bce\x1b[0m\n\nTo work on this session, run \x1b[32mjist claim\x1b[0m.\n"
        );
        assert_eq!(
            super::format_session_state_message(
                "moop",
                &super::ClaimState::ClaimedByCurrentSetup,
                Some(&super::SyncStatus {
                    unsynced_files: 0,
                    total_files: 4,
                }),
                "https://jisthub.com/veryjos/jist-test/session/moop",
                "https://github.com/veryjos/jist-test/commit/c5b8bce",
            ),
            "You have \x1b[32mclaimed\x1b[0m the working commit for session \x1b[32m*\x1b[1;4mmoop\x1b[0m.\n\x1b[34;4mhttps://jisthub.com/veryjos/jist-test/session/moop\x1b[0m\n\nThis session is based on:\n\x1b[34;4mhttps://github.com/veryjos/jist-test/commit/c5b8bce\x1b[0m\n\nAll 4 files are \x1b[32mfully synced\x1b[0m.\n"
        );
        assert_eq!(
            super::format_session_state_message(
                "moop",
                &super::ClaimState::ClaimedByCurrentSetup,
                Some(&super::SyncStatus {
                    unsynced_files: 2,
                    total_files: 4,
                }),
                "https://jisthub.com/veryjos/jist-test/session/moop",
                "https://github.com/veryjos/jist-test/commit/c5b8bce",
            ),
            "You have \x1b[32mclaimed\x1b[0m the working commit for session \x1b[32m*\x1b[1;4mmoop\x1b[0m.\n\x1b[34;4mhttps://jisthub.com/veryjos/jist-test/session/moop\x1b[0m\n\nThis session is based on:\n\x1b[34;4mhttps://github.com/veryjos/jist-test/commit/c5b8bce\x1b[0m\n\n2 files are \x1b[31munsynchronized\x1b[0m.\n2 files are \x1b[32msynchronized\x1b[0m.\n"
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
            super::format_claim_state(&super::ClaimState::ClaimedByOther(super::ClaimeeId {
                user: "jos".to_owned(),
                machine: "desktop".to_owned(),
            })),
            "\x1b[31mClaimed by `jos@desktop`\x1b[0m"
        );
    }

    #[test]
    fn formats_session_markers() {
        assert_eq!(
            super::format_current_session_name("moop", false),
            "\x1b[32m*moop\x1b[0m"
        );
        assert_eq!(
            super::format_current_session_name("moop", true),
            "\x1b[32m*\x1b[1;4mmoop\x1b[0m"
        );
        assert_eq!(
            super::format_session_list_name("moop", true, false),
            "\x1b[32m*moop\x1b[0m"
        );
        assert_eq!(
            super::format_session_list_name("moop", true, true),
            "\x1b[32m*\x1b[1;4mmoop\x1b[0m"
        );
        assert_eq!(
            super::format_session_list_name("moop", false, true),
            "\x1b[1;4mmoop\x1b[0m"
        );
        assert_eq!(
            super::format_session_list_name("moop", false, false),
            "moop"
        );
        assert_eq!(
            super::session_banner_hint(true),
            "(\x1b[32;1;4mclaimed\x1b[0m, \x1b[32m*current session\x1b[0m)"
        );
        assert_eq!(
            super::session_banner_hint(false),
            "(\x1b[32m*current session\x1b[0m)"
        );
    }

    #[test]
    fn formats_and_parses_claimee_id() {
        let claimee_id = super::ClaimeeId::parse("jos@desktop").unwrap();

        assert_eq!(claimee_id.user, "jos");
        assert_eq!(claimee_id.machine, "desktop");
        assert_eq!(claimee_id.to_string(), "jos@desktop");
        assert!(super::ClaimeeId::parse("jos").is_err());
        assert!(super::ClaimeeId::parse("@desktop").is_err());
        assert!(super::ClaimeeId::parse("jos@").is_err());
    }

    #[test]
    fn reads_metadata_values() {
        let metadata = "claimee_id=jos@desktop\nclaimed_at_utc_unix_seconds=42\n";

        assert_eq!(
            super::metadata_value(metadata, "claimee_id"),
            Some("jos@desktop")
        );
        assert_eq!(super::metadata_value(metadata, "missing"), None);
    }

    #[test]
    fn unclaimed_metadata_has_no_claim_owner() {
        let metadata = super::unclaimed_metadata();

        assert_eq!(super::metadata_value(metadata, "claimee_id"), None);
        assert_eq!(super::metadata_value(metadata, "machine_id"), None);
        assert_eq!(super::metadata_value(metadata, "local_user"), None);
    }

    #[test]
    fn detects_git_internal_paths() {
        assert!(super::is_git_internal_path(std::path::Path::new(
            ".git/refs/heads/main"
        )));
        assert!(!super::is_git_internal_path(std::path::Path::new(
            "src/main.rs"
        )));
    }

    #[test]
    fn pins_current_session_in_visible_list() {
        let sessions = vec![
            super::SessionInfo {
                name: "newest".to_owned(),
                claimed: false,
            },
            super::SessionInfo {
                name: "current".to_owned(),
                claimed: true,
            },
            super::SessionInfo {
                name: "older".to_owned(),
                claimed: false,
            },
            super::SessionInfo {
                name: "oldest".to_owned(),
                claimed: false,
            },
        ];
        let visible = super::visible_sessions(&sessions, Some("current"));

        assert_eq!(visible[0].name, "current");
        assert_eq!(visible.len(), super::STATUS_SESSION_LIMIT);
    }
}
