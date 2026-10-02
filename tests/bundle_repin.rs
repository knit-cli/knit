mod common;

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const REPOS: [&str; 2] = ["consumer", "library"];

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    rustup_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = common::unique_temp_dir();
        let home = root.join("home");
        let workspace = root.join("workspace");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let rustup_home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".rustup"));
        fs::write(
            home.join("gitconfig"),
            "[user]\nname = Knit Test\nemail = knit@example.test\n[commit]\ngpgsign = false\n",
        )
        .unwrap();
        Self {
            root,
            home,
            workspace,
            rustup_home,
        }
    }

    fn command(&self, program: &str, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command.current_dir(cwd).args(args);
        for key in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "ELECTRON_RUN_AS_NODE",
            "KNIT_SESSION",
            "KNIT_BUNDLE",
            "IVALDI_COMMIT_SIGNATURE",
            "CARGO_NET_OFFLINE",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
        ] {
            command.env_remove(key);
        }
        command
            .env("HOME", &self.home)
            .env("KNIT_HOME", self.home.join("knit"))
            .env("GIT_CONFIG_GLOBAL", self.home.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("RUSTUP_HOME", &self.rustup_home)
            .env("CARGO_HOME", self.root.join("setup-cargo-home"))
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env("CARGO_NET_GIT_FETCH_WITH_CLI", "true");
        command
    }

    fn run(&self, mut command: Command) -> String {
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("{command:?}: {error}"));
        assert!(
            output.status.success(),
            "{command:?}\nexit: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        self.run(self.command("git", cwd, args)).trim().to_owned()
    }

    fn knit(&self, args: &[&str]) -> String {
        self.run(self.command(env!("CARGO_BIN_EXE_knit"), &self.workspace, args))
    }

    fn checkout(&self, id: &str) -> PathBuf {
        self.workspace.join(".knit/worktrees/rewrite").join(id)
    }

    fn head(&self, id: &str) -> String {
        self.git(&self.checkout(id), &["rev-parse", "HEAD"])
    }

    fn bundle(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.workspace.join(".knit/bundles/rewrite.bundle.json")).unwrap(),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn squash_repins_consumer_lock_to_final_library_commit_and_builds_locked() {
    let f = Fixture::new();
    f.knit(&["init", "demo"]);
    // Consumer sorts first: dependency order must win over repository order.
    for id in REPOS {
        let source = f.root.join(id);
        let remote = f.root.join(format!("{id}.git"));
        fs::create_dir_all(&source).unwrap();
        f.git(&source, &["init", "-b", "main"]);
        fs::write(source.join("README.md"), format!("{id} fixture\n")).unwrap();
        f.git(&source, &["add", "."]);
        f.git(&source, &["commit", "-m", "Initial repository"]);
        f.git(
            &f.root,
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        let remote_url = url::Url::from_file_path(&remote).unwrap().to_string();
        f.git(&source, &["remote", "add", "origin", &remote_url]);
        f.knit(&["project", "add", id, source.to_str().unwrap()]);
    }
    f.knit(&["bundle", "rewrite"]);
    let library = f.checkout("library");
    let consumer = f.checkout("consumer");
    let remote_url = url::Url::from_file_path(f.root.join("library.git"))
        .unwrap()
        .to_string();
    for repo in [&library, &consumer] {
        fs::create_dir_all(repo.join("src")).unwrap();
    }
    fs::write(
        library.join("Cargo.toml"),
        "[package]\nname = \"fixture-library\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(library.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    fs::write(consumer.join("Cargo.toml"), format!("[package]\nname = \"fixture-consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nfixture-library = {{ git = {remote_url:?}, branch = \"knit/rewrite\" }}\n")).unwrap();
    fs::write(
        consumer.join("src/main.rs"),
        "fn main() { println!(\"{}\", fixture_library::value()); }\n",
    )
    .unwrap();
    // A separate tracked lockfile must remain byte-for-byte intact.
    fs::create_dir_all(consumer.join("unrelated")).unwrap();
    let unrelated = b"# unrelated lockfile; preserve formatting\nversion = 3\n\n[[package]]\nname = \"unrelated\"\nversion = \"0.1.0\"\n";
    fs::write(consumer.join("unrelated/Cargo.lock"), unrelated).unwrap();
    f.knit(&["commit", "--all", "-m", "Add Cargo fixtures"]);
    fs::write(library.join("src/lib.rs"), "pub fn value() -> u32 { 2 }\n").unwrap();
    fs::write(
        consumer.join("src/main.rs"),
        "fn main() { assert_eq!(fixture_library::value(), 2); }\n",
    )
    .unwrap();
    f.knit(&["commit", "--all", "-m", "Update both crates"]);
    let old_library = f.head("library");
    f.git(
        &library,
        &["push", "origin", "HEAD:refs/heads/knit/rewrite"],
    );
    f.run(f.command("cargo", &consumer, &["generate-lockfile"]));
    let before_lock = fs::read_to_string(consumer.join("Cargo.lock")).unwrap();
    let old_source = format!("git+{remote_url}?branch=knit%2Frewrite#{old_library}");
    assert!(before_lock.contains(&old_source), "{before_lock}");
    f.knit(&["commit", "--all", "-m", "Pin library dependency"]);
    let before = f.bundle();
    for repo in before["repos"].as_array().unwrap() {
        let id = repo["id"].as_str().unwrap();
        let range = format!("{}..HEAD", repo["baseSha"].as_str().unwrap());
        let count = f.git(&f.checkout(id), &["rev-list", "--count", &range]);
        assert!(count.parse::<usize>().unwrap() >= 2);
        println!("before squash: {id} head={} commits={count}", f.head(id));
    }

    println!("squash output:\n{}", f.knit(&["squash", "-m", "Combined"]));
    let after = f.bundle();
    let groups = after["commitGroups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{groups:#?}");
    assert_eq!(groups[0]["message"], "Combined");
    let commits = groups[0]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2);
    for repo in after["repos"].as_array().unwrap() {
        let id = repo["id"].as_str().unwrap();
        let head = f.head(id);
        assert_eq!(repo["headSha"], head);
        assert_eq!(
            commits
                .iter()
                .find(|commit| commit["repoId"] == id)
                .unwrap()["sha"],
            head
        );
        let range = format!("{}..HEAD", repo["baseSha"].as_str().unwrap());
        let count = f.git(&f.checkout(id), &["rev-list", "--count", &range]);
        assert_eq!(count, "1");
        assert_eq!(f.git(&f.checkout(id), &["status", "--porcelain"]), "");
        println!(
            "after squash: {id} actual_head={head} ledger_head={} commits={count} groups={}",
            repo["headSha"],
            groups.len()
        );
    }
    let new_library = f.head("library");
    assert_ne!(new_library, old_library);
    let after_lock = fs::read_to_string(consumer.join("Cargo.lock")).unwrap();
    assert!(!after_lock.contains(&old_library), "{after_lock}");
    assert_eq!(
        after_lock,
        before_lock.replace(&old_library, &new_library),
        "only the library source SHA may change"
    );
    assert_eq!(
        fs::read(consumer.join("unrelated/Cargo.lock")).unwrap(),
        unrelated
    );
    println!("Cargo.lock repinned: {old_library} -> {new_library}; unrelated bytes unchanged");
    f.git(
        &library,
        &["push", "--force", "origin", "HEAD:refs/heads/knit/rewrite"],
    );

    let fresh_cargo_home = f.root.join("fresh-cargo-home");
    assert!(!fresh_cargo_home.exists());
    for args in [
        vec!["build", "--locked"],
        vec!["build", "--offline", "--locked"],
    ] {
        let mut command = f.command("cargo", &consumer, &args);
        command.env("CARGO_HOME", &fresh_cargo_home);
        // Separate target dirs force compilation in both modes, not just cache hits.
        command.env(
            "CARGO_TARGET_DIR",
            f.root.join(if args.contains(&"--offline") {
                "offline-target"
            } else {
                "locked-target"
            }),
        );
        let output = command.output().unwrap();
        println!(
            "cargo {}: exit={}\n{}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "cargo {} failed", args.join(" "));
        assert_eq!(
            fs::read_to_string(consumer.join("Cargo.lock")).unwrap(),
            after_lock
        );
    }
}
