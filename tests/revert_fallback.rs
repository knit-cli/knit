mod common;

use common::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const BUNDLE: &str = "revert-fallback";

/// Forgejo has no native revert, so `revert landed --apply` must take the
/// git fallback against this fake API.
#[derive(Default)]
struct FakeForgejo {
    remotes: BTreeMap<String, PathBuf>,
    /// Clones the fake merges reviews in, as the forge would on its side.
    collaborators: BTreeMap<String, PathBuf>,
    pulls: BTreeMap<(String, u64), Value>,
    created: Vec<Value>,
}

fn spawn_forgejo(state: Arc<Mutex<FakeForgejo>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = state.clone();
            std::thread::spawn(move || {
                let _ = handle(&mut stream, &state);
            });
        }
    });
    base
}

fn handle(stream: &mut TcpStream, state: &Mutex<FakeForgejo>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let path = target.trim_start_matches('/');
    let path = path.split('?').next().unwrap_or(path);
    let segments = path.split('/').collect::<Vec<_>>();
    let mut state = state.lock().unwrap();
    let (status, response) = match (method.as_str(), segments.as_slice()) {
        ("GET", ["repos", "acme", repo, "pulls", number]) => {
            match state
                .pulls
                .get(&(repo.to_string(), number.parse().unwrap_or(0)))
            {
                Some(pull) => (200, pull.to_string()),
                None => (404, r#"{"message":"not found"}"#.to_string()),
            }
        }
        ("GET", ["repos", "acme", _, "pulls", _, "reviews"]) => (200, "[]".to_string()),
        ("GET", ["repos", "acme", _, "commits", _, "status"]) => {
            (200, r#"{"statuses":[]}"#.to_string())
        }
        ("POST", ["repos", "acme", repo, "pulls"]) => {
            let payload: Value = serde_json::from_slice(&body).unwrap();
            let head = payload["head"].as_str().unwrap().to_string();
            let remote = state.remotes[*repo].clone();
            let sha = git(&remote, ["rev-parse", &format!("refs/heads/{head}")]);
            let number = 10 + state.created.len() as u64;
            let pull = json!({
                "number": number,
                "html_url": format!("https://codeberg.org/acme/{repo}/pulls/{number}"),
                "state": "open",
                "title": payload["title"],
                "body": payload["body"],
                "draft": false,
                "merged": false,
                "mergeable": true,
                "head": {"ref": head, "sha": sha.trim()},
                "base": {"ref": payload["base"], "sha": "base"}
            });
            state
                .created
                .push(json!({"repo": repo, "payload": payload}));
            state.pulls.insert((repo.to_string(), number), pull.clone());
            (201, pull.to_string())
        }
        ("POST", ["repos", "acme", repo, "pulls", number, "merge"]) => {
            let key = (repo.to_string(), number.parse().unwrap_or(0));
            let head = state.pulls[&key]["head"]["ref"]
                .as_str()
                .unwrap()
                .to_string();
            let clone = state.collaborators[*repo].clone();
            git(&clone, ["fetch", "-q", "origin", "main"]);
            git(&clone, ["checkout", "-q", "main"]);
            git(&clone, ["reset", "-q", "--hard", "origin/main"]);
            git(&clone, ["fetch", "-q", "origin", &head]);
            git(
                &clone,
                ["merge", "-q", "--no-ff", "--no-edit", "FETCH_HEAD"],
            );
            git(&clone, ["push", "-q", "origin", "main"]);
            let merge = git(&clone, ["rev-parse", "HEAD"]).trim().to_string();
            let pull = state.pulls.get_mut(&key).unwrap();
            pull["merged"] = json!(true);
            pull["state"] = json!("closed");
            pull["merge_commit_sha"] = json!(merge);
            (200, "{}".to_string())
        }
        _ => (404, format!(r#"{{"unexpected":"{method} {path}"}}"#)),
    };
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
        response.len()
    )?;
    stream.flush()
}

struct Landed {
    root: PathBuf,
    workspace: PathBuf,
    fake_bin: PathBuf,
    fake_dir: PathBuf,
    env: Vec<(String, String)>,
    state: Arc<Mutex<FakeForgejo>>,
    merges: BTreeMap<String, String>,
}

impl Landed {
    fn knit(&self, args: &[&str]) -> String {
        let env = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect::<Vec<_>>();
        knit_with_fake_forge(&self.workspace, args, &self.fake_bin, &self.fake_dir, &env)
    }

    fn knit_fails(&self, args: &[&str]) -> String {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_knit"));
        command
            .args(args)
            .current_dir(&self.workspace)
            .env("KNIT_HOME", isolated_knit_home())
            .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
            .env_remove("KNIT_BUNDLE")
            .env_remove("KNIT_SESSION");
        private_temp_env(&self.workspace, &mut command);
        scrub_ambient_forge_env(&mut command);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success(), "command unexpectedly succeeded");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    fn set_env(&mut self, key: &str, value: &str) {
        self.env.push((key.to_string(), value.to_string()));
    }

    fn bundle(&self) -> Value {
        serde_json::from_str(
            &fs::read_to_string(
                self.workspace
                    .join(format!(".knit/bundles/{BUNDLE}.bundle.json")),
            )
            .unwrap(),
        )
        .unwrap()
    }
}

/// A two-repo bundle whose reviews were merged on the forge — backend with a
/// merge commit, frontend squashed — and then landed and archived.
fn landed_on_forgejo() -> Landed {
    landed_on_forgejo_with(&["backend", "frontend"])
}

/// Like [`landed_on_forgejo`] over `repos`; only `frontend` is squashed.
fn landed_on_forgejo_with(repos: &[&str]) -> Landed {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut remotes = BTreeMap::new();
    let mut clones = BTreeMap::new();
    for repo in repos {
        let (remote, local, collaborator) = init_remote_repo(&root, repo);
        remotes.insert(repo.to_string(), remote);
        clones.insert(repo.to_string(), (local, collaborator));
    }

    knit(&workspace, ["init", "demo"]);
    for (repo, (local, _)) in &clones {
        knit(
            &workspace,
            ["project", "add", repo.as_str(), local.to_str().unwrap()],
        );
    }
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["landing"] = json!({
        "provider": "forgejo",
        "merge": { "repoOrder": ["backend", "frontend"], "method": "merge" }
    });
    fs::write(
        &project_path,
        serde_json::to_string_pretty(&project).unwrap(),
    )
    .unwrap();

    knit(&workspace, ["bundle", "revert fallback"]);
    for repo in clones.keys() {
        let feature = workspace.join(format!(".knit/worktrees/{BUNDLE}/{repo}"));
        append_line(&feature.join("app.txt"), &format!("{repo} feature line"));
    }
    knit(&workspace, ["commit", "--all", "-m", "Feature change"]);

    let mut merges = BTreeMap::new();
    let mut pulls = BTreeMap::new();
    let mut bundle: Value = serde_json::from_str(
        &fs::read_to_string(workspace.join(format!(".knit/bundles/{BUNDLE}.bundle.json"))).unwrap(),
    )
    .unwrap();
    let mut publications = Vec::new();
    let mut urls = Vec::new();
    for (index, (repo, (_, collaborator))) in clones.iter().enumerate() {
        let feature = workspace.join(format!(".knit/worktrees/{BUNDLE}/{repo}"));
        git(
            &feature,
            ["push", "-q", "origin", &format!("knit/{BUNDLE}")],
        );
        let head = git(&feature, ["rev-parse", "HEAD"]).trim().to_string();
        git(collaborator, ["fetch", "-q", "origin"]);
        if repo != "frontend" {
            git(
                collaborator,
                [
                    "merge",
                    "-q",
                    "--no-ff",
                    "-m",
                    "Merge pull request 4",
                    &format!("origin/knit/{BUNDLE}"),
                ],
            );
        } else {
            git(
                collaborator,
                ["merge", "-q", "--squash", &format!("origin/knit/{BUNDLE}")],
            );
            git(collaborator, ["commit", "-q", "-m", "Feature change (#5)"]);
        }
        git(collaborator, ["push", "-q", "origin", "main"]);
        let merge = git(collaborator, ["rev-parse", "HEAD"]).trim().to_string();
        let number = 4 + index as u64;
        let url = format!("https://codeberg.org/acme/{repo}/pulls/{number}");
        pulls.insert(
            (repo.clone(), number),
            json!({
                "number": number,
                "html_url": url,
                "state": "closed",
                "title": format!("revert fallback ({repo})"),
                "merged": true,
                "merge_commit_sha": merge,
                "head": {"ref": format!("knit/{BUNDLE}"), "sha": head},
                "base": {"ref": "main", "sha": merge}
            }),
        );
        merges.insert(repo.clone(), merge);
        bundle["repos"][index]["remote"] = json!(format!("https://codeberg.org/acme/{repo}.git"));
        publications.push(json!({
            "repoId": repo,
            "provider": "forgejo",
            "kind": "pull_request",
            "number": number,
            "url": url,
            "baseBranch": "main",
            "headBranch": format!("knit/{BUNDLE}"),
            "state": "MERGED",
            "updatedAt": "2026-10-11T00:00:00.000Z"
        }));
        urls.push(url);
    }
    bundle["publications"] = json!(publications);
    bundle["nodes"].as_array_mut().unwrap().push(json!({
        "id": "land-fixture",
        "type": "feature.landed",
        "createdAt": "2026-10-11T00:00:01.000Z",
        "repoIds": repos,
        "planId": format!("land-{BUNDLE}"),
        "runId": "run-fixture",
        "provider": "forgejo",
        "publicationUrls": urls,
        "landing": {"terminal": true}
    }));
    fs::write(
        workspace.join(format!(".knit/bundles/{BUNDLE}.bundle.json")),
        serde_json::to_string_pretty(&bundle).unwrap(),
    )
    .unwrap();
    knit(
        &workspace,
        ["bundle", "archive", BUNDLE, "--reason", "landed"],
    );

    let state = Arc::new(Mutex::new(FakeForgejo {
        remotes: remotes.clone(),
        collaborators: clones
            .iter()
            .map(|(repo, (_, collaborator))| (repo.clone(), collaborator.clone()))
            .collect(),
        pulls,
        created: Vec::new(),
    }));
    let base = spawn_forgejo(state.clone());
    let mut env = vec![
        ("KNIT_FORGEJO_API_BASE".to_string(), base),
        ("KNIT_FORGEJO_TOKEN".to_string(), "test-token".to_string()),
        ("GIT_CONFIG_COUNT".to_string(), remotes.len().to_string()),
    ];
    for (index, (repo, remote)) in remotes.iter().enumerate() {
        env.push((
            format!("GIT_CONFIG_KEY_{index}"),
            format!("url.{}.insteadOf", remote.display()),
        ));
        env.push((
            format!("GIT_CONFIG_VALUE_{index}"),
            format!("https://codeberg.org/acme/{repo}.git"),
        ));
    }
    let fake_bin = root.join("fake-bin");
    let fake_dir = root.join("fake-forge");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&fake_dir).unwrap();
    Landed {
        root,
        workspace,
        fake_bin,
        fake_dir,
        env,
        state,
        merges,
    }
}

fn remote_path(landed: &Landed, repo: &str) -> PathBuf {
    landed.state.lock().unwrap().remotes[repo].clone()
}

#[test]
fn forge_without_native_revert_gets_git_revert_reviews_landed_in_reverse() {
    let landed = landed_on_forgejo();
    assert_eq!(landed.bundle()["state"], "archived");
    assert!(!landed
        .workspace
        .join(format!(".knit/worktrees/{BUNDLE}"))
        .exists());

    let applied = landed.knit(&["--bundle", BUNDLE, "revert", "landed", "--apply"]);
    assert!(applied.contains("Recorded PR revert group"), "{applied}");
    assert!(applied.contains("Reopened bundle"), "{applied}");

    let created = landed.state.lock().unwrap().created.clone();
    assert_eq!(created.len(), 2, "{created:?}");
    for entry in &created {
        let repo = entry["repo"].as_str().unwrap();
        let payload = &entry["payload"];
        let number = if repo == "backend" { 4 } else { 5 };
        let branch = payload["head"].as_str().unwrap();
        assert!(
            branch.starts_with(&format!("knit/revert-{number}-")),
            "{branch}"
        );
        assert_eq!(payload["base"], "main");
        assert_eq!(payload["title"], format!("Revert revert fallback ({repo})"));
        assert!(payload["body"]
            .as_str()
            .unwrap()
            .contains(&format!("Knit-Bundle: {BUNDLE}")));

        // The branch is the merged base plus one revert that undoes the
        // feature line.
        let remote = remote_path(&landed, repo);
        let tip = git(&remote, ["rev-parse", &format!("refs/heads/{branch}")]);
        let parent = git(&remote, ["rev-parse", &format!("{}^", tip.trim())]);
        assert_eq!(parent.trim(), landed.merges[repo]);
        let content = git(&remote, ["show", &format!("{}:app.txt", tip.trim())]);
        assert!(
            !content.contains(&format!("{repo} feature line")),
            "{content}"
        );
        let message = git(&remote, ["log", "-1", "--format=%s", tip.trim()]);
        assert!(message.starts_with("Revert"), "{message}");
    }

    let reopened = landed.bundle();
    assert_eq!(reopened["state"], "open");
    for publication in reopened["publications"].as_array().unwrap() {
        assert_eq!(publication["state"], "OPEN");
        assert!(publication["headBranch"]
            .as_str()
            .unwrap()
            .starts_with("knit/revert-"));
    }
    assert!(!landed.workspace.join(".knit/revert-clones").exists());

    landed.knit(&["--bundle", BUNDLE, "land"]);
    let plan: Value = serde_json::from_str(
        &fs::read_to_string(
            landed
                .workspace
                .join(format!(".knit/land-plans/{BUNDLE}.land.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let merges = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["type"] == "merge_pr")
        .map(|step| step["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(merges, vec!["merge-frontend", "merge-backend"]);

    fs::remove_dir_all(&landed.root).unwrap();
}

#[test]
fn conflicting_git_revert_fails_without_pushing_and_names_the_review() {
    let landed = landed_on_forgejo();
    let collaborator = landed.root.join("backend-collaborator");
    fs::write(collaborator.join("app.txt"), "rewritten after the merge\n").unwrap();
    git(&collaborator, ["commit", "-q", "-am", "Rewrite app"]);
    git(&collaborator, ["push", "-q", "origin", "main"]);

    let error = landed.knit_fails(&["--bundle", BUNDLE, "revert", "landed", "--apply"]);
    assert!(error.contains("backend"), "{error}");
    assert!(error.contains("PR #4"), "{error}");
    assert!(error.contains("conflicts"), "{error}");
    assert!(error.contains("by hand"), "{error}");
    let branches = git(
        &remote_path(&landed, "backend"),
        ["branch", "--list", "knit/revert-4-*"],
    );
    assert!(branches.trim().is_empty(), "{branches}");
    assert!(!landed.workspace.join(".knit/revert-clones").exists());

    fs::remove_dir_all(&landed.root).unwrap();
}

const FOUR: [&str; 4] = ["api", "backend", "frontend", "worker"];

#[test]
fn revert_reviews_open_in_parallel_and_record_in_repo_order() {
    let mut landed = landed_on_forgejo_with(&FOUR);
    landed.set_env("KNIT_FORGE_JOBS", "4");
    // A later rewrite of backend's file makes its revert conflict; the other
    // three still get their reviews.
    let collaborator = landed.root.join("backend-collaborator");
    fs::write(collaborator.join("app.txt"), "rewritten after the merge\n").unwrap();
    git(&collaborator, ["commit", "-q", "-am", "Rewrite app"]);
    git(&collaborator, ["push", "-q", "origin", "main"]);

    let error = landed.knit_fails(&["--bundle", BUNDLE, "revert", "landed", "--apply"]);
    assert!(
        error.contains("PR revert completed with failures:"),
        "{error}"
    );
    assert!(error.contains("backend: "), "{error}");
    assert!(error.contains("conflicts"), "{error}");
    assert!(error.contains("Opened 3 revert reviews in "), "{error}");
    let lines = ["api", "frontend", "worker"]
        .iter()
        .map(|repo| {
            error
                .find(&format!(
                    "{repo}: revert PR https://codeberg.org/acme/{repo}/pulls/"
                ))
                .unwrap_or_else(|| panic!("{repo} missing from {error}"))
        })
        .collect::<Vec<_>>();
    assert!(lines.windows(2).all(|pair| pair[0] < pair[1]), "{error}");

    let created = landed.state.lock().unwrap().created.clone();
    let mut created_repos = created
        .iter()
        .map(|entry| entry["repo"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    created_repos.sort();
    assert_eq!(created_repos, vec!["api", "frontend", "worker"]);

    let bundle = landed.bundle();
    let publications = bundle["publications"].as_array().unwrap();
    assert_eq!(
        publications
            .iter()
            .map(|publication| publication["repoId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        FOUR
    );
    for publication in publications {
        let reverted = publication["repoId"] != "backend";
        assert_eq!(
            publication["number"].as_u64().unwrap() >= 10,
            reverted,
            "{publication}"
        );
    }
    assert!(!bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["type"] == "pr.revert"));

    fs::remove_dir_all(&landed.root).unwrap();
}

/// Log every `git clone` and `git fetch` Knit runs, with the directory it
/// runs in, then hand the command to the real git. `remote get-url origin`
/// answers with the configured URL, as it would for a real codeberg.org
/// remote; the environment's `insteadOf` only redirects transport.
#[cfg(unix)]
fn install_clone_counter(landed: &Landed) {
    use std::os::unix::fs::PermissionsExt;
    let real = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let shim = landed.fake_bin.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *'remote get-url origin'*) exec '{0}' config --get remote.origin.url ;;\nesac\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    clone) printf '%s\\n' \"$*\" >> \"$FORGE_FAKE_DIR/git-clones.log\"; break ;;\n    fetch) printf '%s :: %s\\n' \"$PWD\" \"$*\" >> \"$FORGE_FAKE_DIR/git-fetches.log\"; break ;;\n  esac\ndone\nexec '{0}' \"$@\"\n",
            real.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn land_flag_reuses_revert_clones_and_verifies_in_repo_order() {
    let mut landed = landed_on_forgejo_with(&FOUR);
    landed.set_env("KNIT_FORGE_JOBS", "4");
    landed.set_env("KNIT_GIT_JOBS", "4");
    // Landing resolves the forge repository from each source checkout's origin.
    for repo in FOUR {
        git(
            &landed.root.join(repo),
            [
                "remote",
                "set-url",
                "origin",
                &format!("https://codeberg.org/acme/{repo}.git"),
            ],
        );
    }
    install_clone_counter(&landed);

    let undone = landed.knit(&["--bundle", BUNDLE, "revert", "landed", "--apply", "--land"]);
    assert!(undone.contains("Opened 4 revert reviews in "), "{undone}");
    assert!(undone.contains("Merged 4 revert reviews in "), "{undone}");
    assert!(undone.contains("Verified 4 repositories in "), "{undone}");
    assert!(
        undone.contains("Revert verified: revert fallback is undone in all 4 repositories."),
        "{undone}"
    );
    let verified = FOUR
        .iter()
        .map(|repo| {
            undone
                .find(&format!("{repo}: revert verified on main"))
                .unwrap_or_else(|| panic!("{repo} missing from {undone}"))
        })
        .collect::<Vec<_>>();
    assert!(
        verified.windows(2).all(|pair| pair[0] < pair[1]),
        "{undone}"
    );

    let clones = fs::read_to_string(landed.fake_dir.join("git-clones.log")).unwrap();
    assert_eq!(clones.lines().count(), 4, "{clones}");
    assert!(
        clones
            .lines()
            .all(|line| line.contains(".knit/revert-clones")),
        "{clones}"
    );
    // Verification fetched the updated base into each revert clone rather
    // than into a new clone or the source checkout.
    let fetches = fs::read_to_string(landed.fake_dir.join("git-fetches.log")).unwrap();
    for repo in FOUR {
        let base_fetch = format!("https://codeberg.org/acme/{repo}.git main");
        assert!(
            fetches
                .lines()
                .any(|line| line.contains(".knit/revert-clones") && line.ends_with(&base_fetch)),
            "{repo}: {fetches}"
        );
        assert!(
            !fetches.lines().any(|line| {
                line.starts_with(landed.root.join(repo).to_str().unwrap())
                    && line.ends_with(&base_fetch)
            }),
            "{repo}: {fetches}"
        );
    }
    assert!(!landed.workspace.join(".knit/revert-clones").exists());

    let bundle = landed.bundle();
    assert_eq!(bundle["state"], "archived");
    let revert = bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["type"] == "pr.revert")
        .unwrap();
    assert_eq!(revert["repoIds"], json!(FOUR));
    assert_eq!(
        revert["revertedReviews"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        FOUR.iter().collect::<Vec<_>>()
    );

    fs::remove_dir_all(&landed.root).unwrap();
}
