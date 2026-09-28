//! Native HTTP contract tests: review identity must not follow moving refs.
use knit::providers::{
    bitbucket::Bitbucket, forgejo::Forgejo, github::GitHub, gitlab::GitLab, Forge, PrTarget,
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Environment(Vec<(&'static str, Option<OsString>)>);
impl Environment {
    fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        self.0.push((key, std::env::var_os(key)));
        std::env::set_var(key, value);
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.iter().rev() {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn native_merged_revision_uses_only_confirmed_review_commit() {
    // One test owns the process environment; no production credentials/config.
    let dir = Temporary(std::env::temp_dir().join(format!(
            "knit-merged-revision-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir_all(&dir.0).unwrap();
    let mut env = Environment(vec![]);
    env.set("KNIT_HOME", dir.0.join("config"));
    env.set("KNIT_GITHUB_API_TRANSPORT", "native");
    for key in [
        "GH_TOKEN",
        "KNIT_GITLAB_TOKEN",
        "KNIT_FORGEJO_TOKEN",
        "KNIT_BITBUCKET_ACCESS_TOKEN",
    ] {
        env.set(key, "synthetic-test-token");
    }
    let target = PrTarget::explicit(&dir.0, "example/service");
    let merge = "1111111111111111111111111111111111111111";
    let base_tip = "2222222222222222222222222222222222222222";
    let feature = "3333333333333333333333333333333333333333";
    let providers: Vec<(Box<dyn Forge>, &str, &str, &str)> = vec![
        (
            Box::new(GitHub),
            "KNIT_GITHUB_API_BASE",
            "/repos/example/service/pulls/7",
            "https://github.com/example/service/pull/7",
        ),
        (
            Box::new(GitLab),
            "KNIT_GITLAB_API_BASE",
            "/projects/example%2Fservice/merge_requests/7",
            "https://gitlab.com/example/service/-/merge_requests/7",
        ),
        (
            Box::new(Forgejo),
            "KNIT_FORGEJO_API_BASE",
            "/repos/example/service/pulls/7",
            "https://codeberg.org/example/service/pulls/7",
        ),
        (
            Box::new(Bitbucket),
            "KNIT_BITBUCKET_API_BASE",
            "/repositories/example/service/pullrequests/7",
            "https://bitbucket.org/example/service/pull-requests/7",
        ),
    ];
    for (forge, base_env, endpoint, url) in providers {
        let merged_state = if forge.id() == "gitlab" {
            "merged"
        } else {
            "MERGED"
        };
        let mut review = json!({"merged":true,"state":merged_state,"merge_commit_sha":merge,"merge_commit":{"hash":merge},
            "head":{"sha":feature},"base":{"sha":base_tip},"sha":feature,
            "source":{"commit":{"hash":feature}},"destination":{"commit":{"hash":base_tip}},
            "squash_commit_sha":feature});
        let mut cases: Vec<(u16, Value, Option<&str>)> = vec![(200, review.clone(), Some(merge))];
        // In particular, GitHub's unmerged test-merge SHA is not a landed commit.
        review["merged"] = json!(false);
        review["state"] = json!("open");
        cases.push((200, review.clone(), None));
        review["state"] = json!("closed");
        cases.push((200, review.clone(), None));
        review["merged"] = json!(true);
        review["state"] = json!(merged_state);
        review["merge_commit_sha"] = Value::Null;
        review["merge_commit"] = Value::Null;
        // GitLab squash-only metadata does not prove the final merge revision.
        cases.push((200, review.clone(), None));
        review.as_object_mut().unwrap().remove("merge_commit_sha");
        review.as_object_mut().unwrap().remove("merge_commit");
        cases.push((200, review.clone(), None));
        review["merge_commit_sha"] = json!("");
        review["merge_commit"] = json!({"hash":" "});
        cases.push((200, review.clone(), None));
        // HTTP failures must be errors, not a successful "identity unavailable".
        cases.push((403, json!({"message":"forbidden"}), None));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        env.set(
            base_env,
            format!("http://{}", listener.local_addr().unwrap()),
        );
        listener.set_nonblocking(true).unwrap();
        let replies: Vec<_> = cases.iter().map(|(s, v, _)| (*s, v.to_string())).collect();
        let server = std::thread::spawn(move || {
            for (status, body) in replies {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("expected review request: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.trim_end(), format!("GET {endpoint} HTTP/1.1"));
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        for (status, _, expected) in cases {
            let actual = forge.merged_revision(&target, url);
            if status == 403 {
                assert!(actual.is_err(), "{} hid HTTP failure", forge.id());
            } else {
                assert_eq!(actual.unwrap().as_deref(), expected, "{}", forge.id());
            }
        }
        server.join().unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, r#"#!/bin/sh
[ "$#" = 5 ] && [ "$1" = pr ] && [ "$2" = view ] && [ "$3" = https://github.com/example/service/pull/7 ] && [ "$4" = --json ] && [ "$5" = state,mergeCommit ] || exit 23
cat "$KNIT_TEST_MERGED_RESPONSE"
"#).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        env.set("PATH", std::env::join_paths(paths).unwrap());
        let response = dir.0.join("response.json");
        env.set("KNIT_TEST_MERGED_RESPONSE", &response);
        let target = PrTarget::checkout(&dir.0);
        for (body, expected) in [
            (
                json!({"state":"MERGED","mergeCommit":{"oid":merge},"headRefOid":feature}),
                Some(merge),
            ),
            (json!({"state":"OPEN","mergeCommit":{"oid":merge}}), None),
            (json!({"state":"CLOSED","mergeCommit":{"oid":merge}}), None),
            (
                json!({"state":"MERGED","mergeCommit":null,"headRefOid":feature}),
                None,
            ),
        ] {
            std::fs::write(&response, body.to_string()).unwrap();
            assert_eq!(
                GitHub
                    .merged_revision(&target, "https://github.com/example/service/pull/7")
                    .unwrap()
                    .as_deref(),
                expected
            );
        }
    }
}
