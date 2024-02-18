use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{select, SinkExt, StreamExt};
use log::{error, info, warn};
use ng_server::protocol;
use rand::seq::SliceRandom;
use tokio::{
    net::TcpListener,
    sync::mpsc::{unbounded_channel, UnboundedSender},
    task::JoinHandle,
    time::sleep,
};
use tokio_stream::wrappers::{IntervalStream, UnboundedReceiverStream};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{self, Message},
};

const TIMEOUT_DURATION: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct Game {
    state: protocol::GameState,
    connections: HashMap<String, UnboundedSender<protocol::Response>>,
    client_states: HashMap<String, protocol::ClientState>,
    ready_players: HashSet<String>,
    assignment_map: HashMap<String, String>,
    message_history: Vec<(String, String)>,
    voting_session: Option<VotingSession>,
}

impl Game {
    pub fn renew_inherit_clients(&mut self) {
        self.client_states = HashMap::from_iter(self.client_states.keys().map(|player| {
            (
                player.clone(),
                protocol::ClientState::Player(protocol::PlayerState::Alive, None),
            )
        }));
        self.state = protocol::GameState::default();
        self.ready_players = HashSet::default();
        self.assignment_map = HashMap::default();
        self.message_history = Vec::new();
        self.voting_session = None;
    }
}

pub struct VotingSession {
    agreed: HashSet<String>,
    objection: HashSet<String>,
}

fn protocol_to_websocket_message(protocol: protocol::Response) -> Message {
    Message::Text(serde_json::to_string(&protocol).unwrap())
}

async fn process_message(
    message: Result<Message, tungstenite::Error>,
    response_sender: &UnboundedSender<protocol::Response>,
    session_game: &Arc<Mutex<Game>>,
    session_addr: SocketAddr,
    session_name: &mut Option<String>,
    timer_handle: &mut Option<JoinHandle<()>>,
) -> bool {
    match message {
        Ok(message) => match message {
            tungstenite::Message::Text(content) => {
                let session_action: Result<protocol::Actions, _> = serde_json::from_str(&content);
                if let Ok(action) = session_action {
                    match action {
                        protocol::Actions::Login { name } => {
                            info!(
                                "Login request from {}, requested name: {}",
                                session_addr, name
                            );
                            if session_name.is_some() {
                                response_sender
                                    .send(protocol::Response::LoginResult {
                                        excuse: Some("您已经加入过了".to_owned()),
                                    })
                                    .unwrap();
                            }
                            let mut game_lock = session_game.lock().unwrap();
                            if game_lock.connections.contains_key(&name) {
                                response_sender
                                    .send(protocol::Response::LoginResult {
                                        excuse: Some("该名称已占用".to_owned()),
                                    })
                                    .unwrap();
                                info!(
                                    "Rejected login request from {} because name is occupied",
                                    session_addr
                                );
                            } else {
                                response_sender
                                    .send(protocol::Response::LoginResult { excuse: None })
                                    .unwrap();
                                *session_name = Some(name.clone());
                                game_lock
                                    .connections
                                    .insert(name.clone(), response_sender.clone());
                                let client_state = match game_lock.state {
                                    protocol::GameState::Joining => protocol::ClientState::Player(
                                        protocol::PlayerState::Alive,
                                        None,
                                    ),
                                    protocol::GameState::Assigning
                                    | protocol::GameState::Fighting => {
                                        protocol::ClientState::Spectator
                                    }
                                };
                                game_lock.client_states.insert(name.clone(), client_state);
                                response_sender
                                    .send(protocol::Response::MessageHistory {
                                        history: game_lock.message_history.clone(),
                                    })
                                    .unwrap();
                                response_sender
                                    .send(protocol::Response::Overview {
                                        clients: game_lock.client_states.clone(),
                                        game_state: game_lock.state.clone(),
                                    })
                                    .unwrap();
                                for client in game_lock.connections.values() {
                                    client
                                        .send(protocol::Response::PlayerJoin { name: name.clone() })
                                        .unwrap();
                                }
                            }
                        }
                        protocol::Actions::Send(msg) => {
                            if let Some(ref name) = session_name {
                                let mut game_lock = session_game.lock().unwrap();
                                for client in game_lock.connections.values() {
                                    client
                                        .send(protocol::Response::MessageFrom {
                                            sender: name.clone(),
                                            content: msg.clone(),
                                        })
                                        .unwrap();
                                }
                                game_lock.message_history.push((name.clone(), msg.clone()));

                                if game_lock.state == protocol::GameState::Fighting {
                                    if let protocol::ClientState::Player(
                                        protocol::PlayerState::Alive,
                                        Some(word),
                                    ) = game_lock.client_states.get(name).unwrap()
                                    {
                                        if msg.contains(word) {
                                            let word = word.clone();

                                            game_lock
                                                .client_states
                                                .get_mut(name)
                                                .unwrap()
                                                .set_out();

                                            game_lock.ready_players.remove(name);

                                            let timer_game = session_game.clone();
                                            info!("Timer reset");
                                            if let Some(ref timer_handle) = &timer_handle {
                                                timer_handle.abort();
                                            }
                                            *timer_handle = Some(tokio::spawn(async move {
                                                sleep(TIMEOUT_DURATION).await;
                                                let mut game_lock = timer_game.lock().unwrap();
                                                for client in game_lock.connections.values() {
                                                    client
                                                        .send(protocol::Response::GameEndTimeout)
                                                        .unwrap();
                                                }
                                                game_lock.renew_inherit_clients();
                                            }));

                                            game_lock.message_history.push((
                                                "".to_owned(),
                                                format!(
                                                    "玩家 {} 因说出NG词 {} 而出局！",
                                                    name, word
                                                ),
                                            ));
                                            for client in game_lock.connections.values() {
                                                client
                                                    .send(protocol::Response::PlayerOut {
                                                        quitter: name.clone(),
                                                        word: word.clone(),
                                                        suicide: false,
                                                    })
                                                    .unwrap();
                                                client
                                                    .send(protocol::Response::MessageFrom {
                                                        sender: "".to_owned(),
                                                        content: format!(
                                                            "玩家 {} 因说出NG词 {} 而出局！",
                                                            name, word
                                                        ),
                                                    })
                                                    .unwrap();
                                                client
                                                    .send(protocol::Response::TimerReset {
                                                        timer: TIMEOUT_DURATION,
                                                    })
                                                    .unwrap();
                                            }

                                            if game_lock.ready_players.len() <= 1 {
                                                let winner =
                                                    game_lock.ready_players.iter().next().unwrap();

                                                let winner_word = game_lock
                                                    .client_states
                                                    .get(winner)
                                                    .unwrap_or(&protocol::ClientState::Player(
                                                        protocol::PlayerState::Alive,
                                                        Some("".to_owned()),
                                                    ))
                                                    .get_word()
                                                    .unwrap();

                                                for client in game_lock.connections.values() {
                                                    client
                                                        .send(protocol::Response::GameWin {
                                                            winner: winner.clone(),
                                                            word: winner_word.clone(),
                                                        })
                                                        .unwrap();
                                                }

                                                game_lock.renew_inherit_clients();
                                                if let Some(timer_handle) = timer_handle.take() {
                                                    timer_handle.abort();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        protocol::Actions::Suicide => {
                            if let Some(ref name) = session_name {
                                let mut game_lock = session_game.lock().unwrap();
                                if game_lock.state == protocol::GameState::Fighting {
                                    let killed =
                                        game_lock.client_states.get_mut(name).unwrap().set_out();

                                    if killed {
                                        game_lock.message_history.push((
                                            "".to_owned(),
                                            format!("玩家 {} 自爆出局！", name),
                                        ));
                                        game_lock.ready_players.remove(name);
                                        for client in game_lock.connections.values() {
                                            client
                                                .send(protocol::Response::PlayerOut {
                                                    quitter: name.clone(),
                                                    word: game_lock.client_states[name]
                                                        .get_word()
                                                        .unwrap(),
                                                    suicide: true,
                                                })
                                                .unwrap();
                                            client
                                                .send(protocol::Response::MessageFrom {
                                                    sender: "".to_owned(),
                                                    content: format!("玩家 {} 自爆出局！", name),
                                                })
                                                .unwrap();
                                            client
                                                .send(protocol::Response::TimerReset {
                                                    timer: TIMEOUT_DURATION,
                                                })
                                                .unwrap();
                                        }

                                        let timer_game = session_game.clone();
                                        info!("Timer reset");
                                        if let Some(ref timer_handle) = &timer_handle {
                                            timer_handle.abort();
                                        }
                                        *timer_handle = Some(tokio::spawn(async move {
                                            sleep(TIMEOUT_DURATION).await;
                                            let mut game_lock = timer_game.lock().unwrap();
                                            for client in game_lock.connections.values() {
                                                client
                                                    .send(protocol::Response::GameEndTimeout)
                                                    .unwrap();
                                            }
                                            game_lock.renew_inherit_clients();
                                        }));

                                        if game_lock.ready_players.len() <= 1 {
                                            let winner =
                                                game_lock.ready_players.iter().next().unwrap();

                                            let winner_word = game_lock
                                                .client_states
                                                .get(winner)
                                                .unwrap_or(&protocol::ClientState::Player(
                                                    protocol::PlayerState::Alive,
                                                    Some("".to_owned()),
                                                ))
                                                .get_word()
                                                .unwrap();

                                            for client in game_lock.connections.values() {
                                                client
                                                    .send(protocol::Response::GameWin {
                                                        winner: winner.clone(),
                                                        word: winner_word.clone(),
                                                    })
                                                    .unwrap();
                                            }

                                            game_lock.renew_inherit_clients();
                                            if let Some(ref timer_handle) = timer_handle.take() {
                                                timer_handle.abort();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        protocol::Actions::RequestAbort => {
                            if let Some(ref name) = session_name {
                                let mut game_lock = session_game.lock().unwrap();
                                match game_lock.state {
                                    protocol::GameState::Assigning
                                    | protocol::GameState::Fighting => {
                                        if game_lock.client_states[name].is_player() {
                                            if game_lock.voting_session.is_none() {
                                                for client in game_lock.connections.values() {
                                                    client
                                                        .send(protocol::Response::StartVoteAbort)
                                                        .unwrap();
                                                    client
                                                        .send(protocol::Response::VotedAbort {
                                                            voter: name.clone(),
                                                            abort: true,
                                                        })
                                                        .unwrap();
                                                }
                                                game_lock.voting_session = Some(VotingSession {
                                                    agreed: HashSet::from_iter(Some(name.clone())),
                                                    objection: HashSet::new(),
                                                });
                                            }
                                        }
                                    }
                                    protocol::GameState::Joining => (),
                                }
                            }
                        }
                        protocol::Actions::VoteAbort { abort } => {
                            if let Some(ref name) = session_name {
                                let mut game_lock = session_game.lock().unwrap();
                                if game_lock.client_states[name].is_player() {
                                    if let Some(ref mut voting_session) = game_lock.voting_session {
                                        if !voting_session.agreed.contains(name)
                                            && !voting_session.objection.contains(name)
                                        {
                                            if abort {
                                                voting_session.agreed.insert(name.clone());
                                            } else {
                                                voting_session.objection.insert(name.clone());
                                            }
                                        }

                                        let agree_count = voting_session.agreed.len();
                                        let objection_count = voting_session.objection.len();

                                        for client in game_lock.connections.values() {
                                            client
                                                .send(protocol::Response::VotedAbort {
                                                    voter: name.clone(),
                                                    abort,
                                                })
                                                .unwrap();
                                        }

                                        let player_count = game_lock
                                            .client_states
                                            .values()
                                            .filter(|cs| cs.is_player())
                                            .count();

                                        if player_count == agree_count + objection_count {
                                            if agree_count > objection_count {
                                                for client in game_lock.connections.values() {
                                                    client
                                                        .send(protocol::Response::VoteAbortResult {
                                                            abort: true,
                                                        })
                                                        .unwrap();
                                                }

                                                game_lock.renew_inherit_clients();
                                                if let Some(timer_handle) = timer_handle.take() {
                                                    timer_handle.abort();
                                                }
                                            } else {
                                                for client in game_lock.connections.values() {
                                                    client
                                                        .send(protocol::Response::VoteAbortResult {
                                                            abort: false,
                                                        })
                                                        .unwrap();
                                                }
                                                game_lock.voting_session = None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        protocol::Actions::AssignWord { word } => {
                            if let Some(ref name) = session_name {
                                let mut game_lock = session_game.lock().unwrap();
                                if let protocol::GameState::Assigning = game_lock.state {
                                    if word.trim().is_empty() {
                                        response_sender
                                            .send(protocol::Response::AssignResult {
                                                excuse: Some("该词被禁止".to_owned()),
                                            })
                                            .unwrap();
                                        return true;
                                    }

                                    if let Some(assignee) =
                                        game_lock.assignment_map.get(name).cloned()
                                    {
                                        if let Some(client_state) =
                                            game_lock.client_states.get_mut(&assignee)
                                        {
                                            client_state.set_word(word);

                                            response_sender
                                                .send(protocol::Response::AssignResult {
                                                    excuse: None,
                                                })
                                                .unwrap();
                                        } else {
                                            response_sender
                                                .send(protocol::Response::AssignResult {
                                                    excuse: Some("指定对象已经无效".to_owned()),
                                                })
                                                .unwrap();
                                        }
                                    }
                                }
                            }
                        }
                        protocol::Actions::SetReady => {
                            if let Some(ref name) = session_name {
                                let mut game_lock = session_game.lock().unwrap();
                                match game_lock.state {
                                    protocol::GameState::Joining => {
                                        if game_lock.ready_players.insert(name.clone()) {
                                            response_sender
                                                .send(protocol::Response::ReadyResult {
                                                    excuse: None,
                                                })
                                                .unwrap();
                                            for client in game_lock.connections.values() {
                                                client
                                                    .send(protocol::Response::PlayerReady {
                                                        name: name.clone(),
                                                    })
                                                    .unwrap();
                                            }

                                            if game_lock.ready_players.len()
                                                == game_lock.client_states.len()
                                                && game_lock.ready_players.len() > 1
                                            {
                                                game_lock.state = protocol::GameState::Assigning;
                                                game_lock.ready_players.clear();
                                                let mut name_list = game_lock
                                                    .client_states
                                                    .keys()
                                                    .cloned()
                                                    .collect::<Vec<_>>();
                                                name_list.shuffle(&mut rand::thread_rng());
                                                let player_count = name_list.len();
                                                for (idx, name) in name_list.iter().enumerate() {
                                                    game_lock.assignment_map.insert(
                                                        name.clone(),
                                                        name_list[(idx + 1) % player_count].clone(),
                                                    );
                                                    if let Some(cl_sender) =
                                                        game_lock.connections.get(name)
                                                    {
                                                        cl_sender
                                                            .send(protocol::Response::AssignStart {
                                                                assignee: name_list
                                                                    [(idx + 1) % player_count]
                                                                    .clone(),
                                                            })
                                                            .unwrap();
                                                    }
                                                }
                                            }
                                        } else {
                                            response_sender
                                                .send(protocol::Response::ReadyResult {
                                                    excuse: Some("您已经准备过了".to_owned()),
                                                })
                                                .unwrap();
                                        }
                                    }
                                    protocol::GameState::Assigning => {
                                        if let Some(assignee) = game_lock.assignment_map.get(name) {
                                            if game_lock
                                                .client_states
                                                .get(assignee)
                                                .unwrap_or(&protocol::ClientState::Spectator)
                                                .word_set()
                                            {
                                                if game_lock.ready_players.insert(name.clone()) {
                                                    response_sender
                                                        .send(protocol::Response::ReadyResult {
                                                            excuse: None,
                                                        })
                                                        .unwrap();
                                                    for client in game_lock.connections.values() {
                                                        client
                                                            .send(protocol::Response::PlayerReady {
                                                                name: name.clone(),
                                                            })
                                                            .unwrap();
                                                    }

                                                    let player_count = game_lock
                                                        .client_states
                                                        .values()
                                                        .filter(|cs| cs.is_player())
                                                        .count();

                                                    if game_lock.ready_players.len() == player_count
                                                    {
                                                        for player in game_lock.ready_players.iter()
                                                        {
                                                            if let Some(cl_sender) =
                                                                game_lock.connections.get(player)
                                                            {
                                                                let word_map = HashMap::from_iter(
                                                                    game_lock
                                                                        .client_states
                                                                        .iter()
                                                                        .filter(|(name, cs)| {
                                                                            cs.is_player()
                                                                                && *name != player
                                                                        })
                                                                        .map(|(name, cs)| {
                                                                            (
                                                                                name.clone(),
                                                                                cs.get_word()
                                                                                    .unwrap(),
                                                                            )
                                                                        }),
                                                                );

                                                                cl_sender.send(
                                                                protocol::Response::GameStart { assigned: word_map },
                                                                ).unwrap();
                                                                cl_sender.send(
                                                                    protocol::Response::TimerReset {
                                                                        timer: TIMEOUT_DURATION,
                                                                    },
                                                                ).unwrap();
                                                            }
                                                        }

                                                        game_lock.state =
                                                            protocol::GameState::Fighting;
                                                        let timer_game = session_game.clone();
                                                        info!("Timer reset");
                                                        if let Some(ref timer_handle) =
                                                            &timer_handle
                                                        {
                                                            timer_handle.abort();
                                                        }
                                                        *timer_handle = Some(tokio::spawn(
                                                            async move {
                                                                sleep(TIMEOUT_DURATION).await;
                                                                let mut game_lock =
                                                                    timer_game.lock().unwrap();
                                                                for client in
                                                                    game_lock.connections.values()
                                                                {
                                                                    client.send(protocol::Response::GameEndTimeout).unwrap();
                                                                }
                                                                game_lock.renew_inherit_clients();
                                                            },
                                                        ));
                                                    }
                                                } else {
                                                    response_sender
                                                        .send(protocol::Response::ReadyResult {
                                                            excuse: Some(
                                                                "您已经准备过了".to_owned(),
                                                            ),
                                                        })
                                                        .unwrap();
                                                }
                                            } else {
                                                response_sender
                                                    .send(protocol::Response::ReadyResult {
                                                        excuse: Some("您还未指定词语".to_owned()),
                                                    })
                                                    .unwrap();
                                            }
                                        }
                                    }
                                    protocol::GameState::Fighting => (),
                                }
                            }
                        }
                        protocol::Actions::CancelReady => {
                            if let Some(ref name) = session_name {
                                let mut game_lock = session_game.lock().unwrap();
                                response_sender
                                    .send(protocol::Response::ReadyResult { excuse: None })
                                    .unwrap();

                                if game_lock.ready_players.remove(name) {
                                    for client in game_lock.connections.values() {
                                        client
                                            .send(protocol::Response::PlayerNotReady {
                                                name: name.clone(),
                                            })
                                            .unwrap();
                                    }
                                }
                            }
                        }
                    }
                } else {
                    warn!("Invalid message from: {}", session_addr);
                }
            }
            _ => warn!("Invalid message from: {}", session_addr),
        },
        Err(err) => match err {
            tungstenite::Error::ConnectionClosed
            | tungstenite::Error::AlreadyClosed
            | tungstenite::Error::Io(_)
            | tungstenite::Error::AttackAttempt => return false,
            _ => error!("{}", err.to_string()),
        },
    }
    true
}

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let server = TcpListener::bind("0.0.0.0:1453").await.unwrap();
    let game = Arc::new(Mutex::new(Game::default()));

    while let Ok((stream, session_addr)) = server.accept().await {
        let session_game = game.clone();
        tokio::spawn(async move {
            let websocket = accept_async(stream).await.unwrap();
            let (mut write, read) = websocket.split();
            let (response_sender, response_receiver) = unbounded_channel();
            let mut action_receiver_stream = read.fuse();
            let mut response_receiver_stream =
                UnboundedReceiverStream::new(response_receiver).fuse();
            let mut timer_handle = None;
            let mut session_name = None;
            let mut heartbeat_interval =
                IntervalStream::new(tokio::time::interval(Duration::from_secs(1))).fuse();
            loop {
                select! {
                    message = action_receiver_stream.select_next_some() => {
                        if !process_message(message, &response_sender, &session_game, session_addr, &mut session_name, &mut timer_handle).await {
                            break;
                        }
                    }
                    response = response_receiver_stream.select_next_some() => {
                        match write.send(protocol_to_websocket_message(response)).await {
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    }
                    _ = heartbeat_interval.select_next_some() => {
                        match write.send(Message::Ping(vec![])).await {
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    }
                }
            }
            info!("Closing session: {}", session_addr);
            if let Some(name) = session_name {
                let mut game_lock = session_game.lock().unwrap();
                let client_state = game_lock.client_states.remove(&name).unwrap();
                game_lock.connections.remove(&name);
                game_lock.ready_players.remove(&name);
                if let Some(ref mut voting_session) = game_lock.voting_session {
                    voting_session.agreed.remove(&name);
                    voting_session.objection.remove(&name);
                    let agree_count = voting_session.agreed.len();
                    let objection_count = voting_session.objection.len();

                    let player_count = game_lock
                        .client_states
                        .values()
                        .filter(|cs| cs.is_player())
                        .count();

                    if player_count == agree_count + objection_count {
                        if agree_count > objection_count {
                            for client in game_lock.connections.values() {
                                client
                                    .send(protocol::Response::VoteAbortResult { abort: true })
                                    .unwrap();
                            }

                            game_lock.renew_inherit_clients();
                            if let Some(timer_handle) = timer_handle.take() {
                                timer_handle.abort();
                            }
                        } else {
                            for client in game_lock.connections.values() {
                                client
                                    .send(protocol::Response::VoteAbortResult { abort: false })
                                    .unwrap();
                            }
                            game_lock.voting_session = None;
                        }
                    }
                }
                if game_lock.state == protocol::GameState::Fighting {
                    if let protocol::ClientState::Player(protocol::PlayerState::Alive, Some(_)) =
                        client_state
                    {
                        game_lock
                            .message_history
                            .push(("".to_owned(), format!("玩家 {} 因为退出游戏而出局！", name)));
                    }
                }
                for client in game_lock.connections.values() {
                    if game_lock.state == protocol::GameState::Fighting {
                        if let protocol::ClientState::Player(
                            protocol::PlayerState::Alive,
                            Some(ref word),
                        ) = client_state
                        {
                            client
                                .send(protocol::Response::PlayerOut {
                                    quitter: name.clone(),
                                    word: word.clone(),
                                    suicide: false,
                                })
                                .unwrap();
                            client
                                .send(protocol::Response::MessageFrom {
                                    sender: "".to_owned(),
                                    content: format!("玩家 {} 因为退出游戏而出局！", name),
                                })
                                .unwrap();
                        }
                    }
                    client
                        .send(protocol::Response::PlayerQuit { name: name.clone() })
                        .unwrap();
                }
                if game_lock.state == protocol::GameState::Fighting {
                    if game_lock.ready_players.len() <= 1 {
                        let winner = game_lock.ready_players.iter().next().unwrap();

                        let winner_word = game_lock
                            .client_states
                            .get(winner)
                            .unwrap_or(&protocol::ClientState::Player(
                                protocol::PlayerState::Alive,
                                Some("".to_owned()),
                            ))
                            .get_word()
                            .unwrap();

                        for client in game_lock.connections.values() {
                            client
                                .send(protocol::Response::GameWin {
                                    winner: winner.clone(),
                                    word: winner_word.clone(),
                                })
                                .unwrap();
                        }

                        game_lock.renew_inherit_clients();
                        if let Some(timer_handle) = timer_handle {
                            timer_handle.abort();
                        }
                    }
                } else if game_lock.state == protocol::GameState::Assigning {
                    if client_state.is_player() {
                        if let Some(assignee) = game_lock.assignment_map.get(&name) {
                            if let Some(assignee) = game_lock.client_states.get(assignee) {
                                if assignee.get_word().is_none() {
                                    for client in game_lock.connections.values() {
                                        client
                                            .send(protocol::Response::GameEndUnproceedable)
                                            .unwrap()
                                    }
                                    game_lock.renew_inherit_clients();
                                    if let Some(timer_handle) = timer_handle {
                                        timer_handle.abort();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = write.close().await;
        });
    }
}
