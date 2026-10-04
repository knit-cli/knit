//! Native landing shims: Rust's Windows process lookup does not find git.cmd.
use std::{fs, path::Path, process::Command};

pub fn reexec_in_isolated_knit_home() -> bool {
    const CHILD_TEST: &str = "KNIT_PROVIDER_FIXTURE_CHILD_TEST";
    let thread = std::thread::current();
    let test_name = thread.name().expect("test harness thread name");
    if std::env::var(CHILD_TEST).as_deref() == Ok(test_name) {
        return false;
    }

    let home = crate::common::unique_temp_dir();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD_TEST, test_name)
        .env("KNIT_HOME", &home);
    crate::common::scrub_ambient_forge_env(&mut command);
    let output = command.output();
    fs::remove_dir_all(home).unwrap();
    let output = output.expect("reexecute provider test with isolated credentials");
    assert!(
        output.status.success(),
        "isolated test {test_name} failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

pub fn install(bin: &Path, forge_cli: Option<&str>) {
    fs::create_dir_all(bin).unwrap();
    // Resolve before the fixture directory is added to PATH; never recurse into the shim.
    let git_name = format!("git{}", std::env::consts::EXE_SUFFIX);
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(&git_name))
        .find(|path| path.is_file())
        .expect("real git executable on PATH")
        .canonicalize()
        .unwrap();
    fs::write(bin.join("fixture-real-git"), real_git.to_str().unwrap()).unwrap();
    let source = bin.join("provider_landing_cli.rs");
    fs::write(&source, include_str!("../fixtures/provider_landing_cli.rs")).unwrap();
    let executable = bin.join(format!("provider-fixture{}", std::env::consts::EXE_SUFFIX));
    let output = Command::new("rustc")
        .args(["--edition=2021", "--crate-name", "provider_fixture"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("compile native provider fixture with the test toolchain");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in std::iter::once("git").chain(forge_cli) {
        fs::copy(
            &executable,
            bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
    }
}
