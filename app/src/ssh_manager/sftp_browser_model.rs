use anyhow::{anyhow, Context, Result};
use async_io::Timer;
use command::{r#async::Command, Output, Stdio};
use futures_lite::{future, io::AsyncWriteExt};
use repo_metadata::file_tree_update::{
    DirectoryNodeMetadata, FileNodeMetadata, FileTreeEntryUpdate, RepoMetadataUpdate,
    RepoNodeMetadata,
};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tempfile::{Builder as TempFileBuilder, TempPath};
use warp_core::HostId;
use warp_ssh_manager::{AuthType, KeychainSecretStore, SecretKind, SshSecretStore, SshServerInfo};
use warp_util::standardized_path::StandardizedPath;
use warpui::{Entity, ModelContext, SingletonEntity};

use crate::settings::SshSettings;

const SFTP_CONNECT_TIMEOUT_SECS: u64 = 15;
const SFTP_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct SftpHost {
    server: SshServerInfo,
}

pub struct SftpBrowserModel {
    hosts: HashMap<HostId, SftpHost>,
}

impl SftpBrowserModel {
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self {
            hosts: HashMap::new(),
        }
    }

    pub fn connect(&mut self, server: SshServerInfo, ctx: &mut ModelContext<Self>) {
        let host_id = Self::host_id_for_node(&server.node_id);
        self.hosts.insert(host_id.clone(), SftpHost { server });
        self.load_initial_root(host_id, ctx);
    }

    pub fn is_managed_host(&self, host_id: &HostId) -> bool {
        self.hosts.contains_key(host_id)
    }

    pub fn server_for_host(&self, host_id: &HostId) -> Option<SshServerInfo> {
        self.hosts.get(host_id).map(|host| host.server.clone())
    }

    pub fn run_batch_for_host(
        &self,
        host_id: HostId,
        batch: String,
        ctx: &mut ModelContext<Self>,
        on_complete: impl FnOnce(&mut Self, Result<Output>, &mut ModelContext<Self>) + 'static,
    ) {
        let Some(host) = self.hosts.get(&host_id).cloned() else {
            return;
        };
        let keepalive = SshSettings::as_ref(ctx).keepalive_options();
        ctx.spawn(
            async move { run_sftp_batch(&host.server, &batch, SFTP_OPERATION_TIMEOUT, keepalive).await },
            on_complete,
        );
    }

    pub fn run_ssh_command_for_host(
        &self,
        host_id: HostId,
        remote_command: String,
        ctx: &mut ModelContext<Self>,
        on_complete: impl FnOnce(&mut Self, Result<Output>, &mut ModelContext<Self>) + 'static,
    ) {
        let Some(host) = self.hosts.get(&host_id).cloned() else {
            return;
        };
        let keepalive = SshSettings::as_ref(ctx).keepalive_options();
        ctx.spawn(
            async move {
                run_ssh_command(
                    &host.server,
                    &remote_command,
                    SFTP_OPERATION_TIMEOUT,
                    keepalive,
                )
                .await
            },
            on_complete,
        );
    }

    pub fn load_directory(
        &self,
        host_id: HostId,
        repo_root: StandardizedPath,
        dir_path: StandardizedPath,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(host) = self.hosts.get(&host_id).cloned() else {
            return;
        };
        let keepalive = SshSettings::as_ref(ctx).keepalive_options();
        ctx.spawn(
            async move {
                let output = run_sftp_batch(
                    &host.server,
                    &format!("ls -la {}\n", quote_sftp_path(dir_path.as_str())),
                    SFTP_OPERATION_TIMEOUT,
                    keepalive,
                )
                .await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "sftp ls failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let entries = parse_sftp_ls(&String::from_utf8_lossy(&output.stdout), &dir_path)?;
                Ok((host_id, repo_root, dir_path, entries))
            },
            |_, result, ctx| match result {
                Ok((host_id, repo_root, dir_path, entries)) => {
                    let update = update_for_directory(repo_root, dir_path, entries);
                    repo_metadata::RepoMetadataModel::handle(ctx).update(ctx, |model, ctx| {
                        model.apply_remote_incremental_update(&host_id, &update, ctx);
                    });
                }
                Err(error) => {
                    log::warn!("sftp directory load failed: {error:#}");
                }
            },
        );
    }

    fn load_initial_root(&self, host_id: HostId, ctx: &mut ModelContext<Self>) {
        let Some(host) = self.hosts.get(&host_id).cloned() else {
            return;
        };
        let keepalive = SshSettings::as_ref(ctx).keepalive_options();
        ctx.spawn(
            async move {
                let output = run_sftp_batch(
                    &host.server,
                    "pwd\nls -la .\n",
                    SFTP_OPERATION_TIMEOUT,
                    keepalive,
                )
                .await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "sftp initial list failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                let root = parse_remote_pwd(&stdout).unwrap_or_else(|| "/".to_string());
                let root_path =
                    StandardizedPath::try_with_encoding(&root, typed_path::PathType::Unix)
                        .with_context(|| format!("invalid remote root path: {root}"))?;
                let entries = parse_sftp_ls(&stdout, &root_path)?;
                Ok((host_id, root_path, entries))
            },
            |_, result, ctx| match result {
                Ok((host_id, root_path, entries)) => {
                    let update = update_for_directory(root_path.clone(), root_path, entries);
                    repo_metadata::RepoMetadataModel::handle(ctx).update(ctx, |model, ctx| {
                        model.insert_remote_snapshot(host_id, &update, ctx);
                    });
                }
                Err(error) => {
                    log::warn!("sftp initial root load failed: {error:#}");
                }
            },
        );
    }

    fn host_id_for_node(node_id: &str) -> HostId {
        HostId::new(format!("ssh-manager-sftp:{node_id}"))
    }
}

impl Entity for SftpBrowserModel {
    type Event = ();
}

impl SingletonEntity for SftpBrowserModel {}

#[derive(Clone)]
pub struct SftpEntry {
    path: StandardizedPath,
    is_dir: bool,
}

pub async fn run_sftp_batch_for_server(
    server: SshServerInfo,
    batch: String,
    keepalive: remote_server::ssh::SshKeepaliveOptions,
) -> Result<Output> {
    run_sftp_batch(&server, &batch, SFTP_OPERATION_TIMEOUT, keepalive).await
}

pub async fn run_ssh_command_for_server(
    server: SshServerInfo,
    remote_command: String,
    keepalive: remote_server::ssh::SshKeepaliveOptions,
) -> Result<Output> {
    run_ssh_command(&server, &remote_command, SFTP_OPERATION_TIMEOUT, keepalive).await
}

fn update_for_directory(
    repo_root: StandardizedPath,
    dir_path: StandardizedPath,
    entries: Vec<SftpEntry>,
) -> RepoMetadataUpdate {
    let subtree_metadata = entries
        .into_iter()
        .map(|entry| {
            if entry.is_dir {
                RepoNodeMetadata::Directory(DirectoryNodeMetadata {
                    path: entry.path,
                    ignored: false,
                    loaded: false,
                })
            } else {
                RepoNodeMetadata::File(FileNodeMetadata {
                    extension: Path::new(entry.path.as_str())
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(str::to_owned),
                    path: entry.path,
                    ignored: false,
                })
            }
        })
        .collect();

    RepoMetadataUpdate {
        repo_path: repo_root,
        remove_entries: Vec::new(),
        update_entries: vec![FileTreeEntryUpdate {
            parent_path_to_replace: dir_path,
            subtree_metadata,
        }],
    }
}

async fn run_sftp_batch(
    server: &SshServerInfo,
    batch: &str,
    timeout: Duration,
    keepalive: remote_server::ssh::SshKeepaliveOptions,
) -> Result<Output> {
    let secret = read_secret(server);
    let askpass = secret
        .as_deref()
        .filter(|secret| !secret.is_empty())
        .map(create_askpass_script)
        .transpose()?;

    let mut command = Command::new("sftp");
    command
        .args(sftp_args(server, &keepalive))
        .arg("-b")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(script) = askpass.as_ref() {
        command
            .env("SSH_ASKPASS", script.path.as_os_str())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "openwarp");
    }

    let mut child = command.spawn().context("failed to spawn sftp")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(batch.as_bytes())
            .await
            .context("failed to write sftp batch")?;
    }

    future::race(
        async {
            child
                .output()
                .await
                .context("failed to collect sftp output")
        },
        async move {
            Timer::after(timeout).await;
            Err(anyhow!("sftp batch timed out after {timeout:?}"))
        },
    )
    .await
}

async fn run_ssh_command(
    server: &SshServerInfo,
    remote_command: &str,
    timeout: Duration,
    keepalive: remote_server::ssh::SshKeepaliveOptions,
) -> Result<Output> {
    let secret = read_secret(server);
    let askpass = secret
        .as_deref()
        .filter(|secret| !secret.is_empty())
        .map(create_askpass_script)
        .transpose()?;

    let mut command = Command::new("ssh");
    command
        .args(ssh_args(server, &keepalive))
        .arg(remote_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(script) = askpass.as_ref() {
        command
            .env("SSH_ASKPASS", script.path.as_os_str())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "openwarp");
    }

    future::race(
        async {
            command
                .output()
                .await
                .context("failed to collect ssh output")
        },
        async move {
            Timer::after(timeout).await;
            Err(anyhow!("ssh command timed out after {timeout:?}"))
        },
    )
    .await
}

fn sftp_args(
    server: &SshServerInfo,
    keepalive: &remote_server::ssh::SshKeepaliveOptions,
) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=no".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={SFTP_CONNECT_TIMEOUT_SECS}"),
    ];

    if let Some(interval) = keepalive.server_alive_interval_secs {
        args.push("-o".to_string());
        args.push(format!("ServerAliveInterval={interval}"));
    }
    if let Some(max) = keepalive.server_alive_count_max {
        args.push("-o".to_string());
        args.push(format!("ServerAliveCountMax={max}"));
    }
    if let Some(enabled) = keepalive.tcp_keepalive_enabled {
        args.push("-o".to_string());
        args.push(format!(
            "TCPKeepAlive={}",
            if enabled { "yes" } else { "no" }
        ));
    }

    if server.port != 22 {
        args.push("-P".to_string());
        args.push(server.port.to_string());
    }
    if server.auth_type == AuthType::Key {
        if let Some(path) = server.key_path.as_deref() {
            if !path.is_empty() {
                args.push("-i".to_string());
                args.push(path.to_string());
            }
        }
    }
    args.push(if server.username.is_empty() {
        server.host.clone()
    } else {
        format!("{}@{}", server.username, server.host)
    });
    args
}

fn ssh_args(
    server: &SshServerInfo,
    keepalive: &remote_server::ssh::SshKeepaliveOptions,
) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=no".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={SFTP_CONNECT_TIMEOUT_SECS}"),
    ];

    if let Some(interval) = keepalive.server_alive_interval_secs {
        args.push("-o".to_string());
        args.push(format!("ServerAliveInterval={interval}"));
    }
    if let Some(max) = keepalive.server_alive_count_max {
        args.push("-o".to_string());
        args.push(format!("ServerAliveCountMax={max}"));
    }
    if let Some(enabled) = keepalive.tcp_keepalive_enabled {
        args.push("-o".to_string());
        args.push(format!(
            "TCPKeepAlive={}",
            if enabled { "yes" } else { "no" }
        ));
    }

    if server.port != 22 {
        args.push("-p".to_string());
        args.push(server.port.to_string());
    }
    if server.auth_type == AuthType::Key {
        if let Some(path) = server.key_path.as_deref() {
            if !path.is_empty() {
                args.push("-i".to_string());
                args.push(path.to_string());
            }
        }
    }
    args.push(if server.username.is_empty() {
        server.host.clone()
    } else {
        format!("{}@{}", server.username, server.host)
    });
    args
}

fn read_secret(server: &SshServerInfo) -> Option<String> {
    let kind = match server.auth_type {
        AuthType::Password => SecretKind::Password,
        AuthType::Key => SecretKind::Passphrase,
    };
    KeychainSecretStore
        .get(&server.node_id, kind)
        .ok()
        .flatten()
        .map(|secret| secret.to_string())
}

struct AskpassScript {
    path: TempPath,
}

fn create_askpass_script(secret: &str) -> Result<AskpassScript> {
    #[cfg(windows)]
    let mut file = TempFileBuilder::new().suffix(".cmd").tempfile()?;
    #[cfg(not(windows))]
    let mut file = TempFileBuilder::new().suffix(".sh").tempfile()?;

    #[cfg(windows)]
    {
        use std::io::Write;
        writeln!(file, "@echo off")?;
        writeln!(file, "echo {secret}")?;
    }
    #[cfg(not(windows))]
    {
        use std::io::Write;
        writeln!(file, "#!/bin/sh")?;
        writeln!(file, "printf '%s\\n' '{}'", secret.replace('\'', "'\"'\"'"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = file.as_file().metadata()?.permissions();
            permissions.set_mode(0o700);
            file.as_file().set_permissions(permissions)?;
        }
    }

    Ok(AskpassScript {
        path: file.into_temp_path(),
    })
}

fn parse_remote_pwd(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.strip_prefix("Remote working directory: ")
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    })
}

fn parse_sftp_ls(output: &str, parent: &StandardizedPath) -> Result<Vec<SftpEntry>> {
    let mut entries = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.starts_with("sftp>")
            || line.starts_with("Remote working directory:")
            || line.starts_with("Connected to ")
            || line.starts_with("Changing to:")
        {
            continue;
        }

        let Some(first) = line.chars().next() else {
            continue;
        };
        if !matches!(first, 'd' | '-' | 'l') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let mode = parts.next().unwrap_or_default();
        for _ in 0..7 {
            let _ = parts.next();
        }
        let name_with_suffix = parts.collect::<Vec<_>>().join(" ");
        let Some(name) = name_with_suffix.split(" -> ").next().map(str::trim) else {
            continue;
        };
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }

        let path = remote_child_path(parent.as_str(), name)?;
        entries.push(SftpEntry {
            path,
            is_dir: mode.starts_with('d'),
        });
    }
    Ok(entries)
}

fn remote_child_path(parent: &str, name: &str) -> Result<StandardizedPath> {
    let joined = if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    };
    StandardizedPath::try_with_encoding(&joined, typed_path::PathType::Unix)
        .with_context(|| format!("invalid remote child path: {joined}"))
}

pub fn quote_sftp_path(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn std_path(path: &str) -> StandardizedPath {
        StandardizedPath::try_with_encoding(path, typed_path::PathType::Unix).unwrap()
    }

    #[test]
    fn parses_pwd() {
        assert_eq!(
            parse_remote_pwd("Remote working directory: /home/alice\n").as_deref(),
            Some("/home/alice")
        );
    }

    #[test]
    fn parses_ls_entries() {
        let output = "\
drwxr-xr-x    2 alice users        4096 May 22 10:00 src
-rw-r--r--    1 alice users          12 May 22 10:00 README.md
lrwxrwxrwx    1 alice users           3 May 22 10:00 link -> src
";
        let entries = parse_sftp_ls(output, &std_path("/home/alice")).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path.as_str(), "/home/alice/src");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].path.as_str(), "/home/alice/README.md");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[2].path.as_str(), "/home/alice/link");
    }
}
