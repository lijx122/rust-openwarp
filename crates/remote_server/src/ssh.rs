use std::path::Path;
use std::process::Output;
use std::time::Duration;

use anyhow::{anyhow, Result};
use command::r#async::Command;
use warpui::r#async::FutureExt as _;

/// Timeout for `ssh -O exit`. The command only talks to the local
/// ControlMaster over a Unix socket, so it should return almost
/// immediately; if it doesn't, we'd rather give up than block
/// teardown.
const STOP_CONTROL_MASTER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default)]
pub struct SshKeepaliveOptions {
    pub server_alive_interval_secs: Option<u64>,
    pub server_alive_count_max: Option<u32>,
    pub tcp_keepalive_enabled: Option<bool>,
}

/// Builds the common SSH argument list for multiplexed connections through
/// an existing ControlMaster socket.
pub fn ssh_args(socket_path: &Path) -> Vec<String> {
    ssh_args_with_options(socket_path, &SshKeepaliveOptions::default())
}

pub fn ssh_args_with_options(socket_path: &Path, options: &SshKeepaliveOptions) -> Vec<String> {
    let mut args = vec![
        "-q".to_string(),
        "-o".to_string(),
        "PasswordAuthentication=no".to_string(),
        "-o".to_string(),
        "ForwardX11=no".to_string(),
    ];

    if let Some(interval) = options.server_alive_interval_secs {
        args.push("-o".to_string());
        args.push(format!("ServerAliveInterval={interval}"));
    }
    if let Some(max) = options.server_alive_count_max {
        args.push("-o".to_string());
        args.push(format!("ServerAliveCountMax={max}"));
    }
    if let Some(enabled) = options.tcp_keepalive_enabled {
        args.push("-o".to_string());
        args.push(format!(
            "TCPKeepAlive={}",
            if enabled { "yes" } else { "no" }
        ));
    }

    args.push("-o".to_string());
    args.push(format!("ControlPath={}", socket_path.display()));
    args.push("placeholder@placeholder".to_string());
    args
}

fn scp_or_sftp_args(socket_path: &Path, options: &SshKeepaliveOptions) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        format!("ControlPath={}", socket_path.display()),
        "-o".to_string(),
        "ControlMaster=no".to_string(),
        "-o".to_string(),
        "PasswordAuthentication=no".to_string(),
        "-o".to_string(),
        "ForwardX11=no".to_string(),
        "-o".to_string(),
        "ConnectTimeout=15".to_string(),
    ];

    if let Some(interval) = options.server_alive_interval_secs {
        args.push("-o".to_string());
        args.push(format!("ServerAliveInterval={interval}"));
    }
    if let Some(max) = options.server_alive_count_max {
        args.push("-o".to_string());
        args.push(format!("ServerAliveCountMax={max}"));
    }
    if let Some(enabled) = options.tcp_keepalive_enabled {
        args.push("-o".to_string());
        args.push(format!(
            "TCPKeepAlive={}",
            if enabled { "yes" } else { "no" }
        ));
    }

    args
}

/// Runs `ssh -O exit -o ControlPath=<socket_path>` to force the local
/// SSH `ControlMaster` managing `socket_path` to exit immediately,
/// without waiting for multiplexed channels to finish draining.
///
/// The user's interactive ssh is spawned with `-o ControlMaster=yes` by
/// `warp_ssh_helper`, so it is both the interactive session and the
/// multiplex master. When the user's remote shell exits, that ssh can
/// hang waiting for half-closed slave channels (e.g. from
/// `ssh ... remote-server-proxy`) to finish cleanup on the remote
/// side. Sending `-O exit` bypasses that wait.
///
/// **Only safe to call once the user's shell has already exited** --
/// this tears down the interactive ssh outright. In practice it is
/// invoked from the `ExitShell` teardown path on the client.
///
/// Fire-and-forget. Errors are logged but not propagated: at teardown
/// time there is nothing useful to do with them.
pub async fn stop_control_master(socket_path: &Path) {
    let args = ssh_args(socket_path);
    let result = async {
        Command::new("ssh")
            .arg("-O")
            .arg("exit")
            .args(&args)
            .kill_on_drop(true)
            .output()
            .await
    }
    .with_timeout(STOP_CONTROL_MASTER_TIMEOUT)
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            log::info!(
                "stop_control_master: `ssh -O exit` succeeded for {}",
                socket_path.display()
            );
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::info!(
                "stop_control_master: `ssh -O exit` for {} exited with {:?}: {stderr}",
                socket_path.display(),
                output.status.code(),
            );
        }
        Ok(Err(e)) => {
            log::info!(
                "stop_control_master: failed to spawn `ssh -O exit` for {}: {e}",
                socket_path.display()
            );
        }
        Err(_) => {
            log::warn!(
                "stop_control_master: `ssh -O exit` for {} timed out after {:?}",
                socket_path.display(),
                STOP_CONTROL_MASTER_TIMEOUT,
            );
        }
    }
}

/// Run a single SSH command through the ControlMaster socket and return a result where:
/// - `Err` for transport-level failures (e.g. couldn't spawn `ssh`, or timeout).
/// - `Ok(output)` callers should check `output.status` to distinguish a successful remote command from a non-zero remote exit.
pub async fn run_ssh_command(
    socket_path: &Path,
    remote_command: &str,
    timeout: Duration,
) -> Result<Output> {
    run_ssh_command_with_options(
        socket_path,
        remote_command,
        timeout,
        &SshKeepaliveOptions::default(),
    )
    .await
}

pub async fn run_ssh_command_with_options(
    socket_path: &Path,
    remote_command: &str,
    timeout: Duration,
    options: &SshKeepaliveOptions,
) -> Result<Output> {
    async {
        Command::new("ssh")
            .args(ssh_args_with_options(socket_path, options))
            .arg(remote_command)
            .kill_on_drop(true)
            .output()
            .await
    }
    .with_timeout(timeout)
    .await
    .map_err(|_| anyhow!("SSH command timed out after {timeout:?}"))?
    .map_err(|e| anyhow!("SSH command failed to execute: {e}"))
}

/// Pipe a script into `bash -s` on the remote host via the ControlMaster
/// socket. Returns a result where:
/// - `Err` for transport-level failures (e.g. couldn't spawn `ssh`, or timeout).
/// - `Ok(output)` callers should check `output.status` to distinguish a successful remote script from a non-zero remote exit.
///
/// We pipe via stdin rather than passing the script as an SSH command-line
/// argument because the install script is multi-line and contains shell
/// constructs (case statements, variable expansions, single/double quotes)
/// that would require complex, fragile escaping if passed as an argument.
/// The `bash -s` + stdin approach avoids all escaping issues and has no
/// argument length limits.
pub async fn run_ssh_script(socket_path: &Path, script: &str, timeout: Duration) -> Result<Output> {
    run_ssh_script_with_options(
        socket_path,
        script,
        timeout,
        &SshKeepaliveOptions::default(),
    )
    .await
}

pub async fn run_ssh_script_with_options(
    socket_path: &Path,
    script: &str,
    timeout: Duration,
    options: &SshKeepaliveOptions,
) -> Result<Output> {
    use std::process::Stdio;

    let mut child = Command::new("ssh")
        .args(ssh_args_with_options(socket_path, options))
        .arg("bash -s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn SSH for script: {e}"))?;

    // Write the script to stdin.
    if let Some(mut stdin) = child.stdin.take() {
        use futures_lite::io::AsyncWriteExt;
        stdin
            .write_all(script.as_bytes())
            .await
            .map_err(|e| anyhow!("Failed to write script to stdin: {e}"))?;
        // Close stdin so the remote bash exits after reading the script.
        drop(stdin);
    }

    child
        .output()
        .with_timeout(timeout)
        .await
        .map_err(|_| anyhow!("Script timed out after {timeout:?}"))?
        .map_err(|e| anyhow!("Script failed: {e}"))
}

/// 通过既有 ControlMaster socket 上传单个文件到远端。
pub async fn scp_upload(
    socket_path: &Path,
    local_path: &Path,
    remote_path: &str,
    timeout: Duration,
) -> Result<()> {
    scp_upload_with_options(
        socket_path,
        local_path,
        remote_path,
        timeout,
        &SshKeepaliveOptions::default(),
    )
    .await
}

pub async fn scp_upload_with_options(
    socket_path: &Path,
    local_path: &Path,
    remote_path: &str,
    timeout: Duration,
    options: &SshKeepaliveOptions,
) -> Result<()> {
    let output = async {
        Command::new("scp")
            .args(scp_or_sftp_args(socket_path, options))
            .arg(local_path.as_os_str())
            .arg(format!("placeholder@placeholder:{remote_path}"))
            .kill_on_drop(true)
            .output()
            .await
    }
    .with_timeout(timeout)
    .await
    .map_err(|_| anyhow!("SCP upload timed out after {timeout:?}"))?
    .map_err(|e| anyhow!("SCP upload failed to execute: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow!("SCP upload failed: {stderr}"))
}

pub async fn scp_download(
    socket_path: &Path,
    remote_path: &str,
    local_path: &Path,
    timeout: Duration,
    options: &SshKeepaliveOptions,
) -> Result<()> {
    let output = async {
        Command::new("scp")
            .args(scp_or_sftp_args(socket_path, options))
            .arg(format!("placeholder@placeholder:{remote_path}"))
            .arg(local_path.as_os_str())
            .kill_on_drop(true)
            .output()
            .await
    }
    .with_timeout(timeout)
    .await
    .map_err(|_| anyhow!("SCP download timed out after {timeout:?}"))?
    .map_err(|e| anyhow!("SCP download failed to execute: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow!("SCP download failed: {stderr}"))
}

pub async fn run_sftp_batch(
    socket_path: &Path,
    batch: &str,
    timeout: Duration,
    options: &SshKeepaliveOptions,
) -> Result<Output> {
    use std::process::Stdio;

    let mut child = Command::new("sftp")
        .args(scp_or_sftp_args(socket_path, options))
        .arg("-b")
        .arg("-")
        .arg("placeholder@placeholder")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn SFTP batch command: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use futures_lite::io::AsyncWriteExt;
        stdin
            .write_all(batch.as_bytes())
            .await
            .map_err(|e| anyhow!("Failed to write SFTP batch to stdin: {e}"))?;
        drop(stdin);
    }

    child
        .output()
        .with_timeout(timeout)
        .await
        .map_err(|_| anyhow!("SFTP batch timed out after {timeout:?}"))?
        .map_err(|e| anyhow!("SFTP batch failed: {e}"))
}
