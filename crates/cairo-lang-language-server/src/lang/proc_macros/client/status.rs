use std::sync::Arc;

use super::ProcMacroClient;

#[derive(Debug, Default, Clone)]
pub enum ClientStatus {
    #[default]
    Pending,
    Starting(Arc<ProcMacroClient>),
    Ready(Arc<ProcMacroClient>),
    /// Failed to start multiple times.
    /// No more actions will be taken.
    Crashed,
}

impl ClientStatus {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}