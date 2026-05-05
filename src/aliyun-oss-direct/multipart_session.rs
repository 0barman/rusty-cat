#[derive(Debug, Default)]
pub(crate) struct MultipartSession {
    pub(crate) target_url: Option<String>,
    pub(crate) upload_id: Option<String>,
}
