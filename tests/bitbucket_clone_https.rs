//! Real Git and TLS exercise the saved credential, without native-auth fallback.
#[cfg(unix)]
#[test]
fn typed_bitbucket_defaults_authenticate_full_clone_over_https() {
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/bitbucket_clone_https.py"
        ))
        .arg(env!("CARGO_BIN_EXE_knit"))
        .output()
        .expect("run real HTTPS Git fixture");
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
