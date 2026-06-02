use chrono::{DateTime, Datelike, Local};
use clap::{Parser, Subcommand};
use notify::{RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
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
    /// Check out an existing JIST session if the working tree is clean.
    Checkout {
        /// Name for this JIST session.
        session_name: String,
    },
    /// Delete a JIST session from the private remote.
    Delete {
        /// Name for this JIST session.
        session_name: String,
    },
    /// List all JIST sessions on the private remote.
    List,
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
        /// Name for this JIST session. Defaults to the current session.
        session_name: Option<String>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        CommandArgs::Claim => claim(),
        CommandArgs::Checkout { session_name } => checkout(&session_name),
        CommandArgs::Delete { session_name } => delete(&session_name),
        CommandArgs::List => list(),
        CommandArgs::Push => push(),
        CommandArgs::Session { session_name } => session(&session_name),
        CommandArgs::Status => status(),
        CommandArgs::Unclaim => unclaim(),
        CommandArgs::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        CommandArgs::Work { session_name } => work(session_name.as_deref()),
    }
}

fn list() -> ExitCode {
    match render_all_sessions() {
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

fn checkout(session_name: &str) -> ExitCode {
    match checkout_session(session_name) {
        Ok(()) => {
            println!("Checked out JIST session `{session_name}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn delete(session_name: &str) -> ExitCode {
    match delete_session(session_name) {
        Ok(()) => {
            println!("Deleted JIST session `{session_name}`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn work(session_name: Option<&str>) -> ExitCode {
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
    let current_head = git_output(["rev-parse", "HEAD"], None)?;
    let current_short_head = git_output(["rev-parse", "--short", "HEAD"], None)?;
    let selected_session = selected_session_name(&sessions, &current_head)?;
    let mut output = String::new();

    if let Some(session) = sessions
        .iter()
        .find(|session| session.working_commit == current_head)
    {
        output.push_str(&format_session_working_commit_status(session, None)?);
        output.push('\n');
        output.push_str("Sessions:\n");
        let session_refs: Vec<&SessionInfo> = sessions.iter().collect();
        output.push_str(&format_short_session_list(
            &session_refs,
            selected_session.as_deref(),
        )?);

        return Ok(output);
    }

    let (based_on_current, other_sessions): (Vec<&SessionInfo>, Vec<&SessionInfo>) = sessions
        .iter()
        .partition(|session| session.base_commit == current_head);

    output.push_str(&format!(
        "You are on git commit {}.\n",
        format_blue(&current_short_head)
    ));
    output.push_str(&format!(
        "The following sessions are {}:\n",
        format_green("based on this commit")
    ));
    output.push_str(&format_short_session_list(
        &based_on_current,
        selected_session.as_deref(),
    )?);

    output.push('\n');
    output.push_str("Other sessions in this repository:\n");
    output.push_str(&format_short_session_list(
        &other_sessions,
        selected_session.as_deref(),
    )?);

    output.push('\n');
    output.push_str(&format!(
        "To display all sessions, run {}.\n",
        format_blue("jist list")
    ));
    output.push_str(&format!(
        "To work on a session, run {}.\n",
        format_blue("jist work <session name>")
    ));

    Ok(output)
}

fn format_session_working_commit_status(
    session: &SessionInfo,
    push_deadline: Option<Instant>,
) -> Result<String, String> {
    let original_repository = original_repository_name()?;
    let session_url = jisthub_session_url(&original_repository, &session.name)?;
    let short_base_commit = git_output(["rev-parse", "--short", &session.base_commit], None)?;
    let jist_branch = jist_branch_name(&session.name);
    let sync_status = sync_status_for_branch(&jist_branch)?;
    let claim = remote_claim_info(&jist_branch)?;
    let banner = format_session_working_commit_banner(session, &short_base_commit, claim.as_ref());
    let work_indicator = format_session_work_indicator(claim.as_ref())?;
    let sync_message = format_session_sync_status(&sync_status);
    let sync_section = match push_deadline {
        Some(deadline) => format!("{sync_message}\n{}", format_scheduled_sync(deadline)),
        None => sync_message,
    };

    Ok(format!(
        "{banner}\nVisit {} for detailed information.\n\n{}\n\n{}\n",
        format_url(&session_url),
        sync_section,
        work_indicator
    ))
}

fn format_session_working_commit_banner(
    session: &SessionInfo,
    short_base_commit: &str,
    claim: Option<&ClaimInfo>,
) -> String {
    if claim
        .map(|claim| claim.claimee_id.is_current_setup().unwrap_or(false))
        .unwrap_or(false)
    {
        format!(
            "You have {} session {}, based on {}.",
            format_green("claimed"),
            format_selected_session_name(&session.name),
            format_blue(short_base_commit)
        )
    } else {
        format!(
            "You are checked out on session {}, based on {}.",
            format_selected_session_name(&session.name),
            format_blue(short_base_commit)
        )
    }
}

fn format_session_work_indicator(claim: Option<&ClaimInfo>) -> Result<String, String> {
    match claim {
        Some(claim) if claim.claimee_id.is_current_setup().unwrap_or(false) => Ok(format!(
            "{}\nTo unclaim the current session, run {}.",
            format_current_user_claim_message(claim),
            format_blue("jist unclaim")
        )),
        Some(claim) => Ok(format!(
            "This session is {} by another developer: {}.\n{}",
            format_red("claimed"),
            claim.claimee_id,
            format_claim_duration(&claim)
        )),
        None => Ok(format!(
            "To work on this session, type {}.",
            format_blue("jist work")
        )),
    }
}

fn format_current_user_claim_message(claim: &ClaimInfo) -> String {
    match claim.claimed_at_utc_unix_seconds {
        Some(claimed_at) => format!(
            "Your user ({}) created its claim {} ago.",
            claim.claimee_id,
            format_elapsed_since_unix_seconds(claimed_at)
        ),
        None => format!(
            "Your user ({}) created its claim at an unknown time.",
            claim.claimee_id
        ),
    }
}

fn format_claim_duration(claim: &ClaimInfo) -> String {
    match claim.claimed_at_utc_unix_seconds {
        Some(claimed_at) => format!(
            "Claimed since {} ({} ago).",
            format_local_claim_timestamp(claimed_at),
            format_elapsed_since_unix_seconds(claimed_at)
        ),
        None => "Claimed for an unknown duration.".to_owned(),
    }
}

fn format_local_claim_timestamp(unix_seconds: i64) -> String {
    let timestamp = DateTime::from_timestamp(unix_seconds, 0)
        .map(|utc| utc.with_timezone(&Local))
        .unwrap_or_else(Local::now);
    let month = timestamp.format("%B");
    let day = timestamp.day();
    let hour = timestamp.format("%-I%P");
    let timezone = timestamp.format("%Z");

    format!("{month} {} at {hour} {timezone}", format_ordinal_day(day))
}

fn format_ordinal_day(day: u32) -> String {
    let suffix = match day % 100 {
        11..=13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };

    format!("{day}{suffix}")
}

fn render_all_sessions() -> Result<String, String> {
    let sessions = list_sessions()?;
    let current_head = git_output(["rev-parse", "HEAD"], None)?;
    let selected_session = selected_session_name(&sessions, &current_head)?;
    let session_refs: Vec<&SessionInfo> = sessions.iter().collect();
    let mut output = String::new();

    output.push_str("Sessions:\n");
    output.push_str(&format_full_session_list(
        &session_refs,
        selected_session.as_deref(),
    )?);

    Ok(output)
}

fn format_short_session_list(
    sessions: &[&SessionInfo],
    selected_session: Option<&str>,
) -> Result<String, String> {
    format_session_list(sessions, Some(STATUS_SESSION_LIMIT), selected_session)
}

fn format_full_session_list(
    sessions: &[&SessionInfo],
    selected_session: Option<&str>,
) -> Result<String, String> {
    format_session_list(sessions, None, selected_session)
}

fn format_session_list(
    sessions: &[&SessionInfo],
    limit: Option<usize>,
    selected_session: Option<&str>,
) -> Result<String, String> {
    let mut output = String::new();

    if sessions.is_empty() {
        output.push_str("  <no sessions>\n");
    } else {
        let visible_count = limit.unwrap_or(sessions.len()).min(sessions.len());
        for session in sessions.iter().take(visible_count) {
            let current = selected_session == Some(session.name.as_str());
            let claimed = remote_claim_info(&jist_branch_name(&session.name))?.is_some();
            output.push_str(&format!(
                "  - {} ({})\n",
                format_session_name(&session.name, selected_session),
                format_session_list_metadata(session, current, claimed)
            ));
        }

        let hidden_count = sessions.len().saturating_sub(visible_count);
        if hidden_count > 0 {
            output.push_str(&format!(
                "    +{hidden_count} older {}\n",
                pluralize("session", hidden_count)
            ));
        }
    }

    Ok(output)
}

fn format_session_list_metadata(session: &SessionInfo, current: bool, claimed: bool) -> String {
    let mut metadata = Vec::new();

    if current {
        metadata.push(format_green("current session"));
    }

    if claimed {
        metadata.push(format_green("claimed"));
    }

    metadata.push(format_last_modified(session.modified_at_unix_seconds));
    metadata.join(", ")
}

fn selected_session_name(
    sessions: &[SessionInfo],
    current_head: &str,
) -> Result<Option<String>, String> {
    if let Some(session) = sessions
        .iter()
        .find(|session| session.working_commit == current_head)
    {
        return Ok(Some(session.name.clone()));
    }

    selected_session()
}

fn current_session_name() -> Result<Option<String>, String> {
    let sessions = list_sessions()?;
    let current_head = git_output(["rev-parse", "HEAD"], None)?;

    selected_session_name(&sessions, &current_head)
}

fn format_session_name(session_name: &str, selected_session: Option<&str>) -> String {
    if selected_session == Some(session_name) {
        format_selected_session_name(session_name)
    } else {
        session_name.to_owned()
    }
}

fn format_selected_session_name(session_name: &str) -> String {
    format!("\x1b[32m*{session_name}\x1b[0m")
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

fn checkout_session(session_name: &str) -> Result<(), String> {
    ensure_clean_worktree()?;
    checkout_jist_branch(&jist_branch_name(session_name))
}

fn delete_session(session_name: &str) -> Result<(), String> {
    ensure_gh_installed()?;

    let source_repo_name = repository_name()?;
    let repository = jist_repository_name(&source_repo_name)?;
    ensure_jist_remote(&repository)?;

    let jist_branch = jist_branch_name(session_name);
    if remote_branch_commit(&jist_branch)?.is_none() {
        return Err(format!("JIST session `{session_name}` does not exist."));
    }

    require_delete_confirmation(session_name)?;
    git_status(["push", JIST_REMOTE, &format!(":refs/heads/{jist_branch}")])?;
    fetch_jist_sessions()?;

    Ok(())
}

fn require_delete_confirmation(session_name: &str) -> Result<(), String> {
    print!("Delete JIST session `{session_name}`? Type `confirm` to continue: ");
    io::stdout()
        .flush()
        .map_err(|error| format!("Failed to write confirmation prompt: {error}"))?;

    let mut response = String::new();
    io::stdin()
        .read_line(&mut response)
        .map_err(|error| format!("Failed to read confirmation: {error}"))?;

    if response.trim() == "confirm" {
        Ok(())
    } else {
        Err("Delete cancelled.".to_owned())
    }
}

fn ensure_clean_worktree() -> Result<(), String> {
    let status = git_output(["status", "--porcelain"], None)?;

    if status.is_empty() {
        Ok(())
    } else {
        Err("Cannot check out a JIST session with uncommitted working changes.".to_owned())
    }
}

fn work_session(session_name: Option<&str>) -> Result<(), String> {
    let session_name = match session_name {
        Some(session_name) => {
            start_jist_session(session_name)?;
            session_name.to_owned()
        }
        None => current_session_name()?.ok_or_else(|| {
            "Check out a JIST session or pass a session name before running `jist work`.".to_owned()
        })?,
    };

    ensure_fully_synchronized(&jist_branch_name(&session_name))?;
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

fn run_work_loop() -> Result<(), String> {
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
    ctrlc::set_handler(move || {
        if signal_running.swap(false, Ordering::SeqCst) {
            eprintln!("Ctrl+C pressed. Attempting to unclaim and exit...");
        }
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
    render_work_status(&message, push_deadline)?;

    while running.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => {
                if event.paths.iter().any(|path| !is_git_internal_path(path)) {
                    push_deadline = Some(Instant::now() + WORK_PUSH_DEBOUNCE);
                    message = format!(
                        "Change detected. Next push after {} minutes of quiet.",
                        WORK_PUSH_DEBOUNCE.as_secs() / 60
                    );
                    render_work_status(&message, push_deadline)?;
                }
            }
            Ok(Err(error)) => {
                message = format!("Filesystem watcher warning: {error}");
                render_work_status(&message, push_deadline)?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                render_work_status(&message, push_deadline)?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Filesystem watcher stopped unexpectedly.".to_owned());
            }
        }

        if push_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            message = "Pushing changes...".to_owned();
            render_work_status(&message, push_deadline)?;

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

            render_work_status(&message, push_deadline)?;
        }
    }

    render_work_status("Stopping. Unclaiming session...", push_deadline)?;
    Ok(())
}

fn render_work_status(message: &str, push_deadline: Option<Instant>) -> Result<(), String> {
    print!("\x1b[2J\x1b[H");
    print!("{}", render_work_session_status(push_deadline)?);
    println!();
    println!("Work: {message}");

    Ok(())
}

fn format_scheduled_sync(deadline: Instant) -> String {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let total_seconds = remaining.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;

    format!("Sync scheduled in {minutes}:{seconds:02}")
}

fn render_work_session_status(push_deadline: Option<Instant>) -> Result<String, String> {
    let sessions = list_sessions()?;
    let current_head = git_output(["rev-parse", "HEAD"], None)?;

    if let Some(session) = sessions
        .iter()
        .find(|session| session.working_commit == current_head)
    {
        format_session_working_commit_status(session, push_deadline)
    } else {
        render_status()
    }
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
    ensure_fully_synchronized(&current_branch)?;
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

fn ensure_fully_synchronized(jist_branch: &str) -> Result<(), String> {
    let sync_status = sync_status_for_branch(jist_branch)?;

    if sync_status.unsynced_files == 0 {
        Ok(())
    } else {
        Err(format!(
            "Cannot claim this JIST session while {} {} unsynchronized.",
            sync_status.unsynced_files,
            pluralize("file", sync_status.unsynced_files)
        ))
    }
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
    Ok(remote_claim_info(jist_branch)?.map(|claim| claim.claimee_id))
}

fn remote_claim_info(jist_branch: &str) -> Result<Option<ClaimInfo>, String> {
    let metadata = git_output(
        [
            "show",
            &format!("refs/remotes/{JIST_REMOTE}/{jist_branch}:.jist-meta"),
        ],
        None,
    )?;
    let claimed_at_utc_unix_seconds = metadata_value(&metadata, "claimed_at_utc_unix_seconds")
        .and_then(|value| value.parse().ok());

    if let Some(claimee_id) = metadata_value(&metadata, "claimee_id") {
        return Ok(Some(ClaimInfo {
            claimee_id: ClaimeeId::parse(claimee_id)?,
            claimed_at_utc_unix_seconds,
        }));
    }

    match (
        metadata_value(&metadata, "machine_id"),
        metadata_value(&metadata, "local_user"),
    ) {
        (Some(machine_id), Some(local_user)) => Ok(Some(ClaimInfo {
            claimee_id: ClaimeeId {
                user: local_user.to_owned(),
                machine: machine_id.to_owned(),
            },
            claimed_at_utc_unix_seconds,
        })),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaimInfo {
    claimee_id: ClaimeeId,
    claimed_at_utc_unix_seconds: Option<i64>,
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
    working_commit: String,
    base_commit: String,
    modified_at_unix_seconds: i64,
}

#[cfg(test)]
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

#[cfg(test)]
enum ClaimState {
    Unclaimed,
    ClaimedByCurrentSetup,
    ClaimedByOther(ClaimeeId),
}

#[cfg(test)]
impl ClaimState {
    fn is_claimed(&self) -> bool {
        !matches!(self, ClaimState::Unclaimed)
    }
}

#[cfg(test)]
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

#[cfg(test)]
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

fn format_session_sync_status(sync_status: &SyncStatus) -> String {
    if sync_status.unsynced_files == 0 {
        format!(
            "All {} {} {} \x1b[32mfully synced\x1b[0m.",
            sync_status.total_files,
            pluralize("file", sync_status.total_files),
            pluralized_verb(sync_status.total_files)
        )
    } else {
        let synchronized_files = sync_status
            .total_files
            .saturating_sub(sync_status.unsynced_files);

        format!(
            "{} {} {} \x1b[31munsynchronized\x1b[0m.\n{} {} {} \x1b[32msynchronized\x1b[0m.",
            sync_status.unsynced_files,
            pluralize("file", sync_status.unsynced_files),
            pluralized_verb(sync_status.unsynced_files),
            synchronized_files,
            pluralize("file", synchronized_files),
            pluralized_verb(synchronized_files)
        )
    }
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

#[cfg(test)]
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
                "You have \x1b[32mclaimed\x1b[0m the working commit for session {}.\n{}\n\nThis session is based on:\n{}\n\nAll {} {} {} \x1b[32mfully synced\x1b[0m.\n",
                format_current_session_name(session_name, true),
                format_url(sessionbase_url),
                format_url(sessionbase_commit_url),
                sync_status.total_files,
                pluralize("file", sync_status.total_files),
                pluralized_verb(sync_status.total_files)
            ),
            Some(sync_status) => {
                let synchronized_files = sync_status
                    .total_files
                    .saturating_sub(sync_status.unsynced_files);
                format!(
                    "You have \x1b[32mclaimed\x1b[0m the working commit for session {}.\n{}\n\nThis session is based on:\n{}\n\n{} {} {} \x1b[31munsynchronized\x1b[0m.\n{} {} {} \x1b[32msynchronized\x1b[0m.\n",
                    format_current_session_name(session_name, true),
                    format_url(sessionbase_url),
                    format_url(sessionbase_commit_url),
                    sync_status.unsynced_files,
                    pluralize("file", sync_status.unsynced_files),
                    pluralized_verb(sync_status.unsynced_files),
                    synchronized_files,
                    pluralize("file", synchronized_files),
                    pluralized_verb(synchronized_files)
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

#[cfg(test)]
fn format_current_session_name(session_name: &str, claimed: bool) -> String {
    if claimed {
        format!("\x1b[32m*\x1b[1;4m{session_name}\x1b[0m")
    } else {
        format!("\x1b[32m*{session_name}\x1b[0m")
    }
}

#[cfg(test)]
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

fn format_blue(text: &str) -> String {
    format!("\x1b[34m{text}\x1b[0m")
}

fn format_green(text: &str) -> String {
    format!("\x1b[32m{text}\x1b[0m")
}

fn format_red(text: &str) -> String {
    format!("\x1b[31m{text}\x1b[0m")
}

fn format_url(url: &str) -> String {
    format!("\x1b[34;4m{url}\x1b[0m")
}

#[cfg(test)]
fn format_command(command: &str) -> String {
    format!("\x1b[32m{command}\x1b[0m")
}

fn format_last_modified(unix_seconds: i64) -> String {
    format!(
        "last modified {}",
        format_time_since_unix_seconds(unix_seconds)
    )
}

fn format_time_since_unix_seconds(unix_seconds: i64) -> String {
    format!("{} ago", format_elapsed_since_unix_seconds(unix_seconds))
}

fn format_elapsed_since_unix_seconds(unix_seconds: i64) -> String {
    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let elapsed_seconds = now_seconds.saturating_sub(unix_seconds).max(0) as u64;

    format_duration(elapsed_seconds)
}

fn format_duration(total_seconds: u64) -> String {
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;

    format!("{days}d {hours}h {minutes}m")
}

fn pluralize(word: &str, count: usize) -> &str {
    if count == 1 {
        word
    } else if word == "session" {
        "sessions"
    } else {
        "files"
    }
}

fn pluralized_verb(count: usize) -> &'static str {
    if count == 1 { "is" } else { "are" }
}

#[cfg(test)]
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
            "--format=%(refname:short)%09%(committerdate:unix)%09%(objectname)",
            &format!("refs/remotes/{JIST_REMOTE}/jist-*"),
        ],
        None,
    )?;

    refs.lines()
        .filter_map(session_from_ref_line)
        .map(|session| {
            let jist_branch = jist_branch_name(&session.name);
            let base_commit = git_output(
                [
                    "rev-parse",
                    &format!("refs/remotes/{JIST_REMOTE}/{jist_branch}^"),
                ],
                None,
            )?;

            Ok(SessionInfo {
                base_commit,
                ..session
            })
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

fn session_from_ref_line(line: &str) -> Option<SessionInfo> {
    let mut fields = line.split('\t');
    let ref_name = fields.next()?;
    let modified_at = fields.next()?;
    let working_commit = fields.next()?;
    let name = ref_name.strip_prefix(&format!("{JIST_REMOTE}/jist-"))?;
    let modified_at_unix_seconds: i64 = modified_at.parse().ok()?;

    Some(SessionInfo {
        name: name.to_owned(),
        working_commit: working_commit.to_owned(),
        base_commit: String::new(),
        modified_at_unix_seconds,
    })
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

#[cfg(test)]
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

#[cfg(test)]
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
        let session =
            super::session_from_ref_line("jist/jist-test-session-2\t123\tabc123").unwrap();

        assert_eq!(session.name, "test-session-2");
        assert_eq!(session.modified_at_unix_seconds, 123);
        assert_eq!(session.working_commit, "abc123");
        assert!(super::session_from_ref_line("origin/main\t123").is_none());
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
            super::format_session_sync_status(&super::SyncStatus {
                unsynced_files: 1,
                total_files: 5,
            }),
            "1 file is \x1b[31munsynchronized\x1b[0m.\n4 files are \x1b[32msynchronized\x1b[0m."
        );
        assert_eq!(
            super::format_session_sync_status(&super::SyncStatus {
                unsynced_files: 0,
                total_files: 1,
            }),
            "All 1 file is \x1b[32mfully synced\x1b[0m."
        );
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
                working_commit: "working-4".to_owned(),
                base_commit: "base-4".to_owned(),
                modified_at_unix_seconds: 4,
            },
            super::SessionInfo {
                name: "current".to_owned(),
                working_commit: "working-3".to_owned(),
                base_commit: "base-3".to_owned(),
                modified_at_unix_seconds: 3,
            },
            super::SessionInfo {
                name: "older".to_owned(),
                working_commit: "working-2".to_owned(),
                base_commit: "base-2".to_owned(),
                modified_at_unix_seconds: 2,
            },
            super::SessionInfo {
                name: "oldest".to_owned(),
                working_commit: "working-1".to_owned(),
                base_commit: "base-1".to_owned(),
                modified_at_unix_seconds: 1,
            },
        ];
        let visible = super::visible_sessions(&sessions, Some("current"));

        assert_eq!(visible[0].name, "current");
        assert_eq!(visible.len(), super::STATUS_SESSION_LIMIT);
    }
}
