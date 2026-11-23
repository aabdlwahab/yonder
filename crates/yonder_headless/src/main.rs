//! The `yonder` program for machines without a screen.

use std::ffi::OsString;

const USAGE: &str = "\
Yonder for machines without a screen.

Usage:
  yonder hub <command>      Run the hub, or talk to it (yonder hub --help)
  yonder server <command>   The machine server Yonder starts over SSH
";

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if let Some((job, args)) = yonder_headless::job_from_args(&args) {
        yonder_headless::run_job(job, args);
    }
    let first = args.get(1).and_then(|arg| arg.to_str());
    if matches!(first, None | Some("help" | "-h" | "--help")) {
        print!("{USAGE}");
        return;
    }
    // A copy the app uploads is started with the server's own arguments
    // (`proxy`, `run`, `version`, `--askpass` …), so the rest is the server.
    yonder_headless::run_server(args, Vec::new());
}
