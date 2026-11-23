//! One `yonder` program does every job. This crate holds the jobs that need
//! no screen, so the program for servers (`main.rs` here) and the app's own
//! binary run them the same way:
//!
//! - `yonder server …`: the machine server the app starts over SSH.
//! - `yonder hub …`: the hub that owns agent threads, and its command line.

use std::ffi::{OsStr, OsString};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    /// The machine server. `prefixed` when it was started as
    /// `yonder server …`, so it starts its daemon the same way; a copy the
    /// app uploaded under the server's own name isn't.
    Server {
        prefixed: bool,
    },
    Hub,
}

/// The job a command line asks for, with the arguments to run it with (a
/// program name first, then that job's own arguments). `None` when the
/// command line names no job.
pub fn job_from_args(args: &[OsString]) -> Option<(Job, Vec<OsString>)> {
    let (program, rest) = args.split_first()?;
    // The hub's old program name, now a link to `yonder`.
    if Path::new(program).file_stem() == Some(OsStr::new("yonder-hub")) {
        return Some((Job::Hub, with_program("yonder hub", rest)));
    }
    // A copy the app uploads as a machine's server (`zed-remote-server-…`)
    // only ever does the server's job, with the server's own arguments.
    if Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with("zed-remote-server"))
    {
        return Some((
            Job::Server { prefixed: false },
            with_program("yonder server", rest),
        ));
    }
    let (first, rest) = rest.split_first()?;
    match first.to_str()? {
        "hub" => Some((Job::Hub, with_program("yonder hub", rest))),
        "server" => Some((
            Job::Server { prefixed: true },
            with_program("yonder server", rest),
        )),
        _ => None,
    }
}

fn with_program(program: &str, rest: &[OsString]) -> Vec<OsString> {
    std::iter::once(OsString::from(program))
        .chain(rest.iter().cloned())
        .collect()
}

/// Runs `job` and exits with its result.
pub fn run_job(job: Job, args: Vec<OsString>) -> ! {
    match job {
        Job::Hub => {
            yonder_hub::run_cli(args);
            std::process::exit(0);
        }
        // The server starts its daemon from this program again, so it puts
        // `server` back in front of the daemon's arguments.
        Job::Server { prefixed: true } => run_server(args, vec![OsString::from("server")]),
        Job::Server { prefixed: false } => run_server(args, Vec::new()),
    }
}

/// Runs the machine server's command line and exits with its result.
/// `self_args` go in front of the arguments when the server starts itself.
pub fn run_server(args: Vec<OsString>, self_args: Vec<OsString>) -> ! {
    match remote_server::run_cli(args, self_args) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("Error: {error:?}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn jobs_from_the_command_line() {
        assert_eq!(
            job_from_args(&args(&["/usr/bin/yonder", "hub", "list"])),
            Some((Job::Hub, args(&["yonder hub", "list"])))
        );
        assert_eq!(
            job_from_args(&args(&["yonder", "server", "proxy", "--identifier", "x"])),
            Some((
                Job::Server { prefixed: true },
                args(&["yonder server", "proxy", "--identifier", "x"])
            ))
        );
        assert_eq!(
            job_from_args(&args(&["/home/a/.local/bin/yonder-hub", "serve"])),
            Some((Job::Hub, args(&["yonder hub", "serve"])))
        );
        assert_eq!(
            job_from_args(&args(&[
                "/home/a/.zed_server/zed-remote-server-stable-1.30.0+stable.abc",
                "version"
            ])),
            Some((
                Job::Server { prefixed: false },
                args(&["yonder server", "version"])
            ))
        );
    }

    #[test]
    fn anything_else_is_not_a_job() {
        assert_eq!(job_from_args(&args(&["yonder"])), None);
        assert_eq!(job_from_args(&args(&["yonder", "src/main.rs"])), None);
        assert_eq!(job_from_args(&args(&["yonder", "proxy"])), None);
        assert_eq!(
            job_from_args(&args(&["yonder", "--askpass", "/tmp/s"])),
            None
        );
    }
}
