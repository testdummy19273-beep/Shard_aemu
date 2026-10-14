//! The app state machine. The UI renders from this state only; transitions are a pure function.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppState {
    NotSetUp,
    SettingUp,
    Stopped,
    Booting,
    Running,
    Stopping,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    SetupStarted,
    SetupDone,
    /// Setup cancelled or failed before completing; `set_up` says whether an older
    /// complete install is still usable.
    SetupAborted {
        set_up: bool,
    },
    BootStarted,
    Booted,
    /// Found an emulator from this folder already running (Shard was closed and left it on).
    Adopted,
    StopStarted,
    Stopped,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{input:?} is not valid in state {state:?}")]
pub struct InvalidTransition {
    pub state: AppState,
    pub input: Input,
}

impl AppState {
    pub fn apply(&self, input: &Input) -> Result<AppState, InvalidTransition> {
        use AppState as S;
        use Input as I;
        let next = match (self, input) {
            (_, I::Failed(msg)) => S::Error(msg.clone()),
            (S::NotSetUp | S::Stopped | S::Error(_), I::SetupStarted) => S::SettingUp,
            (S::SettingUp, I::SetupDone) => S::Stopped,
            (S::SettingUp, I::SetupAborted { set_up: true }) => S::Stopped,
            (S::SettingUp, I::SetupAborted { set_up: false }) => S::NotSetUp,
            (S::Stopped | S::Error(_), I::BootStarted) => S::Booting,
            (S::Booting, I::Booted) => S::Running,
            (S::Stopped | S::Error(_) | S::NotSetUp, I::Adopted) => S::Running,
            (S::Booting | S::Running | S::Error(_), I::StopStarted) => S::Stopping,
            (S::Stopping, I::Stopped) => S::Stopped,
            // The emulator went away on its own (window closed) while running.
            (S::Running, I::Stopped) => S::Stopped,
            _ => {
                return Err(InvalidTransition {
                    state: self.clone(),
                    input: input.clone(),
                })
            }
        };
        Ok(next)
    }

    pub fn label(&self) -> &str {
        match self {
            AppState::NotSetUp => "Not set up",
            AppState::SettingUp => "Setting up",
            AppState::Stopped => "Stopped",
            AppState::Booting => "Booting",
            AppState::Running => "Running",
            AppState::Stopping => "Stopping",
            AppState::Error(_) => "Error",
        }
    }
}
