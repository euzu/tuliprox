use crate::{
    protocol::{AgentId, AgentMessage, Command, Envelope, PlaybackEvent, RunId},
    TestkitError,
};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, Mutex},
    time::Instant,
};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as ClientMessage};

pub type ControllerState = Arc<SharedControllerState>;

/// One connected agent registration.
///
/// `token` identifies the exact websocket connection that registered the agent.
/// A replacement connection for the same `AgentId` overwrites the registration
/// with a new token, so the previous connection's cleanup cannot delete it.
struct RegisteredAgent {
    token: u64,
    sender: mpsc::Sender<Envelope<Command>>,
}

pub struct SharedControllerState {
    run_id: RunId,
    generation: u64,
    agents: Mutex<HashMap<AgentId, RegisteredAgent>>,
    ready_agents: Mutex<HashSet<AgentId>>,
    last_seen: Mutex<HashMap<AgentId, Instant>>,
    events: Mutex<Vec<Envelope<AgentMessage>>>,
    next_controller_sequence: AtomicU64,
    next_connection_token: AtomicU64,
}

impl SharedControllerState {
    #[must_use]
    pub fn new(run_id: RunId, generation: u64) -> Self {
        Self {
            run_id,
            generation,
            agents: Mutex::new(HashMap::new()),
            ready_agents: Mutex::new(HashSet::new()),
            last_seen: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            next_controller_sequence: AtomicU64::new(1),
            next_connection_token: AtomicU64::new(1),
        }
    }

    #[must_use]
    pub fn next_command_sequence(&self) -> u64 { self.next_controller_sequence.fetch_add(1, Ordering::Relaxed) }

    fn next_connection_token(&self) -> u64 { self.next_connection_token.fetch_add(1, Ordering::Relaxed) }

    async fn register_agent(&self, agent_id: AgentId, token: u64, sender: mpsc::Sender<Envelope<Command>>) {
        let mut agents = self.agents.lock().await;
        agents.insert(agent_id.clone(), RegisteredAgent { token, sender });
        self.last_seen.lock().await.insert(agent_id, Instant::now());
    }

    async fn unregister_agent(&self, agent_id: &AgentId, token: u64) {
        let mut agents = self.agents.lock().await;
        if agents.get(agent_id).is_some_and(|registered| registered.token == token) {
            agents.remove(agent_id);
            self.ready_agents.lock().await.remove(agent_id);
            self.last_seen.lock().await.remove(agent_id);
        }
    }

    /// Applies an agent message only while the sending connection still owns the
    /// registration token. The `agents` lock is held across the update so a
    /// superseded socket cannot refresh `last_seen`, re-insert readiness, or append
    /// playback events after a replacement connection has registered.
    async fn apply_agent_message(&self, agent_id: &AgentId, token: u64, event: Envelope<AgentMessage>) -> bool {
        let agents = self.agents.lock().await;
        if agents.get(agent_id).is_none_or(|registered| registered.token != token) {
            return false;
        }
        let is_ready = matches!(event.payload, AgentMessage::Ready);
        let is_event = matches!(event.payload, AgentMessage::Event { .. });
        if is_ready {
            self.ready_agents.lock().await.insert(agent_id.clone());
        }
        if is_event {
            self.events.lock().await.push(event);
        }
        self.last_seen.lock().await.insert(agent_id.clone(), Instant::now());
        true
    }

    #[must_use]
    pub fn run_id(&self) -> RunId { self.run_id.clone() }

    pub async fn dispatch(&self, agent_id: &AgentId, command: Envelope<Command>) -> Result<(), TestkitError> {
        command.validate()?;
        if command.run_id != self.run_id || command.run_generation != self.generation {
            return Err(TestkitError::Protocol("command does not belong to the active run generation".to_owned()));
        }
        let sender = self
            .agents
            .lock()
            .await
            .get(agent_id)
            .map(|registered| registered.sender.clone())
            .ok_or_else(|| TestkitError::Protocol(format!("agent {} is not connected", agent_id.0)))?;
        sender.send(command).await.map_err(|_| TestkitError::Protocol("agent control channel closed".to_owned()))
    }

    pub async fn connected_agents(&self) -> Vec<AgentId> { self.agents.lock().await.keys().cloned().collect() }

    pub async fn ready_agents(&self) -> Vec<AgentId> {
        const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
        let now = Instant::now();
        let ready = self.ready_agents.lock().await;
        let seen = self.last_seen.lock().await;
        ready
            .iter()
            .filter(|agent| {
                seen.get(*agent).is_some_and(|last_seen| now.duration_since(*last_seen) <= HEARTBEAT_TIMEOUT)
            })
            .cloned()
            .collect()
    }

    pub async fn take_events(&self) -> Vec<Envelope<AgentMessage>> { std::mem::take(&mut *self.events.lock().await) }

    /// Removes only an event belonging to the requested playback. Events for
    /// concurrent starts remain queued for their own waiter.
    pub async fn take_playback_event(&self, playback_id: &str) -> Option<PlaybackEvent> {
        let mut events = self.events.lock().await;
        let position = events.iter().position(|envelope| {
            matches!(
                &envelope.payload,
                AgentMessage::Event {
                    event: PlaybackEvent::FirstValidFrame { playback_id: observed, .. }
                        | PlaybackEvent::HeadersReceived { playback_id: observed, .. }
                        | PlaybackEvent::Progress { playback_id: observed, .. }
                        | PlaybackEvent::Terminal { playback_id: observed, .. },
                } if observed.0 == playback_id
            )
        })?;
        match events.remove(position).payload {
            AgentMessage::Event { event } => Some(event),
            AgentMessage::Hello { .. } | AgentMessage::Ready | AgentMessage::Heartbeat => None,
        }
    }
}

pub async fn controller_websocket(State(state): State<ControllerState>, websocket: WebSocketUpgrade) -> Response {
    std::future::ready(()).await;
    websocket.on_upgrade(move |socket| serve_agent(socket, state)).into_response()
}

async fn serve_agent(socket: WebSocket, state: ControllerState) {
    let (mut sink, mut stream) = socket.split();
    let Some(Ok(Message::Text(first))) = stream.next().await else { return };
    let Ok(hello) = serde_json::from_str::<Envelope<AgentMessage>>(&first) else { return };
    if hello.validate().is_err() || hello.run_id != state.run_id || hello.run_generation != state.generation {
        return;
    }
    let AgentMessage::Hello { .. } = hello.payload else { return };
    let agent_id = hello.agent_id;
    let connection_token = state.next_connection_token();
    let (tx, mut rx) = mpsc::channel(32);
    state.register_agent(agent_id.clone(), connection_token, tx).await;
    loop {
        tokio::select! {
            Some(command) = rx.recv() => {
                let Ok(encoded) = serde_json::to_string(&command) else { break };
                if sink.send(Message::Text(encoded.into())).await.is_err() { break; }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(Message::Text(text))) => {
                    if let Ok(event) = serde_json::from_str::<Envelope<AgentMessage>>(&text) {
                        if event.validate().is_ok()
                            && event.run_id == state.run_id
                            && event.run_generation == state.generation
                            && event.agent_id == agent_id
                            && !state.apply_agent_message(&agent_id, connection_token, event).await
                        {
                            // A newer connection replaced this registration; stop
                            // serving the stale socket before it can mutate state.
                            break;
                        }
                    }
                }
                _ => {}
            },
        }
    }
    state.unregister_agent(&agent_id, connection_token).await;
}

pub struct AgentControlConnection {
    commands: mpsc::Receiver<Envelope<Command>>,
    events: mpsc::Sender<Envelope<AgentMessage>>,
}

impl AgentControlConnection {
    pub async fn connect(controller_url: &str, hello: Envelope<AgentMessage>) -> Result<Self, TestkitError> {
        hello.validate()?;
        if !matches!(hello.payload, AgentMessage::Hello { .. }) {
            return Err(TestkitError::Protocol("first agent message must be hello".to_owned()));
        }
        let (mut socket, _) =
            connect_async(controller_url).await.map_err(|error| TestkitError::Protocol(error.to_string()))?;
        let encoded = serde_json::to_string(&hello).map_err(|error| TestkitError::Protocol(error.to_string()))?;
        socket
            .send(ClientMessage::Text(encoded.into()))
            .await
            .map_err(|error| TestkitError::Protocol(error.to_string()))?;
        let (mut sink, mut stream) = socket.split();
        let (command_tx, command_rx) = mpsc::channel(32);
        let (event_tx, mut event_rx) = mpsc::channel::<Envelope<AgentMessage>>(32);
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let Ok(encoded) = serde_json::to_string(&event) else { break };
                if sink.send(ClientMessage::Text(encoded.into())).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(message) = stream.next().await {
                let Ok(message) = message else { break };
                match message {
                    ClientMessage::Text(text) => {
                        let Ok(command) = serde_json::from_str::<Envelope<Command>>(&text) else { continue };
                        if command.validate().is_err() || command_tx.send(command).await.is_err() {
                            break;
                        }
                    }
                    ClientMessage::Close(_) => break,
                    _ => {}
                }
            }
        });
        Ok(Self { commands: command_rx, events: event_tx })
    }

    pub async fn next_command(&mut self) -> Result<Option<Envelope<Command>>, TestkitError> {
        Ok(self.commands.recv().await)
    }

    pub async fn send_event(&self, event: Envelope<AgentMessage>) -> Result<(), TestkitError> {
        event.validate()?;
        self.events.send(event).await.map_err(|_| TestkitError::Protocol("agent event channel closed".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};

    #[tokio::test]
    async fn unknown_agent_cannot_receive_commands() {
        let state = SharedControllerState::new(RunId::new("run"), 1);
        let command = Envelope {
            schema_version: 1,
            run_id: RunId::new("run"),
            run_generation: 1,
            message_id: "command".to_owned(),
            agent_id: AgentId::new("controller"),
            agent_boot_id: "boot".to_owned(),
            source_sequence: 1,
            caused_by_command_id: None,
            local_elapsed_nanos: 0,
            payload: Command::StopAll,
        };
        assert!(state.dispatch(&AgentId::new("missing"), command).await.is_err());
    }

    #[tokio::test]
    async fn stale_connection_cleanup_does_not_remove_replacement_registration() {
        let state = SharedControllerState::new(RunId::new("run"), 1);
        let agent_id = AgentId::new("agent");

        let (old_tx, _old_rx) = mpsc::channel(1);
        let old_token = state.next_connection_token();
        state.register_agent(agent_id.clone(), old_token, old_tx).await;
        assert!(state.connected_agents().await.contains(&agent_id));

        let (new_tx, _new_rx) = mpsc::channel(1);
        let new_token = state.next_connection_token();
        state.register_agent(agent_id.clone(), new_token, new_tx).await;

        // The previous connection's cleanup must not delete the replacement.
        state.unregister_agent(&agent_id, old_token).await;
        assert!(state.connected_agents().await.contains(&agent_id));

        state.unregister_agent(&agent_id, new_token).await;
        assert!(!state.connected_agents().await.contains(&agent_id));
    }

    #[tokio::test]
    async fn superseded_connection_cannot_update_state_or_append_events() {
        let state = SharedControllerState::new(RunId::new("run"), 1);
        let agent_id = AgentId::new("agent");

        let (old_tx, _old_rx) = mpsc::channel(1);
        let old_token = state.next_connection_token();
        state.register_agent(agent_id.clone(), old_token, old_tx).await;

        let (new_tx, _new_rx) = mpsc::channel(1);
        let new_token = state.next_connection_token();
        state.register_agent(agent_id.clone(), new_token, new_tx).await;

        let ready = |sequence: u64| Envelope {
            schema_version: 1,
            run_id: RunId::new("run"),
            run_generation: 1,
            message_id: format!("ready-{sequence}"),
            agent_id: agent_id.clone(),
            agent_boot_id: "boot".to_owned(),
            source_sequence: sequence,
            caused_by_command_id: None,
            local_elapsed_nanos: 0,
            payload: AgentMessage::Ready,
        };
        let event = Envelope {
            schema_version: 1,
            run_id: RunId::new("run"),
            run_generation: 1,
            message_id: "event".to_owned(),
            agent_id: agent_id.clone(),
            agent_boot_id: "boot".to_owned(),
            source_sequence: 9,
            caused_by_command_id: None,
            local_elapsed_nanos: 0,
            payload: AgentMessage::Event {
                event: crate::protocol::PlaybackEvent::Terminal {
                    playback_id: crate::protocol::PlaybackId::new("playback"),
                    outcome: "passed".to_owned(),
                    typed_outcome: None,
                },
            },
        };

        // A superseded socket cannot mark the agent ready or append events.
        assert!(!state.apply_agent_message(&agent_id, old_token, ready(1)).await);
        assert!(!state.apply_agent_message(&agent_id, old_token, event).await);
        assert!(matches!(state.ready_agents().await.as_slice(), []));
        assert!(matches!(state.take_events().await.as_slice(), []));

        // The active connection still can.
        assert!(state.apply_agent_message(&agent_id, new_token, ready(2)).await);
        assert!(state.ready_agents().await.contains(&agent_id));
    }

    #[tokio::test]
    async fn agent_receives_generation_bound_command_over_websocket() {
        let state: ControllerState = Arc::new(SharedControllerState::new(RunId::new("run"), 1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/agent", get(controller_websocket)).with_state(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let agent_id = AgentId::new("agent");
        let hello = Envelope {
            schema_version: 1,
            run_id: RunId::new("run"),
            run_generation: 1,
            message_id: "hello".to_owned(),
            agent_id: agent_id.clone(),
            agent_boot_id: "boot".to_owned(),
            source_sequence: 1,
            caused_by_command_id: None,
            local_elapsed_nanos: 0,
            payload: AgentMessage::Hello {
                hostname: "test".to_owned(),
                supported_protocols: vec!["ts".to_owned()],
                maximum_listeners: 1,
            },
        };
        let mut agent = AgentControlConnection::connect(&format!("ws://{address}/agent"), hello).await.unwrap();
        for _ in 0..10 {
            if !state.connected_agents().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        agent
            .send_event(Envelope {
                schema_version: 1,
                run_id: RunId::new("run"),
                run_generation: 1,
                message_id: "ready".to_owned(),
                agent_id: agent_id.clone(),
                agent_boot_id: "boot".to_owned(),
                source_sequence: 2,
                caused_by_command_id: None,
                local_elapsed_nanos: 0,
                payload: AgentMessage::Ready,
            })
            .await
            .unwrap();
        for _ in 0..10 {
            if state.ready_agents().await.contains(&agent_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(state.ready_agents().await.contains(&agent_id));
        let command = Envelope {
            schema_version: 1,
            run_id: RunId::new("run"),
            run_generation: 1,
            message_id: "stop".to_owned(),
            agent_id: AgentId::new("controller"),
            agent_boot_id: "controller-boot".to_owned(),
            source_sequence: 1,
            caused_by_command_id: None,
            local_elapsed_nanos: 0,
            payload: Command::StopAll,
        };
        state.dispatch(&agent_id, command).await.unwrap();
        assert!(matches!(agent.next_command().await, Ok(Some(Envelope { payload: Command::StopAll, .. }))));
        agent
            .send_event(Envelope {
                schema_version: 1,
                run_id: RunId::new("run"),
                run_generation: 1,
                message_id: "event".to_owned(),
                agent_id,
                agent_boot_id: "boot".to_owned(),
                source_sequence: 3,
                caused_by_command_id: None,
                local_elapsed_nanos: 0,
                payload: AgentMessage::Event {
                    event: crate::protocol::PlaybackEvent::Terminal {
                        playback_id: crate::protocol::PlaybackId::new("playback"),
                        outcome: "passed".to_owned(),
                        typed_outcome: None,
                    },
                },
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(state.take_events().await.len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn waiting_for_one_playback_keeps_other_playback_events() {
        let state = SharedControllerState::new(RunId::new("run"), 1);
        for playback_id in ["first", "second"] {
            state.events.lock().await.push(Envelope {
                schema_version: 1,
                run_id: RunId::new("run"),
                run_generation: 1,
                message_id: format!("event-{playback_id}"),
                agent_id: AgentId::new("agent"),
                agent_boot_id: "boot".to_owned(),
                source_sequence: 1,
                caused_by_command_id: None,
                local_elapsed_nanos: 0,
                payload: AgentMessage::Event {
                    event: PlaybackEvent::FirstValidFrame {
                        playback_id: crate::protocol::PlaybackId::new(playback_id),
                        sequence: 0,
                    },
                },
            });
        }
        assert!(matches!(
            state.take_playback_event("second").await,
            Some(PlaybackEvent::FirstValidFrame { playback_id, .. }) if playback_id.0 == "second"
        ));
        assert!(matches!(
            state.take_playback_event("first").await,
            Some(PlaybackEvent::FirstValidFrame { playback_id, .. }) if playback_id.0 == "first"
        ));
    }
}
