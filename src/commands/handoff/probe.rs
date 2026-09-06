use super::report::{Check, ProbeReport};
use crate::model::ProjectRequirements;
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Commands are argv, never shell strings. Drain pipes while waiting, cap output,
/// disable Git prompts, and terminate hung probes (including SSH children).
pub(crate) fn command_output(program: &str, args: &[String], cwd: &Path) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=10",
        );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("{program} is unavailable"))?;
    fn drain(mut pipe: impl Read) -> String {
        let mut retained = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            let keep = n.min(16384usize.saturating_sub(retained.len()));
            retained.extend_from_slice(&buf[..keep]);
        }
        String::from_utf8_lossy(&retained).into_owned()
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || drain(stdout));
    let err = std::thread::spawn(move || drain(stderr));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(30) {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    match status {
        Some(s) if s.success() => Ok(format!("{out}\n{err}")),
        Some(_) => bail!("{program} probe failed"),
        None => bail!("{program} probe timed out after 30 seconds"),
    }
}

pub(crate) fn version_numbers(value: &str) -> Option<Vec<u64>> {
    value
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(|part| {
            let part = part.trim_matches('.');
            if part.is_empty() {
                return None;
            }
            let numbers: Option<Vec<_>> = part.split('.').map(|s| s.parse::<u64>().ok()).collect();
            numbers
        })
}
fn version_at_least(actual: &str, minimum: &str) -> bool {
    let (Some(mut a), Some(mut b)) = (version_numbers(actual), version_numbers(minimum)) else {
        return false;
    };
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a >= b
}

pub(crate) fn existing_parent(root: &Path) -> Result<PathBuf> {
    let mut parent = root.to_path_buf();
    while !parent.exists() {
        if !parent.pop() {
            bail!("Workspace has no existing parent");
        }
    }
    if !parent.is_dir() {
        bail!("Workspace path is not a directory");
    }
    Ok(parent)
}

pub(crate) fn system_checks(
    root: &Path,
    requirements: &ProjectRequirements,
    measured_mib: Option<u64>,
) -> ProbeReport {
    let mut report = ProbeReport::default();
    let parent = match existing_parent(root) {
        Ok(p) => p,
        Err(e) => {
            report.add(Check::fail("workspace", "Workspace", e.to_string()));
            return report;
        }
    };
    report.add(Check::ok(
        "knit",
        "Knit",
        format!(
            "{}; schema {}",
            env!("CARGO_PKG_VERSION"),
            crate::model::SCHEMA_VERSION
        ),
    ));
    match command_output("git", &["--version".into()], &parent) {
        Ok(v) if version_at_least(&v, "2.20") => {
            report.add(Check::ok("git", "Git worktrees", v.trim()))
        }
        _ => report.add(Check::fail(
            "git",
            "Git worktrees",
            "Git >= 2.20 is required",
        )),
    }
    let writable = (|| -> Result<()> {
        let path = parent.join(format!(
            ".knit-handoff-probe-{}",
            crate::ids::node_id("write")
        ));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        drop(file);
        fs::remove_file(path)?;
        Ok(())
    })();
    report.add(if writable.is_ok() {
        Check::ok(
            "workspace",
            "Writable workspace",
            root.display().to_string(),
        )
    } else {
        Check::fail(
            "workspace",
            "Writable workspace",
            "Cannot create files in workspace parent",
        )
    });
    let platform = crate::model::ambient_origin().platform;
    report.add(
        if requirements.platforms.is_empty() || requirements.platforms.contains(&platform) {
            Check::ok("platform", "Platform", platform)
        } else {
            Check::fail(
                "platform",
                "Platform",
                format!("{platform} is not in the project's supported platforms"),
            )
        },
    );
    let required = requirements
        .disk_mib
        .unwrap_or(0)
        .max(measured_mib.unwrap_or(0));
    match disk_available_mib(&parent) {
        Ok(free) if free >= required => report.add(Check::ok(
            "disk",
            "Free disk",
            format!("{free} MiB available; {required} MiB required"),
        )),
        Ok(free) => report.add(Check::fail(
            "disk",
            "Free disk",
            format!("{free} MiB available; {required} MiB required"),
        )),
        Err(_) => report.add(Check::fail(
            "disk",
            "Free disk",
            "Cannot determine available disk",
        )),
    }
    for tool in &requirements.tools {
        let scope = if tool.for_.as_deref() == Some("runtime") {
            "runtime"
        } else {
            "editing"
        };
        let valid = !tool.name.is_empty()
            && tool
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !tool.name.starts_with('.');
        let outcome = if valid {
            command_output(&tool.name, &["--version".into()], &parent)
        } else {
            Err(anyhow::anyhow!("Invalid tool name"))
        };
        let satisfied = outcome.as_ref().is_ok_and(|v| {
            let name = tool.name.to_ascii_lowercase();
            let version_line = v
                .lines()
                .find(|line| line.trim().to_ascii_lowercase().starts_with(&name))
                .unwrap_or(v);
            tool.min_version
                .as_ref()
                .is_none_or(|min| version_at_least(version_line, min))
        });
        let message = if satisfied {
            "available".to_string()
        } else {
            format!(
                "{}{} is required",
                tool.name,
                tool.min_version
                    .as_ref()
                    .map(|v| format!(" >= {v}"))
                    .unwrap_or_default()
            )
        };
        let mut check = if satisfied {
            Check::ok(&format!("tool:{}", tool.name), &tool.name, message)
        } else if tool.optional || scope == "runtime" {
            Check::warn(&format!("tool:{}", tool.name), &tool.name, message)
        } else {
            Check::fail(&format!("tool:{}", tool.name), &tool.name, message)
        };
        check.scope = scope.into();
        report.add(check);
    }
    if let Some(min) = requirements.memory_mib {
        let memory = memory_available_mib(&parent);
        report.add(match memory {
            Some(n) if n >= min => Check::ok(
                "memory",
                "Available memory",
                format!("{n} MiB available; {min} MiB requested"),
            ),
            Some(n) => Check::warn(
                "memory",
                "Available memory",
                format!("{n} MiB available; {min} MiB requested"),
            ),
            None => Check::warn(
                "memory",
                "Available memory",
                "Cannot verify available memory",
            ),
        });
    }
    for name in &requirements.env {
        report.add(if std::env::var_os(name).is_some_and(|v| !v.is_empty()) {
            Check::ok(&format!("env:{name}"), name, "present")
        } else {
            Check::fail(
                &format!("env:{name}"),
                name,
                "Environment variable is absent",
            )
        });
    }
    if !requirements.agents.is_empty() {
        let mut c = Check::warn(
            "agents",
            "Agent adapters",
            format!(
                "Verify provider availability in the client: {}",
                requirements.agents.join(", ")
            ),
        );
        c.scope = "agent".into();
        report.add(c);
    }
    report
}

#[cfg(unix)]
fn disk_available_mib(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    // statvfs field widths vary across Unix platforms. Widen before multiplying.
    let mib = u128::from(stat.f_bavail) * u128::from(stat.f_frsize) / 1_048_576;
    Ok(mib.try_into().unwrap_or(u64::MAX))
}
#[cfg(not(unix))]
fn disk_available_mib(_path: &Path) -> Result<u64> {
    bail!("Disk probe unsupported on this platform")
}
fn memory_available_mib(cwd: &Path) -> Option<u64> {
    if let Ok(info) = fs::read_to_string("/proc/meminfo") {
        let membership = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        return linux_memory_available_mib(&info, &membership, &mounts, |path| {
            fs::read_to_string(path).ok()
        });
    }
    if std::env::consts::OS == "macos" {
        let info = command_output("vm_stat", &[], cwd).ok()?;
        let page_size = info
            .lines()
            .next()?
            .split("page size of ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        let pages: u64 = info
            .lines()
            .filter(|l| {
                l.starts_with("Pages free:")
                    || l.starts_with("Pages inactive:")
                    || l.starts_with("Pages speculative:")
            })
            .filter_map(|l| {
                l.split(':')
                    .nth(1)?
                    .trim()
                    .trim_end_matches('.')
                    .parse::<u64>()
                    .ok()
            })
            .sum();
        return Some(pages.saturating_mul(page_size) / 1_048_576);
    }
    None
}

/// MemAvailable describes the host even inside a container. Cgroup usage includes
/// this workspace's processes, so its remaining allowance can be much smaller.
fn linux_memory_available_mib(
    meminfo: &str,
    membership: &str,
    mountinfo: &str,
    read: impl Fn(&Path) -> Option<String>,
) -> Option<u64> {
    let host = meminfo
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?
        .saturating_mul(1024);
    let mut available = host;
    for mount in mountinfo.lines() {
        let Some((fields, filesystem)) = mount.split_once(" - ") else {
            continue;
        };
        let filesystem: Vec<_> = filesystem.split_whitespace().collect();
        let v2 = filesystem.first() == Some(&"cgroup2");
        let v1 = filesystem.first() == Some(&"cgroup")
            && filesystem
                .get(2)
                .is_some_and(|options| options.split(',').any(|option| option == "memory"));
        if !v1 && !v2 {
            continue;
        }
        let fields: Vec<_> = fields.split_whitespace().collect();
        let (Some(root), Some(mountpoint)) = (fields.get(3), fields.get(4)) else {
            continue;
        };
        let root = PathBuf::from(unescape_mount_path(root));
        let mountpoint = PathBuf::from(unescape_mount_path(mountpoint));
        for group in membership.lines() {
            let mut parts = group.splitn(3, ':');
            let (Some(_), Some(controllers), Some(group)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if !(v2 && controllers.is_empty()
                || v1
                    && controllers
                        .split(',')
                        .any(|controller| controller == "memory"))
            {
                continue;
            }
            let group = Path::new(group);
            let relative = match group.strip_prefix(&root) {
                Ok(relative) => relative,
                // A cgroup namespace can expose its own root as `/`.
                Err(_) if group == Path::new("/") => Path::new(""),
                Err(_) => continue,
            };
            if relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                continue;
            }
            let mut directory = mountpoint.join(relative);
            let (limit_file, usage_file) = if v2 {
                ("memory.max", "memory.current")
            } else {
                ("memory.limit_in_bytes", "memory.usage_in_bytes")
            };
            // A child may be unlimited while a parent imposes the effective cap.
            loop {
                let remaining = (|| {
                    let limit = read(&directory.join(limit_file))?
                        .trim()
                        .parse::<u64>()
                        .ok()?;
                    // v1 represents "unlimited" with a page-rounded LONG_MAX.
                    if v1 && limit >= (1 << 60) {
                        return None;
                    }
                    // Even without a usable usage counter, never report more
                    // memory than the container's known hard limit.
                    let usage = read(&directory.join(usage_file))
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .unwrap_or(0);
                    Some(limit.saturating_sub(usage))
                })();
                if let Some(remaining) = remaining {
                    available = available.min(remaining);
                }
                if directory == mountpoint || !directory.pop() {
                    break;
                }
            }
        }
    }
    Some(available / 1_048_576)
}

fn unescape_mount_path(path: &str) -> String {
    // mountinfo escapes these four characters, including literal backslashes.
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_versions_compare_components() {
        assert!(version_at_least("cargo 1.90.0", "1.85"));
        assert!(!version_at_least("node v9.12.1", "24"));
        assert!(version_at_least("v24.1.0", "24"));
        assert!(!version_at_least("unknown", "1"));
    }
    #[test]
    fn env_probe_never_includes_values() {
        let requirements: ProjectRequirements =
            serde_json::from_value(serde_json::json!({"env":["PATH"]})).unwrap();
        let report = system_checks(&std::env::temp_dir(), &requirements, None);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains(&std::env::var("PATH").unwrap()));
    }
    fn memory_fixture(membership: &str, mounts: &str, files: &[(&str, &str)]) -> Option<u64> {
        linux_memory_available_mib("MemAvailable:   2930688 kB\n", membership, mounts, |path| {
            files
                .iter()
                .find_map(|(name, value)| (path == Path::new(name)).then(|| value.to_string()))
        })
    }

    const V2_MOUNT: &str = "31 20 0:28 / /sys/fs/cgroup rw - cgroup2 cgroup rw";

    #[test]
    fn memory_respects_container_limit_and_current_usage() {
        assert_eq!(
            memory_fixture(
                "0::/",
                V2_MOUNT,
                &[
                    ("/sys/fs/cgroup/memory.max", "1073741824\n"),
                    ("/sys/fs/cgroup/memory.current", "268435456\n"),
                ]
            ),
            Some(768)
        );
        assert_eq!(
            linux_memory_available_mib("MemAvailable: 131072 kB", "0::/", V2_MOUNT, |path| {
                Some(
                    if path.ends_with("memory.max") {
                        "1073741824"
                    } else {
                        "268435456"
                    }
                    .into(),
                )
            }),
            Some(128)
        );
        assert_eq!(
            memory_fixture(
                "0::/",
                V2_MOUNT,
                &[
                    ("/sys/fs/cgroup/memory.max", "100"),
                    ("/sys/fs/cgroup/memory.current", "101"),
                ]
            ),
            Some(0)
        );
    }

    #[test]
    fn memory_falls_back_for_missing_unlimited_or_malformed_cgroups() {
        for files in [
            vec![],
            vec![
                ("/sys/fs/cgroup/memory.max", "max"),
                ("/sys/fs/cgroup/memory.current", "100"),
            ],
            vec![
                ("/sys/fs/cgroup/memory.max", "invalid"),
                ("/sys/fs/cgroup/memory.current", "100"),
            ],
        ] {
            assert_eq!(memory_fixture("0::/", V2_MOUNT, &files), Some(2862));
        }
        assert_eq!(memory_fixture("", "", &[]), Some(2862));
        for files in [
            vec![("/sys/fs/cgroup/memory.max", "1073741824")],
            vec![
                ("/sys/fs/cgroup/memory.max", "1073741824"),
                ("/sys/fs/cgroup/memory.current", "invalid"),
            ],
        ] {
            assert_eq!(memory_fixture("0::/", V2_MOUNT, &files), Some(1024));
        }
    }

    #[test]
    fn memory_resolves_cgroup_mounts_and_parent_limits() {
        let mounts = "31 20 0:28 /docker/test /custom\\040cgroup rw - cgroup2 cgroup rw";
        assert_eq!(
            memory_fixture(
                "0::/docker/test/child",
                mounts,
                &[
                    ("/custom cgroup/child/memory.max", "max"),
                    ("/custom cgroup/child/memory.current", "0"),
                    ("/custom cgroup/memory.max", "1073741824"),
                    ("/custom cgroup/memory.current", "536870912"),
                ]
            ),
            Some(512)
        );
        assert_eq!(
            memory_fixture(
                "0::/",
                mounts,
                &[
                    ("/custom cgroup/memory.max", "1073741824"),
                    ("/custom cgroup/memory.current", "536870912"),
                ]
            ),
            Some(512)
        );
    }

    #[test]
    fn memory_supports_v1_limits_and_unlimited_sentinel() {
        let mounts = "31 20 0:28 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory";
        let membership = "3:cpu,cpuacct:/other\n4:memory:/workspace";
        let limit = "/sys/fs/cgroup/memory/workspace/memory.limit_in_bytes";
        let usage = "/sys/fs/cgroup/memory/workspace/memory.usage_in_bytes";
        assert_eq!(
            memory_fixture(
                membership,
                mounts,
                &[(limit, "1073741824"), (usage, "268435456")]
            ),
            Some(768)
        );
        assert_eq!(
            memory_fixture(
                membership,
                mounts,
                &[(limit, "9223372036854771712"), (usage, "268435456")]
            ),
            Some(2862)
        );
    }
}
