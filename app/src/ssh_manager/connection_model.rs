use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use warp_core::{HostId, SessionId};
use warpui::{Entity, EntityId, ModelContext, SingletonEntity};

use crate::remote_server::manager::RemoteServerManager;
use crate::remote_server::manager::RemoteServerManagerEvent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

#[derive(Clone, Debug)]
pub struct SshDisconnectRecord {
    pub at: SystemTime,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct SshNodeConnection {
    pub terminal_view_id: EntityId,
    pub session_id: Option<SessionId>,
    pub host_id: Option<HostId>,
    pub state: SshConnectionState,
    pub recent_disconnects: VecDeque<SshDisconnectRecord>,
}

pub struct SshConnectionModel {
    pending_terminal_to_node: HashMap<EntityId, String>,
    session_to_node: HashMap<SessionId, String>,
    node_connections: HashMap<String, SshNodeConnection>,
}

impl SshConnectionModel {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&RemoteServerManager::handle(ctx), |me, event, ctx| {
            me.handle_remote_server_event(event, ctx);
        });

        Self {
            pending_terminal_to_node: HashMap::new(),
            session_to_node: HashMap::new(),
            node_connections: HashMap::new(),
        }
    }

    pub fn start_connect(
        &mut self,
        node_id: String,
        terminal_view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        self.pending_terminal_to_node
            .insert(terminal_view_id, node_id.clone());
        self.node_connections.insert(
            node_id,
            SshNodeConnection {
                terminal_view_id,
                session_id: None,
                host_id: None,
                state: SshConnectionState::Connecting,
                recent_disconnects: VecDeque::new(),
            },
        );
        ctx.notify();
    }

    pub fn mark_connected_with_host_id(
        &mut self,
        node_id: String,
        host_id: HostId,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(connection) = self.node_connections.get_mut(&node_id) else {
            log::warn!("ssh connection missing for node {node_id} while marking connected");
            return;
        };
        connection.host_id = Some(host_id);
        connection.state = SshConnectionState::Connected;
        ctx.notify();
    }

    pub fn mark_failed(&mut self, node_id: String, reason: String, ctx: &mut ModelContext<Self>) {
        let Some(connection) = self.node_connections.get_mut(&node_id) else {
            log::warn!("ssh connection missing for node {node_id} while marking failed: {reason}");
            return;
        };
        connection.state = SshConnectionState::Failed;
        Self::push_disconnect_record(&mut connection.recent_disconnects, reason);
        ctx.notify();
    }

    pub fn bind_terminal_session(
        &mut self,
        terminal_view_id: EntityId,
        session_id: SessionId,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(node_id) = self.pending_terminal_to_node.remove(&terminal_view_id) else {
            return;
        };
        let host_id = RemoteServerManager::as_ref(ctx)
            .host_id_for_session(session_id)
            .cloned();
        self.session_to_node.insert(session_id, node_id.clone());
        let connection = self
            .node_connections
            .entry(node_id)
            .or_insert(SshNodeConnection {
                terminal_view_id,
                session_id: Some(session_id),
                host_id: host_id.clone(),
                state: SshConnectionState::Connecting,
                recent_disconnects: VecDeque::new(),
            });
        connection.terminal_view_id = terminal_view_id;
        connection.session_id = Some(session_id);
        connection.host_id = host_id;
        connection.state = if connection.host_id.is_some() {
            SshConnectionState::Connected
        } else {
            SshConnectionState::Connecting
        };
        ctx.notify();
    }

    pub fn connection_for_node(&self, node_id: &str) -> Option<&SshNodeConnection> {
        self.node_connections.get(node_id)
    }

    pub fn node_id_for_terminal_view(&self, terminal_view_id: EntityId) -> Option<&str> {
        if let Some(node_id) = self.pending_terminal_to_node.get(&terminal_view_id) {
            return Some(node_id);
        }

        self.node_connections
            .iter()
            .find_map(|(node_id, connection)| {
                (connection.terminal_view_id == terminal_view_id).then_some(node_id.as_str())
            })
    }

    pub fn host_id_for_node(&self, node_id: &str) -> Option<&HostId> {
        self.node_connections.get(node_id)?.host_id.as_ref()
    }

    pub fn release_terminal_view(
        &mut self,
        terminal_view_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) -> Option<String> {
        let pending_node_id = self.pending_terminal_to_node.remove(&terminal_view_id);
        let node_id = pending_node_id.or_else(|| {
            self.node_connections
                .iter()
                .find_map(|(node_id, connection)| {
                    (connection.terminal_view_id == terminal_view_id).then_some(node_id.clone())
                })
        })?;

        if let Some(connection) = self.node_connections.remove(&node_id) {
            if let Some(session_id) = connection.session_id {
                self.session_to_node.remove(&session_id);
            }
        }

        ctx.notify();
        Some(node_id)
    }

    fn handle_remote_server_event(
        &mut self,
        event: &RemoteServerManagerEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(session_id) = event.session_id() else {
            return;
        };
        let Some(node_id) = self.session_to_node.get(&session_id).cloned() else {
            return;
        };
        let Some(connection) = self.node_connections.get_mut(&node_id) else {
            return;
        };

        match event {
            RemoteServerManagerEvent::SessionConnected { host_id, .. }
            | RemoteServerManagerEvent::SessionReconnected { host_id, .. } => {
                connection.host_id = Some(host_id.clone());
                connection.state = SshConnectionState::Connected;
            }
            RemoteServerManagerEvent::SessionReconnectStarted { .. } => {
                connection.state = SshConnectionState::Reconnecting;
            }
            RemoteServerManagerEvent::SessionConnectionFailed { error, .. } => {
                connection.state = SshConnectionState::Failed;
                Self::push_disconnect_record(&mut connection.recent_disconnects, error.clone());
            }
            RemoteServerManagerEvent::SessionDisconnected { exit_status, .. } => {
                connection.state = SshConnectionState::Disconnected;
                let reason = match exit_status {
                    Some(status) => format!(
                        "remote server exited (code={:?}, signal_killed={})",
                        status.code, status.signal_killed
                    ),
                    None => "remote server disconnected".to_string(),
                };
                Self::push_disconnect_record(&mut connection.recent_disconnects, reason);
            }
            RemoteServerManagerEvent::SessionDeregistered { .. } => {
                connection.state = SshConnectionState::Disconnected;
            }
            RemoteServerManagerEvent::SessionConnecting { .. }
            | RemoteServerManagerEvent::HostConnected { .. }
            | RemoteServerManagerEvent::HostDisconnected { .. }
            | RemoteServerManagerEvent::NavigatedToDirectory { .. }
            | RemoteServerManagerEvent::RepoMetadataSnapshot { .. }
            | RemoteServerManagerEvent::RepoMetadataUpdated { .. }
            | RemoteServerManagerEvent::RepoMetadataDirectoryLoaded { .. }
            | RemoteServerManagerEvent::SetupStateChanged { .. }
            | RemoteServerManagerEvent::BinaryCheckComplete { .. }
            | RemoteServerManagerEvent::BinaryInstallComplete { .. }
            | RemoteServerManagerEvent::ClientRequestFailed { .. }
            | RemoteServerManagerEvent::ServerMessageDecodingError { .. } => {}
        }

        ctx.notify();
    }

    fn push_disconnect_record(records: &mut VecDeque<SshDisconnectRecord>, reason: String) {
        records.push_front(SshDisconnectRecord {
            at: SystemTime::now(),
            reason,
        });
        while records.len() > 5 {
            let _ = records.pop_back();
        }
    }
}

impl Entity for SshConnectionModel {
    type Event = ();
}

impl SingletonEntity for SshConnectionModel {}
