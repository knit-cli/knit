use anyhow::{Context, Result};
use clap::{ArgMatches, CommandFactory, FromArgMatches};
use knit::commands::failure::{self, ErrorKind};
use knit::{run, Cli};

fn main() -> Result<()> {
    // Windows gives the main thread a 1 MiB stack (Unix mains get 8 MiB).
    // Both clap parsing of a large CLI in debug builds and knit's recursive
    // descents (serde_json parsing, directory walks) can exceed that, so the
    // entire CLI — including argument parsing — runs on a worker thread with
    // an explicit stack size to make behavior uniform across platforms.
    let (json, result) = std::thread::Builder::new()
        .name("knit".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let (json, cli) = parse();
            (json, run(cli))
        })
        .context("failed to spawn knit worker thread")?
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    // Every `--json` command fails the same way: the error envelope on
    // stdout, the `Error:` line on stderr (printed when `main` returns the
    // error), and a non-zero exit. See `knit::commands::failure`.
    if let (true, Err(error)) = (json, &result) {
        failure::report_json_failure(error);
    }
    if result
        .as_ref()
        .is_err_and(|e| e.is::<knit::commands::handoff::report::ProbeFailed>())
    {
        std::process::exit(2);
    }
    result
}

/// Parse the command line, returning whether the chosen command runs with
/// `--json`. A rejected command line that asked for `--json` gets the error
/// envelope too, with kind `usage`, before clap prints its message and exits 2.
fn parse() -> (bool, Cli) {
    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        Err(error) => {
            if error.use_stderr() && raw_args_ask_for_json() {
                // clap's message without its usage block and help hint, on
                // one line: "the following required arguments were not
                // provided: --plan <PLAN>".
                let rendered = error.render().to_string();
                let message = rendered
                    .lines()
                    .take_while(|line| !line.trim().is_empty())
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" ");
                let message = message.trim_start_matches("error: ");
                println!("{}", failure::envelope_for(ErrorKind::Usage, message));
            }
            error.exit()
        }
    };
    let json = wants_json(&matches);
    if json {
        // stdout belongs to the JSON document; `human!` lines (such as the
        // handoff note `knit status` prints) go to stderr instead.
        knit::output::route_human_lines_to_stderr();
    }
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    (json, cli)
}

/// True when the command or any parent command has a `json` flag that is set.
/// Any subcommand that declares a `json: bool` argument is covered.
fn wants_json(matches: &ArgMatches) -> bool {
    let mut current = Some(matches);
    while let Some(matches) = current {
        if matches!(matches.try_get_one::<bool>("json"), Ok(Some(true))) {
            return true;
        }
        current = matches.subcommand().map(|(_, sub)| sub);
    }
    false
}

fn raw_args_ask_for_json() -> bool {
    std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json")
}
