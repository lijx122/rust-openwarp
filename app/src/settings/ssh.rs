use settings::{
    macros::define_settings_group, RespectUserSyncSetting, SupportedPlatforms, SyncToCloud,
};
use std::time::Duration;
use warp_core::features::FeatureFlag;

define_settings_group!(SshSettings,
    settings: [
        enable_legacy_ssh_wrapper: EnableSshWrapper {
            type: bool,
            default: true,
            supported_platforms: SupportedPlatforms::ALL,
            sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
            private: false,
            storage_key: "EnableSSHWrapper",
            toml_path: "warpify.ssh.enable_legacy_ssh_wrapper",
            description: "Whether the legacy SSH wrapper is enabled for SSH sessions.",
        },
        keepalive_interval_secs: KeepaliveIntervalSecs {
            type: u64,
            default: 30,
            supported_platforms: SupportedPlatforms::ALL,
            sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
            private: false,
            storage_key: "SshKeepaliveIntervalSecs",
            toml_path: "warpify.ssh.keepalive_interval_secs",
            description: "How often SSH/SFTP keepalive packets are sent, in seconds.",
        },
        keepalive_max_failures: KeepaliveMaxFailures {
            type: u32,
            default: 3,
            supported_platforms: SupportedPlatforms::ALL,
            sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
            private: false,
            storage_key: "SshKeepaliveMaxFailures",
            toml_path: "warpify.ssh.keepalive_max_failures",
            description: "How many keepalive failures are tolerated before reconnecting.",
        },
        tcp_keepalive_enabled: TcpKeepaliveEnabled {
            type: bool,
            default: true,
            supported_platforms: SupportedPlatforms::ALL,
            sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
            private: false,
            storage_key: "SshTcpKeepaliveEnabled",
            toml_path: "warpify.ssh.tcp_keepalive_enabled",
            description: "Whether TCP keepalive is enabled for SSH/SFTP connections.",
        },
        auto_reconnect_enabled: AutoReconnectEnabled {
            type: bool,
            default: true,
            supported_platforms: SupportedPlatforms::ALL,
            sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
            private: false,
            storage_key: "SshAutoReconnectEnabled",
            toml_path: "warpify.ssh.auto_reconnect_enabled",
            description: "Whether SSH/SFTP connections should reconnect automatically after disconnects.",
        },
        auto_reconnect_max_attempts: AutoReconnectMaxAttempts {
            type: u32,
            default: 10,
            supported_platforms: SupportedPlatforms::ALL,
            sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
            private: false,
            storage_key: "SshAutoReconnectMaxAttempts",
            toml_path: "warpify.ssh.auto_reconnect_max_attempts",
            description: "Maximum number of SSH/SFTP reconnect attempts.",
        },
        sftp_stale_threshold_secs: SftpStaleThresholdSecs {
            type: u64,
            default: 60,
            supported_platforms: SupportedPlatforms::ALL,
            sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
            private: false,
            storage_key: "SftpStaleThresholdSecs",
            toml_path: "warpify.sftp.stale_threshold_secs",
            description: "How long SFTP tree data can stay idle before it is considered stale, in seconds.",
        },
    ]
);

impl SshSettings {
    pub fn keepalive_options(&self) -> remote_server::ssh::SshKeepaliveOptions {
        if !FeatureFlag::SshKeepaliveV2.is_enabled() {
            return remote_server::ssh::SshKeepaliveOptions::default();
        }

        remote_server::ssh::SshKeepaliveOptions {
            server_alive_interval_secs: Some(*self.keepalive_interval_secs.value()),
            server_alive_count_max: Some(*self.keepalive_max_failures.value()),
            tcp_keepalive_enabled: Some(*self.tcp_keepalive_enabled.value()),
        }
    }

    pub fn remote_server_manager_config(
        &self,
    ) -> remote_server::manager::RemoteServerManagerConfig {
        if !FeatureFlag::SshKeepaliveV2.is_enabled() {
            return remote_server::manager::RemoteServerManagerConfig {
                auto_reconnect_enabled: false,
                max_attempts: 1,
                initial_backoff: Duration::from_secs(1),
                max_backoff: Duration::from_secs(30),
            };
        }

        remote_server::manager::RemoteServerManagerConfig {
            auto_reconnect_enabled: *self.auto_reconnect_enabled.value(),
            max_attempts: (*self.auto_reconnect_max_attempts.value()).max(1),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}
