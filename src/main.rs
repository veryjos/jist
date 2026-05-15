use std::env;
use std::process::ExitCode;

const APP_NAME: &str = "Joe's Intermediate Tracker";

fn main() -> ExitCode {
    let mut args = env::args().skip(1);

    match args.next().as_deref() {
        Some("sync") => sync(args.collect()),
        Some("-h" | "--help") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!("Unknown command: {command}");
            eprintln!("Run `jit --help` for usage.");
            ExitCode::FAILURE
        }
    }
}

fn sync(args: Vec<String>) -> ExitCode {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_sync_help();
        return ExitCode::SUCCESS;
    }

    if !args.is_empty() {
        eprintln!("The `sync` command does not accept arguments yet.");
        eprintln!("Run `jit sync --help` for usage.");
        return ExitCode::FAILURE;
    }

    println!("Syncing {APP_NAME}...");
    println!("Nothing to sync yet.");
    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "{APP_NAME}

Usage:
    jit <command>

Commands:
    sync        Sync tracker data

Options:
    -h, --help      Show help
    -V, --version   Show version"
    );
}

fn print_sync_help() {
    println!(
        "{APP_NAME} sync

Usage:
    jit sync

Syncs tracker data. The storage and remote sync backends are not configured yet."
    );
}
