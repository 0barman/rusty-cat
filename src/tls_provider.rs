use crate::error::{InnerErrorCode, MeowError};

/// Reqwest's provider-free Rustls backend panics in `build()` when the process
/// has no provider. Check before entering that path without choosing or
/// installing a provider on the application's behalf.
pub(crate) fn ensure_crypto_provider() -> Result<(), MeowError> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }

    let message = "no Rustls crypto provider is installed; install your application's chosen \
        provider with CryptoProvider::install_default() before creating an internal HTTP client";
    crate::meow_error_log!("http_client", "{}", message);
    Err(MeowError::from_code_str(
        InnerErrorCode::HttpClientBuildFailed,
        message,
    ))
}
