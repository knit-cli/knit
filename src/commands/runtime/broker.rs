//! Terminal and lifecycle forwarding. The worker deliberately never calls this module.
use super::{ActiveBundle, load_active_bundle, trimmed_env};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(crate) fn remote_enabled() -> bool {
    trimmed_env("SVARTAL_RUNTIME_TOKEN").is_some()
        || trimmed_env("T3_PROVIDER_BROKER_SOCKET").is_some()
}
pub(crate) fn forward_active(action: &str, purge: bool, json: bool) -> Result<()> {
    forward(&load_active_bundle()?, action, purge, json)
}
fn environment_id() -> Result<String> {
    if let Some(id) = trimmed_env("KNIT_ENVIRONMENT_ID") {
        return Ok(id);
    }
    let home = trimmed_env("T3CODE_HOME").unwrap_or_else(|| "/var/lib/svartal/.t3code".into());
    let id = std::fs::read_to_string(std::path::Path::new(&home).join("userdata/environment-id"))?;
    let id = id.trim();
    if id.is_empty() {
        bail!("Managed environment identity is empty");
    }
    Ok(id.into())
}
pub(super) fn forward(
    active: &ActiveBundle,
    action: &str,
    purge: bool,
    json_output: bool,
) -> Result<()> {
    let token = trimmed_env("SVARTAL_RUNTIME_TOKEN")
        .context("Managed runtime requires SVARTAL_RUNTIME_TOKEN")?;
    let socket = trimmed_env("T3_PROVIDER_BROKER_SOCKET")
        .unwrap_or_else(|| "/run/svartal/provider-broker.sock".into());
    let body = json!({"environmentId":environment_id()?,"runtimeToken":token,"workspaceRoot":active.root,"bundleId":active.bundle.id,"purge":purge});
    let response = request(&socket, action, &body)?;
    if response["exitCode"].as_i64() != Some(0) {
        bail!(
            "Managed runtime {action} failed: {}",
            response["output"]
                .as_str()
                .unwrap_or("broker refused operation")
        );
    }
    if json_output {
        let status = response
            .get("status")
            .filter(|s| !s.is_null())
            .context("Broker returned no runtime status")?;
        println!("{}", status);
    } else if let Some(output) = response["output"].as_str() {
        print!("{output}");
    }
    Ok(())
}
#[cfg(unix)]
fn request(socket: &str, action: &str, body: &Value) -> Result<Value> {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };
    if !["up", "down", "status"].contains(&action) {
        bail!("Invalid runtime broker action");
    }
    let body = serde_json::to_vec(body)?;
    let mut stream = UnixStream::connect(socket).context("Cannot connect to runtime broker")?;
    // The broker's own budgets plus slack, so its answer arrives before we give
    // up: a terminal that stops waiting first reports failure for a stack the
    // broker goes on to start.
    let budget = match action {
        "up" => 20 * 60 + 30,
        "down" => 5 * 60 + 30,
        _ => 60 + 15,
    };
    stream.set_read_timeout(Some(Duration::from_secs(budget)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    write!(
        stream,
        "POST /v1/runtime/{action} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    let mut response = Vec::new();
    stream.take(16 * 1024 * 1024).read_to_end(&mut response)?;
    decode_response(&response)
}
#[cfg(not(unix))]
fn request(_: &str, _: &str, _: &Value) -> Result<Value> {
    bail!("Managed runtime requires a Unix socket")
}
fn decode_response(response: &[u8]) -> Result<Value> {
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("Invalid broker HTTP response")?;
    let header = std::str::from_utf8(&response[..split])?;
    let code = header
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .context("Missing broker HTTP status")?;
    if code != "200" {
        bail!("Runtime broker returned HTTP {code}");
    }
    let payload = &response[split + 4..];
    let chunked = header.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked")
    });
    if !chunked {
        return Ok(serde_json::from_slice(payload)?);
    }
    let mut rest = payload;
    let mut decoded = Vec::new();
    loop {
        let end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("Invalid chunked broker response")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..end])?
                .split(';')
                .next()
                .unwrap(),
            16,
        )?;
        if size == 0 {
            break;
        }
        rest = &rest[end + 2..];
        if size > rest.len().saturating_sub(2) {
            bail!("Truncated broker response");
        }
        decoded.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    Ok(serde_json::from_slice(&decoded)?)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn broker_http_decodes_plain_chunked_and_rejection() {
        assert_eq!(
            decode_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").unwrap(),
            json!({})
        );
        assert_eq!(
            decode_response(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"
            )
            .unwrap(),
            json!({})
        );
        assert!(
            decode_response(b"HTTP/1.1 403 Forbidden\r\n\r\nsecret")
                .unwrap_err()
                .to_string()
                .contains("403")
        );
    }
    #[cfg(unix)]
    #[test]
    fn broker_forwards_over_unix_socket() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixListener,
        };
        let path =
            std::env::temp_dir().join(format!("knit-broker-test-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut byte = [0];
            while !received.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                received.push(byte[0]);
            }
            let header = String::from_utf8(received).unwrap();
            assert!(header.starts_with("POST /v1/runtime/status HTTP/1.1"));
            let length: usize = header
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["bundleId"],
                "demo"
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"exitCode\":0}")
                .unwrap();
        });
        assert_eq!(
            request(
                path.to_str().unwrap(),
                "status",
                &json!({"bundleId":"demo"})
            )
            .unwrap()["exitCode"],
            0
        );
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
