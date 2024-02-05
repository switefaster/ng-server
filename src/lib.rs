pub mod protocol {
    use std::{collections::HashMap, time::Duration};

    #[derive(Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum GameState {
        #[default]
        Joining,
        Assigning,
        Fighting,
    }

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    pub enum ClientState {
        Player(PlayerState, Option<String>),
        Spectator,
    }

    impl ClientState {
        pub fn is_player(&self) -> bool {
            match self {
                ClientState::Player(_, _) => true,
                ClientState::Spectator => false,
            }
        }

        pub fn get_word(&self) -> Option<String> {
            match self {
                ClientState::Player(_, word) => word.clone(),
                ClientState::Spectator => None,
            }
        }

        pub fn set_word(&mut self, new_word: String) {
            match self {
                ClientState::Player(_, word) => *word = Some(new_word),
                ClientState::Spectator => (),
            }
        }

        pub fn word_set(&self) -> bool {
            match self {
                ClientState::Player(_, Some(_)) => true,
                _ => false,
            }
        }

        pub fn set_out(&mut self) -> bool {
            match self {
                ClientState::Player(state, _) => state.set_out(),
                ClientState::Spectator => false,
            }
        }
    }

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    pub enum PlayerState {
        Alive,
        Out,
    }

    impl PlayerState {
        pub fn set_out(&mut self) -> bool {
            match self {
                PlayerState::Alive => {
                    *self = Self::Out;
                    true
                }
                PlayerState::Out => false,
            }
        }
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    pub enum Actions {
        Login { name: String },
        Send(String),
        Suicide,
        RequestAbort,
        VoteAbort { abort: bool },
        AssignWord { word: String },
        SetReady,
        CancelReady,
    }

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    pub enum Response {
        LoginResult {
            excuse: Option<String>,
        },
        MessageFrom {
            sender: String,
            content: String,
        },
        AssignResult {
            #[serde(skip_serializing_if = "Option::is_none")]
            excuse: Option<String>,
        },
        PlayerOut {
            quitter: String,
            word: String,
            suicide: bool,
        },
        PlayerReady {
            name: String,
        },
        ReadyResult {
            #[serde(skip_serializing_if = "Option::is_none")]
            excuse: Option<String>,
        },
        PlayerNotReady {
            name: String,
        },
        TimerReset {
            timer: Duration,
        },
        GameWin {
            winner: String,
            word: String,
        },
        GameEndTimeout,
        GameEndUnproceedable,
        StartVoteAbort,
        VotedAbort {
            abort: bool,
            voter: String,
        },
        VoteAbortResult {
            abort: bool,
        },
        AssignStart {
            assignee: String,
        },
        GameStart {
            assigned: HashMap<String, String>,
        },
        Overview {
            clients: HashMap<String, ClientState>,
            game_state: GameState,
        },
        MessageHistory {
            history: Vec<(String, String)>,
        },
        PlayerJoin {
            name: String,
        },
        PlayerQuit {
            name: String,
        },
    }
}
