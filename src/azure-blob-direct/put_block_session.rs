use std::collections::BTreeSet;

#[derive(Debug, Default)]
pub(crate) struct PutBlockSession {
    pub(crate) target_url: Option<String>,
    pub(crate) uploaded_blocks: BTreeSet<usize>,
}
