use std::sync::Once;

/// Each unit-test process explicitly selects its own TLS provider.
pub(crate) fn init_tls_provider() {
    static INITIALIZED: Once = Once::new();
    INITIALIZED.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("unit tests must select their TLS provider before creating clients");
    });
}
