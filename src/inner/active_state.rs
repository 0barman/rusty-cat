use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub(crate) struct ActiveState {
    cancel: CancellationToken,
}

impl ActiveState {
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        Self { cancel }
    }

    pub(crate) fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
}
