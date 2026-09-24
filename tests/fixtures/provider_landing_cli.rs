//! Standalone, std-only fixture compiled on the host, including Windows.
//! No shell parsing, MSYS path conversion, or cmd.exe argument expansion.
use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{self, Command},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() {
    if let Err(error) = run() {
        eprintln!("provider landing fixture: {error}");
        process::exit(1);
    }
}

fn real_git(bin: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        fs::read_to_string(bin.join("fixture-real-git"))?.trim(),
    ))
}

fn run() -> Result<()> {
    let exe = env::current_exe()?;
    let bin = exe.parent().ok_or("fixture executable has no parent")?;
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let cli = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("fixture name is not UTF-8")?;
    if cli == "git" {
        let mut git = Command::new(real_git(bin)?);
        if args == ["remote", "get-url", "origin"].map(OsString::from) {
            git.args(["config", "--get", "remote.origin.url"]);
        } else {
            git.args(&args);
        }
        process::exit(git.status()?.code().unwrap_or(1));
    }
    let state = PathBuf::from(env::var_os("FORGE_FAKE_DIR").ok_or("FORGE_FAKE_DIR missing")?);
    let args: Vec<&str> = args
        .iter()
        .map(|s| s.to_str().ok_or("non-UTF-8 forge argument"))
        .collect::<std::result::Result<_, _>>()?;
    let mut calls = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join(format!("{cli}-landing.calls")))?;
    writeln!(calls, "{}", args.join(" "))?;
    let applied = state.join(format!("{cli}-merged")).exists();
    match (cli, args.as_slice()) {
        ("glab", ["mr", "view", "12", "--output", "json"]) => {
            reply(&state, if applied { "merged" } else { "opened" })?;
        }
        ("glab", ["api", "--method", "GET", endpoint]) if endpoint.ends_with("/approvals") => {
            println!("{{\"approved\":true}}");
        }
        ("glab", ["api", "--method", "GET", endpoint]) if endpoint.contains("/pipelines") => {
            println!("[]");
        }
        ("glab", ["mr", "merge", "12", "--yes"]) => merge(bin, &state, cli)?,
        ("tea", ["api", "--method", "GET", "repos/{owner}/{repo}/pulls/4"]) => {
            reply(&state, if applied { "merged" } else { "open" })?;
        }
        (
            "tea",
            ["pr", "list", "--state", "all", "--fields", "index,state,title,head,base,url", "--output", "json"],
        ) => {
            let status = if applied { "merged" } else { "open" };
            println!("[{{\"Index\":4,\"State\":\"{status}\",\"Title\":\"feature\",\"Head\":\"knit/forge-workspace\",\"Base\":\"main\",\"URL\":\"https://codeberg.org/acme/backend/pulls/4\"}}]");
        }
        ("tea", ["pr", "merge", "4", "--style", "merge"]) => merge(bin, &state, cli)?,
        _ => return Err(format!("unexpected {cli} arguments: {args:?}").into()),
    }
    Ok(())
}

fn reply(state: &Path, status: &str) -> Result<()> {
    io::stdout().write_all(&fs::read(state.join(format!("review-{status}.json")))?)?;
    Ok(())
}

fn merge(bin: &Path, state: &Path, cli: &str) -> Result<()> {
    let remote = fs::read_to_string(state.join("remote-path"))?;
    let revision = fs::read_to_string(state.join("merge-sha"))?;
    // The fixture setup created and pushed a real two-parent merge object.
    let status = Command::new(real_git(bin)?)
        .arg("--git-dir")
        .arg(remote.trim())
        .args(["update-ref", "refs/heads/main", revision.trim()])
        .status()?;
    if !status.success() {
        return Err(format!("synthetic merge ref update failed: {status}").into());
    }
    fs::write(state.join(format!("{cli}-merged")), "")?;
    Ok(())
}
