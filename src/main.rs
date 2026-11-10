//! Archfleet: a terminal dashboard for the machines in your home, over plain SSH.
//! One binary: the dashboard, the per-machine helper, clipdeck and the clipboard command.
#![allow(clippy::too_many_arguments, clippy::type_complexity)]
mod backend;
mod cli;
mod clipboards;
#[cfg(unix)]
mod clipdeck;
mod config;
#[cfg(unix)]
mod helper;
#[cfg(unix)]
mod procs;
mod tui;
mod util;

const USAGE: &str = "archfleet — your home machines over SSH

  archfleet                      the dashboard
  archfleet --clipboard          ... opened on the Clipboard tab
  archfleet --wake <alias>       send a Wake-on-LAN packet and exit
  archfleet clipboard ...        read or copy clipboards (also installed as archfleet-clipboard)
  archfleet clipdeck ...         the SUPER+V clipboard picker (also installed as clipdeck)
  archfleet setup-ssh [--copy | --check]
                                 SSH aliases and a key for the machines in hosts.json
  archfleet check                read-only check against every machine
  archfleet helper --stdin       the per-machine helper (the dashboard runs it over SSH)
  archfleet --version";

fn main() {
    let mut argv: Vec<String> = std::env::args().collect();
    let program =
        std::path::Path::new(argv.first().map(String::as_str).unwrap_or("archfleet")).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let rest: Vec<String> = argv.drain(1..).collect();
    let code = match program.as_str() {
        "archfleet-clipboard" => cli::clipboard(&rest),
        #[cfg(unix)]
        "clipdeck" => clipdeck::main(&rest),
        _ => dispatch(&rest),
    };
    std::process::exit(code);
}

fn dispatch(rest: &[String]) -> i32 {
    match rest.first().map(String::as_str) {
        #[cfg(unix)]
        Some("helper") => helper::main_stdin(),
        Some("clipboard") => cli::clipboard(&rest[1..]),
        #[cfg(unix)]
        Some("clipdeck") => clipdeck::main(&rest[1..]),
        Some("setup-ssh") => cli::setup_ssh(&rest[1..]),
        Some("check") => cli::live_check(),
        Some("--wake") => cli::wake(rest.get(1).map(String::as_str).unwrap_or("")),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            0
        }
        Some("-V" | "--version") => {
            println!("archfleet {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--clipboard") | None => tui::run(rest.first().is_some_and(|a| a == "--clipboard")),
        Some(other) => {
            eprintln!("archfleet: unknown command {other:?} (try archfleet --help)");
            2
        }
    }
}
