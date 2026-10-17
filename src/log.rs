//! Flow-level debug logging utilities.
//!
//! At most one global listener can be registered. When no listener is present,
//! `emit`/`emit_lazy` return quickly with minimal overhead. Listener panics are
//! isolated with `catch_unwind` so SDK internal logic remains safe.
//!
//! # Log levels and persistence
//!
//! Consumers register a single listener via [`set_debug_log_listener`] and
//! receive every [`Log`] the SDK emits. Each entry carries a [`LogLevel`] that
//! tells the consumer how to treat it. Use [`LogLevel::persist_recommended`] to
//! decide what to keep:
//!
//! | Level | Meaning | Persist? |
//! |-------|---------|----------|
//! | [`LogLevel::Trace`] | per-chunk / per-poll / per-retry hot-loop noise | **No** — drop or sample only |
//! | [`LogLevel::Debug`] | low-volume diagnostics | optional / short-term only |
//! | [`LogLevel::Info`] | normal operational information | optional |
//! | [`LogLevel::Key`] | key-node checkpoint for troubleshooting | **Yes** — keep and forward to the SDK author |
//! | [`LogLevel::Warn`] | recoverable anomaly / misuse / backpressure | **Yes** |
//! | [`LogLevel::Error`] | failure with full detail, or a caught panic | **Yes** |
//!
//! The author-forwardable troubleshooting stream is everything at
//! `>= LogLevel::Key`, i.e. `Key | Warn | Error`. Never persist a high-frequency
//! `Trace` entry, and never persist any URL or task dump that has not been run
//! through [`sanitize_url`].

use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Log severity level for flow debug entries.
///
/// Ordered from least to most important: `Trace < Debug < Info < Key < Warn <
/// Error`. Consumers can filter the author-forwardable stream with
/// `level >= LogLevel::Key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    /// Very high frequency, per-chunk / per-poll / per-retry hot-loop noise.
    /// Consumers MUST NOT persist these (drop or sample only).
    Trace,
    /// Low-volume diagnostics. Persisting is optional / short-term only.
    Debug,
    /// Normal operational information. Persisting is optional.
    Info,
    /// Key-node checkpoint the SDK author reviews when a consumer reports a bug.
    /// Consumers SHOULD persist these and forward them for troubleshooting.
    Key,
    /// Recoverable anomaly, misuse, or backpressure. Consumers SHOULD persist.
    Warn,
    /// Failure with full detail — a chunk/part upload or download failed, a task
    /// failed, an HTTP error response, a signing failure — or a caught panic.
    /// Consumers SHOULD persist these together with their full context.
    Error,
}

impl LogLevel {
    /// Returns whether consumers are recommended to persist entries at this
    /// level. True for [`LogLevel::Key`], [`LogLevel::Warn`] and
    /// [`LogLevel::Error`]; false for the higher-frequency / lower-value
    /// [`LogLevel::Trace`], [`LogLevel::Debug`] and [`LogLevel::Info`].
    ///
    /// # Examples
    ///
    /// ```
    /// use rusty_cat::api::LogLevel;
    ///
    /// assert!(LogLevel::Error.persist_recommended());
    /// assert!(LogLevel::Key.persist_recommended());
    /// assert!(!LogLevel::Trace.persist_recommended());
    /// ```
    #[inline]
    pub fn persist_recommended(&self) -> bool {
        matches!(self, LogLevel::Key | LogLevel::Warn | LogLevel::Error)
    }

    /// Returns the static upper-case label for this level.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusty_cat::api::LogLevel;
    ///
    /// assert_eq!(LogLevel::Error.as_str(), "ERROR");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Key => "KEY",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One structured log record that can be printed or persisted externally.
///
/// Besides the human-readable [`Log::message`], an entry may carry optional
/// structured context for triage — task id, object key, part index, byte
/// offset/length, HTTP status, retry attempt and a sanitized URL. These are set
/// through the `with_*` builder methods and are most valuable on
/// [`LogLevel::Error`] and [`LogLevel::Key`] entries. Its [`std::fmt::Display`]
/// implementation appends
/// every present field as ` key=value`, so a persisted log line is
/// self-describing.
#[derive(Debug, Clone)]
pub struct Log {
    /// Unix epoch timestamp in milliseconds.
    timestamp_ms: u64,
    /// Log severity level.
    level: LogLevel,
    /// Static tag such as `"meow_client"` or `"enqueue"` for filtering.
    tag: &'static str,
    /// Human-readable message content.
    message: String,
    /// Owning task id, when known.
    task_id: Option<String>,
    /// Object / blob key being transferred, when known.
    object_key: Option<String>,
    /// Zero-based part or chunk index, when known.
    part_index: Option<u64>,
    /// Byte offset within the file, when known.
    offset: Option<u64>,
    /// Byte length of the chunk/range, when known.
    byte_len: Option<u64>,
    /// HTTP status code associated with a failure, when known.
    http_status: Option<u16>,
    /// Retry attempt number, when known.
    attempt: Option<u32>,
    /// Maximum retry attempts configured, when known.
    max_retries: Option<u32>,
    /// SDK error code, when known.
    error_code: Option<i32>,
    /// URL associated with the entry — ALWAYS sanitized via [`sanitize_url`].
    url: Option<String>,
}

impl Log {
    /// Creates a log entry with explicit level and tag (no structured context).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{Log, LogLevel};
    ///
    /// let log = Log::new(LogLevel::Info, "demo", "hello");
    /// assert_eq!(log.level(), LogLevel::Info);
    /// ```
    pub fn new(level: LogLevel, tag: &'static str, message: impl Into<String>) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            timestamp_ms,
            level,
            tag,
            message: message.into(),
            task_id: None,
            object_key: None,
            part_index: None,
            offset: None,
            byte_len: None,
            http_status: None,
            attempt: None,
            max_retries: None,
            error_code: None,
            url: None,
        }
    }

    /// Creates a [`LogLevel::Trace`] entry (high-frequency; do not persist).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::trace("download_chunk", "wrote chunk");
    /// assert_eq!(log.tag(), "download_chunk");
    /// ```
    pub fn trace(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Trace, tag, message)
    }

    /// Creates a [`LogLevel::Debug`] entry.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "debug message");
    /// assert_eq!(log.tag(), "demo");
    /// ```
    pub fn debug(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Debug, tag, message)
    }

    /// Creates a [`LogLevel::Info`] entry.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::info("demo", "info message");
    /// assert_eq!(log.tag(), "demo");
    /// ```
    pub fn info(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Info, tag, message)
    }

    /// Creates a [`LogLevel::Key`] entry (key-node checkpoint; persist & forward).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::key("enqueue", "task enqueued");
    /// assert_eq!(log.tag(), "enqueue");
    /// ```
    pub fn key(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Key, tag, message)
    }

    /// Creates a [`LogLevel::Warn`] entry.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::warn("cleanup", "abort failed");
    /// assert_eq!(log.tag(), "cleanup");
    /// ```
    pub fn warn(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Warn, tag, message)
    }

    /// Creates a [`LogLevel::Error`] entry (failure; persist with full context).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::error("upload_part", "part upload failed").with_part(3);
    /// assert_eq!(log.part_index(), Some(3));
    /// ```
    pub fn error(tag: &'static str, message: impl Into<String>) -> Self {
        Self::new(LogLevel::Error, tag, message)
    }

    /// Attaches the owning task id.
    #[must_use]
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    /// Attaches the object / blob key being transferred.
    #[must_use]
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.object_key = Some(key.into());
        self
    }

    /// Attaches the zero-based part / chunk index.
    #[must_use]
    pub fn with_part(mut self, part_index: u64) -> Self {
        self.part_index = Some(part_index);
        self
    }

    /// Attaches the byte offset within the file.
    #[must_use]
    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Attaches the byte length of the chunk / range.
    #[must_use]
    pub fn with_byte_len(mut self, byte_len: u64) -> Self {
        self.byte_len = Some(byte_len);
        self
    }

    /// Attaches both the byte offset and the byte length of a range.
    #[must_use]
    pub fn with_range(mut self, offset: u64, byte_len: u64) -> Self {
        self.offset = Some(offset);
        self.byte_len = Some(byte_len);
        self
    }

    /// Attaches the HTTP status code associated with a failure.
    #[must_use]
    pub fn with_http_status(mut self, status: u16) -> Self {
        self.http_status = Some(status);
        self
    }

    /// Attaches the retry attempt number.
    #[must_use]
    pub fn with_attempt(mut self, attempt: u32) -> Self {
        self.attempt = Some(attempt);
        self
    }

    /// Attaches the configured maximum number of retry attempts.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Attaches the SDK error code.
    #[must_use]
    pub fn with_error_code(mut self, code: i32) -> Self {
        self.error_code = Some(code);
        self
    }

    /// Attaches a URL, automatically passing it through [`sanitize_url`] so that
    /// SAS tokens, signatures and credentials never reach a persisted log.
    #[must_use]
    pub fn with_url(mut self, url: impl AsRef<str>) -> Self {
        self.url = Some(sanitize_url(url.as_ref()));
        self
    }

    /// Returns timestamp in milliseconds.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "message");
    /// let _ts = log.timestamp_ms();
    /// ```
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Returns log level.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::{Log, LogLevel};
    ///
    /// let log = Log::new(LogLevel::Warn, "demo", "warn");
    /// assert_eq!(log.level(), LogLevel::Warn);
    /// ```
    pub fn level(&self) -> LogLevel {
        self.level
    }

    /// Returns static tag.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("network", "retry");
    /// assert_eq!(log.tag(), "network");
    /// ```
    pub fn tag(&self) -> &'static str {
        self.tag
    }

    /// Returns message by shared reference.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "hello");
    /// assert_eq!(log.message(), "hello");
    /// ```
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the owning task id, if set.
    pub fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    /// Returns the object / blob key, if set.
    pub fn object_key(&self) -> Option<&str> {
        self.object_key.as_deref()
    }

    /// Returns the part / chunk index, if set.
    pub fn part_index(&self) -> Option<u64> {
        self.part_index
    }

    /// Returns the byte offset, if set.
    pub fn offset(&self) -> Option<u64> {
        self.offset
    }

    /// Returns the byte length, if set.
    pub fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }

    /// Returns the HTTP status code, if set.
    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Returns the retry attempt number, if set.
    pub fn attempt(&self) -> Option<u32> {
        self.attempt
    }

    /// Returns the configured maximum retry attempts, if set.
    pub fn max_retries(&self) -> Option<u32> {
        self.max_retries
    }

    /// Returns the SDK error code, if set.
    pub fn error_code(&self) -> Option<i32> {
        self.error_code
    }

    /// Returns the sanitized URL, if set.
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// Consumes the entry and returns owned message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use rusty_cat::api::Log;
    ///
    /// let log = Log::debug("demo", "hello");
    /// let msg = log.into_message();
    /// assert_eq!(msg, "hello");
    /// ```
    pub fn into_message(self) -> String {
        self.message
    }
}

impl fmt::Display for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {} [{}] {}",
            self.timestamp_ms, self.level, self.tag, self.message
        )?;
        if let Some(v) = &self.task_id {
            write!(f, " task_id={v}")?;
        }
        if let Some(v) = &self.object_key {
            write!(f, " key={v}")?;
        }
        if let Some(v) = self.part_index {
            write!(f, " part={v}")?;
        }
        if let Some(v) = self.offset {
            write!(f, " offset={v}")?;
        }
        if let Some(v) = self.byte_len {
            write!(f, " len={v}")?;
        }
        if let Some(v) = self.http_status {
            write!(f, " http_status={v}")?;
        }
        if let Some(v) = self.attempt {
            write!(f, " attempt={v}")?;
        }
        if let Some(v) = self.max_retries {
            write!(f, " max_retries={v}")?;
        }
        if let Some(v) = self.error_code {
            write!(f, " error_code={v}")?;
        }
        if let Some(v) = &self.url {
            write!(f, " url={v}")?;
        }
        Ok(())
    }
}

/// Returns `true` when a URL query parameter name carries a secret (signature,
/// credential, security token, access key) that must never be logged.
fn is_sensitive_param(key: &str) -> bool {
    let k = key.trim().to_ascii_lowercase();
    const EXACT: &[&str] = &[
        "sig",
        "signature",
        "authorization",
        "x-oss-signature",
        "ossaccesskeyid",
        "awsaccesskeyid",
        "googleaccessid",
        "x-oss-credential",
        "x-oss-security-token",
        "security-token",
        "x-amz-signature",
        "x-amz-credential",
        "x-amz-security-token",
    ];
    if EXACT.contains(&k.as_str()) {
        return true;
    }
    if [
        "accesstoken",
        "refreshtoken",
        "sessiontoken",
        "securitytoken",
        "clientsecret",
        "accesskey",
        "accesskeyid",
        "secretaccesskey",
        "apikey",
    ]
    .contains(&k.as_str())
    {
        return true;
    }
    const COMPACT_SUFFIXES: &[&str] = &[
        "sig",
        "signature",
        "credential",
        "token",
        "secret",
        "password",
        "accesskey",
        "apikey",
    ];
    if COMPACT_SUFFIXES
        .iter()
        .any(|suffix| k.len() > suffix.len() && k.ends_with(suffix))
    {
        return true;
    }
    k.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|component| {
            matches!(
                component,
                "sig" | "signature" | "credential" | "token" | "secret" | "password" | "accesskey"
            )
        })
}

fn decode_query_key(raw: &str) -> Option<String> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let input = raw.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut at = 0usize;
    while at < input.len() {
        match input[at] {
            b'%' => {
                let high = *input.get(at + 1)?;
                let low = *input.get(at + 2)?;
                decoded.push(hex(high)?.checked_mul(16)?.checked_add(hex(low)?)?);
                at += 3;
            }
            b'+' => {
                decoded.push(b' ');
                at += 1;
            }
            byte => {
                decoded.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn is_sensitive_raw_query_key(raw: &str) -> bool {
    if is_sensitive_param(raw) {
        return true;
    }
    let Some(decoded_once) = decode_query_key(raw) else {
        return false;
    };
    is_sensitive_param(&decoded_once)
        || decode_query_key(&decoded_once).is_some_and(|decoded| is_sensitive_param(&decoded))
}

fn redact_url_userinfo(base: &str) -> String {
    let authority_start = base
        .find("://")
        .map(|at| at + 3)
        .or_else(|| base.starts_with("//").then_some(2));
    let Some(authority_start) = authority_start else {
        return base.to_owned();
    };
    let authority_end = base[authority_start..]
        .find('/')
        .map(|relative| authority_start + relative)
        .unwrap_or(base.len());
    let Some(userinfo_end) = base[authority_start..authority_end].rfind('@') else {
        return base.to_owned();
    };
    let userinfo_end = authority_start + userinfo_end;
    let mut safe = String::with_capacity(base.len());
    safe.push_str(&base[..authority_start]);
    safe.push_str("REDACTED");
    safe.push_str(&base[userinfo_end..]);
    safe
}

/// Redacts secret query parameters from a URL so it is safe to log or persist.
///
/// Non-secret path, query, and fragment text (for example a SAS `se` expiry or
/// `sp` permissions, the Aliyun `Expires`, an OSS `partNumber`) is kept intact;
/// userinfo and the *values* of signature / credential / security-token /
/// access-key assignments are replaced with `REDACTED`.
///
/// # Examples
///
/// ```
/// use rusty_cat::api::sanitize_url;
///
/// let safe = sanitize_url("https://x.blob.core.windows.net/c/b?sv=2021&se=2030-01-01&sig=AbC%2Bsecret");
/// assert!(safe.contains("sv=2021"));
/// assert!(safe.contains("se=2030-01-01"));
/// assert!(safe.contains("sig=REDACTED"));
/// assert!(!safe.contains("AbC"));
/// ```
pub fn sanitize_url(url: &str) -> String {
    let (without_fragment, fragment) = url
        .split_once('#')
        .map_or((url, None), |(base, fragment)| (base, Some(fragment)));
    let (base, query) = match without_fragment.split_once('?') {
        Some(parts) => parts,
        None => {
            let mut out = redact_secrets(&redact_url_userinfo(without_fragment));
            if let Some(fragment) = fragment {
                out.push('#');
                out.push_str(&redact_secrets(fragment));
            }
            return out;
        }
    };
    let safe_base = redact_secrets(&redact_url_userinfo(base));
    let mut out = String::with_capacity(url.len());
    out.push_str(&safe_base);
    out.push('?');
    let mut first = true;
    for pair in query.split('&') {
        if !first {
            out.push('&');
        }
        first = false;
        match pair.split_once('=') {
            Some((k, _)) if is_sensitive_raw_query_key(k) => {
                out.push_str(k);
                out.push_str("=REDACTED");
            }
            None if is_sensitive_raw_query_key(pair) => {
                out.push_str(pair);
                out.push_str("=REDACTED");
            }
            _ => out.push_str(pair),
        }
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(&redact_secrets(fragment));
    }
    out
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_assignment_key_char(c: char) -> bool {
    is_token_char(c) || c == '%'
}

fn is_unquoted_value_delim(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '&' | '"' | '\'' | ',' | ';' | ')' | '}' | ']' | '<' | '>' | '|'
        )
}

fn is_authorization_value_delim(c: char) -> bool {
    matches!(c, '\r' | '\n' | ';' | ')' | '}' | ']' | '<' | '>' | '|')
}

fn percent_assignment_separator_len(bytes: &[u8], at: usize) -> Option<usize> {
    for separator in [b"%253D".as_slice(), b"%253A", b"%3D", b"%3A"] {
        let end = at.checked_add(separator.len())?;
        if bytes
            .get(at..end)
            .is_some_and(|actual| actual.eq_ignore_ascii_case(separator))
        {
            return Some(separator.len());
        }
    }
    None
}

fn percent_value_delimiter_len(bytes: &[u8], at: usize) -> Option<usize> {
    for delimiter in [
        b"%2526".as_slice(),
        b"%253B",
        b"%252C",
        b"%26",
        b"%3B",
        b"%2C",
    ] {
        let end = at.checked_add(delimiter.len())?;
        if bytes
            .get(at..end)
            .is_some_and(|actual| actual.eq_ignore_ascii_case(delimiter))
        {
            return Some(delimiter.len());
        }
    }
    None
}

fn redact_percent_encoded_assignments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied_until = 0usize;
    let mut at = 0usize;
    while at < bytes.len() {
        let Some(separator_len) = percent_assignment_separator_len(bytes, at) else {
            at += 1;
            continue;
        };
        let mut key_start = at;
        while key_start > 0
            && bytes[key_start - 1].is_ascii()
            && (bytes[key_start - 1].is_ascii_alphanumeric()
                || matches!(bytes[key_start - 1], b'_' | b'-' | b'.' | b'%'))
        {
            key_start -= 1;
        }
        let candidate = &text[key_start..at];
        let candidate_lower = candidate.to_ascii_lowercase();
        let mut encoded_boundary = 0usize;
        for delimiter in ["%2526", "%253b", "%252f", "%253f", "%2523", "%26", "%3b"] {
            if let Some(relative) = candidate_lower.rfind(delimiter) {
                encoded_boundary = encoded_boundary.max(relative + delimiter.len());
            }
        }
        key_start += encoded_boundary;
        let Some(raw_key) = text.get(key_start..at) else {
            at += separator_len;
            continue;
        };
        if !is_sensitive_raw_query_key(raw_key) {
            at += separator_len;
            continue;
        }

        let value_start = at + separator_len;
        let mut value_end = value_start;
        while value_end < bytes.len() {
            if bytes[value_end].is_ascii_whitespace()
                || matches!(
                    bytes[value_end],
                    b'&' | b'"' | b'\'' | b',' | b';' | b')' | b'}' | b']' | b'<' | b'>' | b'|'
                )
                || percent_value_delimiter_len(bytes, value_end).is_some()
            {
                break;
            }
            value_end += 1;
        }
        output.push_str(&text[copied_until..value_start]);
        output.push_str("REDACTED");
        copied_until = value_end;
        at = value_end.max(value_start);
    }
    output.push_str(&text[copied_until..]);
    output
}

fn redact_xml_secret_elements(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied_until = 0usize;
    let mut search_from = 0usize;
    while let Some(relative_open) = text[search_from..].find('<') {
        let open = search_from + relative_open;
        let name_start = open + 1;
        let Some(first) = text.as_bytes().get(name_start).copied() else {
            break;
        };
        if matches!(first, b'/' | b'!' | b'?') {
            search_from = name_start;
            continue;
        }
        let mut name_end = name_start;
        while text.as_bytes().get(name_end).is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
        }) {
            name_end += 1;
        }
        let Some(tag_name) = text.get(name_start..name_end) else {
            search_from = name_start;
            continue;
        };
        let local_name = tag_name.rsplit(':').next().unwrap_or(tag_name);
        if !is_sensitive_param(local_name) {
            search_from = name_end.max(name_start);
            continue;
        }
        let Some(relative_open_end) = text[name_end..].find('>') else {
            break;
        };
        let open_end = name_end + relative_open_end;
        if text[..open_end].ends_with('/') {
            search_from = open_end + 1;
            continue;
        }
        let closing = format!("</{}>", tag_name.to_ascii_lowercase());
        let content_start = open_end + 1;
        let tail_lower = text[content_start..].to_ascii_lowercase();
        let Some(relative_close) = tail_lower.find(&closing) else {
            // Bounded provider diagnostics may end in the middle of a secret
            // element. Once its opening tag is recognised, the whole remaining
            // value is sensitive even if its closing tag was never received.
            output.push_str(&text[copied_until..content_start]);
            output.push_str("REDACTED");
            return output;
        };
        let close = content_start + relative_close;
        output.push_str(&text[copied_until..content_start]);
        output.push_str("REDACTED");
        copied_until = close;
        search_from = close + closing.len();
    }
    output.push_str(&text[copied_until..]);
    output
}

fn chars_eq_ignore_ascii_case(chars: &[char], expected: &str) -> bool {
    chars.len() == expected.len()
        && chars
            .iter()
            .copied()
            .zip(expected.chars())
            .all(|(actual, expected)| actual.eq_ignore_ascii_case(&expected))
}

fn push_chars(out: &mut String, chars: &[char]) {
    out.extend(chars.iter().copied());
}

fn skip_quoted_value(chars: &[char], mut at: usize, quote: char) -> (usize, bool) {
    let mut escaped = false;
    while at < chars.len() {
        let current = chars[at];
        if current == quote && !escaped {
            return (at, true);
        }
        if current == '\\' {
            escaped = !escaped;
        } else {
            escaped = false;
        }
        at += 1;
    }
    (at, false)
}

fn bearer_value_start(chars: &[char], at: usize) -> Option<usize> {
    const BEARER_LEN: usize = 6;
    let end = at.checked_add(BEARER_LEN)?;
    if end > chars.len()
        || (at > 0 && is_token_char(chars[at - 1]))
        || !chars_eq_ignore_ascii_case(&chars[at..end], "bearer")
    {
        return None;
    }

    let mut value_start = end;
    if chars.get(value_start).is_some_and(|c| c.is_whitespace()) {
        while chars.get(value_start).is_some_and(|c| c.is_whitespace()) {
            value_start += 1;
        }
    } else if chars
        .get(value_start..value_start.saturating_add(3))
        .is_some_and(|separator| chars_eq_ignore_ascii_case(separator, "%20"))
    {
        value_start += 3;
    } else {
        return None;
    }

    (value_start < chars.len()).then_some(value_start)
}

fn authorization_value_start(chars: &[char], at: usize) -> Option<usize> {
    const AUTHORIZATION_LEN: usize = 13;
    let end = at.checked_add(AUTHORIZATION_LEN)?;
    if end > chars.len()
        || (at > 0 && is_assignment_key_char(chars[at - 1]))
        || !chars_eq_ignore_ascii_case(&chars[at..end], "authorization")
    {
        return None;
    }

    let mut separator = end;
    if separator < chars.len()
        && matches!(chars[separator], '"' | '\'')
        && at > 0
        && chars[at - 1] == chars[separator]
    {
        separator += 1;
    }
    while separator < chars.len() && chars[separator].is_whitespace() {
        separator += 1;
    }
    if !chars
        .get(separator)
        .is_some_and(|separator| matches!(separator, ':' | '='))
    {
        return None;
    }

    let mut value_start = separator + 1;
    while value_start < chars.len() && chars[value_start].is_whitespace() {
        value_start += 1;
    }
    (value_start < chars.len()).then_some(value_start)
}

/// Redacts secret `key=value` pairs (signatures, credentials, security tokens,
/// access keys) from arbitrary free text — an error chain message, or a provider
/// response body that may echo a signed URL — so the text is safe to log.
///
/// The value of any sensitive key is replaced with `REDACTED`; all other text is
/// preserved verbatim. Use this on any provider body or error string before
/// putting it into a [`Log`] message. For a whole URL prefer [`sanitize_url`].
///
/// # Examples
///
/// ```
/// use rusty_cat::api::redact_secrets;
///
/// let safe = redact_secrets("error: GET https://h/o?sv=2021&sig=AbC%2Bsecret failed");
/// assert!(safe.contains("sv=2021"));
/// assert!(safe.contains("sig=REDACTED"));
/// assert!(!safe.contains("AbC"));
/// ```
pub fn redact_secrets(text: &str) -> String {
    let percent_safe = redact_percent_encoded_assignments(text);
    let xml_safe = redact_xml_secret_elements(&percent_safe);
    let chars: Vec<char> = xml_safe.chars().collect();
    let mut out = String::with_capacity(xml_safe.len());
    let mut at = 0usize;
    while at < chars.len() {
        if let Some(mut value_start) = authorization_value_start(&chars, at) {
            let quote = chars
                .get(value_start)
                .copied()
                .filter(|c| matches!(c, '"' | '\''));
            if quote.is_some() {
                value_start += 1;
            }

            push_chars(&mut out, &chars[at..value_start]);
            out.push_str("REDACTED");
            if let Some(quote) = quote {
                let (end, closed) = skip_quoted_value(&chars, value_start, quote);
                at = end;
                if closed {
                    out.push(quote);
                    at += 1;
                }
            } else {
                at = value_start;
                while at < chars.len() && !is_authorization_value_delim(chars[at]) {
                    at += 1;
                }
            }
            continue;
        }

        if let Some(value_start) = bearer_value_start(&chars, at) {
            push_chars(&mut out, &chars[at..value_start]);
            out.push_str("REDACTED");
            at = value_start;
            while at < chars.len() && !is_unquoted_value_delim(chars[at]) {
                at += 1;
            }
            continue;
        }

        if is_assignment_key_char(chars[at]) && (at == 0 || !is_assignment_key_char(chars[at - 1]))
        {
            let key_start = at;
            let mut key_end = key_start + 1;
            while key_end < chars.len() && is_assignment_key_char(chars[key_end]) {
                key_end += 1;
            }
            let key: String = chars[key_start..key_end].iter().collect();
            let mut separator = key_end;
            if separator < chars.len()
                && matches!(chars[separator], '"' | '\'')
                && key_start > 0
                && chars[key_start - 1] == chars[separator]
            {
                separator += 1;
            }
            while separator < chars.len() && chars[separator].is_whitespace() {
                separator += 1;
            }

            if is_sensitive_raw_query_key(&key)
                && separator < chars.len()
                && matches!(chars[separator], '=' | ':')
            {
                let mut value_start = separator + 1;
                while value_start < chars.len() && chars[value_start].is_whitespace() {
                    value_start += 1;
                }
                let quote = chars
                    .get(value_start)
                    .copied()
                    .filter(|c| matches!(c, '"' | '\''));
                if quote.is_some() {
                    value_start += 1;
                }

                push_chars(&mut out, &chars[key_start..value_start]);
                out.push_str("REDACTED");
                if let Some(quote) = quote {
                    let (end, closed) = skip_quoted_value(&chars, value_start, quote);
                    at = end;
                    if closed {
                        out.push(quote);
                        at += 1;
                    }
                } else {
                    at = value_start;
                    while at < chars.len() && !is_unquoted_value_delim(chars[at]) {
                        at += 1;
                    }
                }
                continue;
            }

            push_chars(&mut out, &chars[key_start..key_end]);
            at = key_end;
            continue;
        }

        out.push(chars[at]);
        at += 1;
    }
    out
}

/// Callback type for global debug log listener.
pub type DebugLogListener = Arc<dyn Fn(Log) + Send + Sync + 'static>;

static DEBUG_LOG_LISTENER: OnceLock<Mutex<Option<DebugLogListener>>> = OnceLock::new();

fn debug_log_listener_slot() -> &'static Mutex<Option<DebugLogListener>> {
    DEBUG_LOG_LISTENER.get_or_init(|| Mutex::new(None))
}

/// Returns whether a debug log listener is currently registered.
///
/// Useful on hot paths to avoid constructing log objects unnecessarily.
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::debug_log_listener_active;
///
/// let _active = debug_log_listener_active();
/// ```
#[inline]
pub fn debug_log_listener_active() -> bool {
    match debug_log_listener_slot().lock() {
        Ok(g) => g.is_some(),
        Err(_) => false,
    }
}

/// Sets or clears the global debug log listener.
///
/// - `Some(listener)`: set or replace current listener.
/// - `None`: clear listener (unregister).
///
/// # Errors
///
/// Returns [`DebugLogListenerError`] when internal listener storage lock is
/// poisoned.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use rusty_cat::api::{set_debug_log_listener, DebugLogListener, Log};
///
/// let listener: DebugLogListener = Arc::new(|log: Log| println!("{log}"));
/// set_debug_log_listener(Some(listener))?;
/// set_debug_log_listener(None)?;
/// # Ok::<(), rusty_cat::api::DebugLogListenerError>(())
/// ```
pub fn set_debug_log_listener(
    listener: Option<DebugLogListener>,
) -> Result<(), DebugLogListenerError> {
    let mut g = debug_log_listener_slot()
        .lock()
        .map_err(|_| DebugLogListenerError(()))?;
    *g = listener;
    Ok(())
}

/// Registers a global singleton debug log listener.
///
/// Returns `Err` if a listener is already registered.
///
/// # Errors
///
/// Returns [`DebugLogListenerError`] when:
/// - a listener is already registered, or
/// - internal listener storage lock is poisoned.
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::{try_set_debug_log_listener, Log};
///
/// let _ = try_set_debug_log_listener(|log: Log| {
///     println!("{log}");
/// });
/// ```
pub fn try_set_debug_log_listener<F>(f: F) -> Result<(), DebugLogListenerError>
where
    F: Fn(Log) + Send + Sync + 'static,
{
    let mut g = debug_log_listener_slot()
        .lock()
        .map_err(|_| DebugLogListenerError(()))?;
    if g.is_some() {
        return Err(DebugLogListenerError(()));
    }
    *g = Some(Arc::new(f));
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugLogListenerError(());

impl fmt::Display for DebugLogListenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("debug log listener already set")
    }
}

impl std::error::Error for DebugLogListenerError {}

/// Emits one log entry.
///
/// Returns immediately when no listener is set. Listener panics are caught and
/// discarded.
///
/// # Panics
///
/// This function does not panic. Listener panics are caught internally.
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::{emit, Log};
///
/// emit(Log::debug("demo", "manual emit"));
/// ```
pub fn emit(log: Log) {
    let cb_opt = debug_log_listener_slot()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(Arc::clone));
    let Some(cb) = cb_opt else {
        return;
    };
    let _ = panic::catch_unwind(AssertUnwindSafe(move || {
        cb(log);
    }));
}

/// Lazily emits a log entry only when listener is active.
///
/// This avoids formatting/allocation overhead when logging is disabled.
///
/// # Panics
///
/// This function does not panic. Any panic from listener callback is caught by
/// [`emit`].
///
/// # Examples
///
/// ```no_run
/// use rusty_cat::api::{emit_lazy, Log};
///
/// emit_lazy(|| Log::debug("demo", format!("computed {}", 42)));
/// ```
#[inline]
pub fn emit_lazy<F>(f: F)
where
    F: FnOnce() -> Log,
{
    if !debug_log_listener_active() {
        return;
    }
    emit(f());
}

/// Internal flow debug logging macro ([`LogLevel::Debug`]).
///
/// The `format!` expression is evaluated only when listener is active.
/// crate::meow_flow_log!(
///     "enqueue",
///    "task_id={:?} offset={} total={}",
///     task_id,
///     offset,
///    total
/// );
#[macro_export]
macro_rules! meow_flow_log {
    ($tag:expr, $($arg:tt)*) => {
        $crate::log::emit_lazy(|| {
            $crate::log::Log::debug($tag, format!($($arg)*))
        });
    };
}

/// Internal trace logging macro ([`LogLevel::Trace`]).
///
/// Same lazy evaluation semantics as [`meow_flow_log`], but emits at
/// [`LogLevel::Trace`]. Use it for VERY high frequency events that fire inside
/// hot loops — per chunk, per poll tick, per retry attempt. Consumers must not
/// persist these (drop or sample only).
///
/// # Examples
///
/// ```
/// rusty_cat::meow_trace_log!("download_chunk", "wrote {} bytes at {}", 1024, 0);
/// ```
#[macro_export]
macro_rules! meow_trace_log {
    ($tag:expr, $($arg:tt)*) => {
        $crate::log::emit_lazy(|| {
            $crate::log::Log::new($crate::log::LogLevel::Trace, $tag, format!($($arg)*))
        });
    };
}

/// Internal key-node logging macro ([`LogLevel::Key`]).
///
/// Same lazy evaluation semantics as [`meow_flow_log`], but emits at
/// [`LogLevel::Key`]. Use it for task/executor lifecycle checkpoints (created,
/// enqueued, started, prepared, completed, paused, resumed, cancelled, closed)
/// that the SDK author reviews to reconstruct a run. Consumers should persist
/// these and forward them for troubleshooting.
///
/// # Examples
///
/// ```
/// rusty_cat::meow_key_log!("enqueue", "task enqueued key={}", 7);
/// ```
#[macro_export]
macro_rules! meow_key_log {
    ($tag:expr, $($arg:tt)*) => {
        $crate::log::emit_lazy(|| {
            $crate::log::Log::new($crate::log::LogLevel::Key, $tag, format!($($arg)*))
        });
    };
}

/// Internal warning logging macro.
///
/// Same lazy evaluation semantics as [`meow_flow_log`], but emits at
/// [`LogLevel::Warn`]. Use it for non-fatal failures that a consumer would want
/// to surface even when filtering out debug noise — for example a remote cleanup
/// (multipart abort) that failed and may leave billable orphaned parts behind.
///
/// # Examples
///
/// ```
/// rusty_cat::meow_warn_log!("cleanup", "abort failed for key={}", 7);
/// ```
#[macro_export]
macro_rules! meow_warn_log {
    ($tag:expr, $($arg:tt)*) => {
        $crate::log::emit_lazy(|| {
            $crate::log::Log::new($crate::log::LogLevel::Warn, $tag, format!($($arg)*))
        });
    };
}

/// Internal error logging macro ([`LogLevel::Error`]).
///
/// Same lazy evaluation semantics as [`meow_flow_log`], but emits at
/// [`LogLevel::Error`]. Use it for failures with full detail — a chunk/part
/// upload or download failed, a task failed, an HTTP error response, a signing
/// failure — and for caught panics. For rich triage context (ids, byte range,
/// HTTP status, attempt, sanitized URL) build the entry with the [`Log`] `with_*`
/// builder methods and emit it via [`emit_lazy`] instead of this macro.
///
/// # Examples
///
/// ```
/// rusty_cat::meow_error_log!("upload_part", "part {} upload failed", 3);
/// ```
#[macro_export]
macro_rules! meow_error_log {
    ($tag:expr, $($arg:tt)*) => {
        $crate::log::emit_lazy(|| {
            $crate::log::Log::new($crate::log::LogLevel::Error, $tag, format!($($arg)*))
        });
    };
}

#[cfg(test)]
mod secret_redaction_tests {
    use super::{redact_secrets, sanitize_url};

    const SECRET: &str = "RC_SECRET_CANARY";

    #[test]
    fn quoted_assignments_and_json_values_are_fully_redacted() {
        let cases = [
            format!(r#"provider credential="{SECRET}" rejected"#),
            format!(r#"provider credential='{SECRET}' rejected"#),
            format!(r#"json={{"sig":"{SECRET}", "token" : "R\u0043_SECRET_CANARY"}}"#),
            format!(r#"json={{"password": "{SECRET}", "safe":"visible"}}"#),
        ];

        for input in cases {
            let safe = redact_secrets(&input);
            assert!(!safe.contains(SECRET), "raw secret leaked: {safe}");
            assert!(
                !safe.contains("R\\u0043_SECRET_CANARY"),
                "JSON-escaped secret leaked: {safe}"
            );
            assert!(
                safe.contains("REDACTED"),
                "redaction marker missing: {safe}"
            );
        }
    }

    #[test]
    fn bearer_authorization_is_redacted_case_insensitively() {
        for input in [
            format!("Authorization: Bearer {SECRET}"),
            format!("authorization: bearer {SECRET}; status=503"),
            format!("Bearer {SECRET}, retrying"),
        ] {
            let safe = redact_secrets(&input);
            assert!(!safe.contains(SECRET), "Bearer token leaked: {safe}");
            assert!(
                safe.contains("REDACTED"),
                "redaction marker missing: {safe}"
            );
        }
    }

    #[test]
    fn query_percent_encoding_is_removed_while_safe_diagnostics_survive() {
        let encoded = "%52%43%5F%53%45%43%52%45%54%5F%43%41%4E%41%52%59";
        let input = format!(
            "provider rejected: status=503 code=AccessDenied retry=2 sig={SECRET}&token={encoded}"
        );
        let safe = redact_secrets(&input);

        assert!(!safe.contains(SECRET));
        assert!(!safe.contains(encoded));
        assert!(safe.contains("status=503"));
        assert!(safe.contains("code=AccessDenied"));
        assert!(safe.contains("retry=2"));
    }

    #[test]
    fn percent_encoded_sensitive_assignment_keys_are_redacted() {
        for input in [
            format!("s%69g={SECRET}&safe=visible"),
            format!("s%2569g={SECRET}&safe=visible"),
            format!("%74oken={SECRET}&safe=visible"),
        ] {
            let safe = redact_secrets(&input);
            assert!(!safe.contains(SECRET), "encoded key leaked: {safe}");
            assert!(safe.contains("REDACTED"));
            assert!(safe.contains("safe=visible"));
        }
    }

    #[test]
    fn encoded_assignments_and_xml_secret_elements_are_redacted_without_touching_safe_keys() {
        let input = format!(
            "design%3Dblue%26sig%3D{SECRET}; \
             <Error><Token>{SECRET}</Token><Credential>{SECRET}</Credential></Error>"
        );
        let safe = redact_secrets(&input);

        assert!(!safe.contains(SECRET), "encoded/XML secret leaked: {safe}");
        assert!(safe.contains("design%3Dblue"));
        assert!(safe.contains("sig%3DREDACTED"));
        assert!(safe.contains("<Token>REDACTED</Token>"));
        assert!(safe.contains("<Credential>REDACTED</Credential>"));
    }

    #[test]
    fn secret_assignment_values_are_not_truncated_at_url_path_or_fragment_characters() {
        for input in [
            format!("token=prefix/{SECRET}/tail; status=503"),
            format!("credential=prefix?{SECRET}#tail; status=503"),
        ] {
            let safe = redact_secrets(&input);
            assert!(!safe.contains(SECRET), "raw secret suffix leaked: {safe}");
            assert!(safe.contains("status=503"), "safe suffix was lost: {safe}");
        }
    }

    #[test]
    fn percent_encoded_secret_values_are_not_truncated_at_encoded_path_or_fragment_characters() {
        for (input, safe_suffix) in [
            (
                format!("sig%3Dprefix%2F{SECRET}%3Ftail%23fragment%26safe%3Dvisible"),
                "safe%3Dvisible",
            ),
            (
                format!("sig%253Dprefix%252F{SECRET}%253Ftail%2523fragment%2526safe%253Dvisible"),
                "safe%253Dvisible",
            ),
        ] {
            let safe = redact_secrets(&input);
            assert!(
                !safe.contains(SECRET),
                "percent-encoded secret suffix leaked: {safe}"
            );
            assert!(safe.contains(safe_suffix), "safe suffix was lost: {safe}");
        }
    }

    #[test]
    fn authorization_header_schemes_and_bearer_paths_are_fully_redacted() {
        for input in [
            format!("Authorization: Bearer prefix/{SECRET}/tail; status=503"),
            format!("Authorization: SharedKey account:{SECRET}/tail; status=503"),
            format!("Authorization: Basic account:{SECRET}/tail; status=503"),
        ] {
            let safe = redact_secrets(&input);
            assert!(
                !safe.contains(SECRET),
                "authorization credential leaked: {safe}"
            );
            assert!(
                safe.starts_with("Authorization: REDACTED;"),
                "authorization scheme was not redacted as one value: {safe}"
            );
            assert!(safe.contains("status=503"), "safe suffix was lost: {safe}");
        }

        let bearer = redact_secrets(&format!("Bearer prefix/{SECRET}/tail, retrying"));
        assert_eq!(bearer, "Bearer REDACTED, retrying");
    }

    #[test]
    fn credential_shaped_suffix_keys_are_redacted_without_matching_safe_words() {
        let sensitive = format!("authToken={SECRET}&myCredential={SECRET}; apiKey={SECRET}");
        let safe = redact_secrets(&sensitive);
        assert!(!safe.contains(SECRET), "suffix-key secret leaked: {safe}");

        let ordinary = "design=blue presigned=false tokenizer=enabled";
        assert_eq!(redact_secrets(ordinary), ordinary);
    }

    #[test]
    fn url_sanitization_preserves_safe_raw_components_and_redacts_secrets() {
        let input = format!(
            "https://user:{SECRET}@example.test/o?tenant=one&plus=a+b&s%69g={SECRET}\
             &sig=&tenant=two#view=details&token={SECRET}"
        );
        let safe = sanitize_url(&input);

        assert!(!safe.contains(SECRET), "URL secret leaked: {safe}");
        assert!(safe.starts_with("https://REDACTED@example.test/o?"));
        assert!(safe.contains("tenant=one&plus=a+b&s%69g=REDACTED&sig=REDACTED&tenant=two"));
        assert!(safe.ends_with("#view=details&token=REDACTED"));
        assert_eq!(
            sanitize_url("https://example.test/o?q=a+b#section/%2F+details"),
            "https://example.test/o?q=a+b#section/%2F+details"
        );
    }

    #[test]
    fn ordinary_text_and_unicode_are_unchanged() {
        let input = "上传失败: status=503, object=中文/🙂, retry after 1s";
        assert_eq!(redact_secrets(input), input);
        let safe_assignments = "design=blue presigned=false tokenizer=enabled";
        assert_eq!(redact_secrets(safe_assignments), safe_assignments);
    }
}

#[cfg(test)]
mod truncated_xml_secret_tests {
    use super::redact_secrets;

    #[test]
    fn xml_secret_without_closing_tag_redacts_the_remaining_text() -> Result<(), &'static str> {
        for tag in ["Token", "Credential", "ns:Token", "AccessKey", "Password"] {
            for tail in [
                "",
                "[provider error body truncated]",
                "</Tok",
                "<Other>visible</Other>",
            ] {
                let input = format!("safe-prefix <{tag}>CANARY秘密/{tail}");
                let safe = redact_secrets(&input);
                if safe.contains("CANARY")
                    || !safe.contains("safe-prefix")
                    || !safe.contains("REDACTED")
                {
                    return Err("an incomplete XML secret was not safely redacted");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn xml_secret_with_closing_tag_preserves_nonsecret_surroundings() -> Result<(), &'static str> {
        let safe = redact_secrets("<Root><Token>CANARY</Token><Safe>visible</Safe><Token/></Root>");
        if safe != "<Root><Token>REDACTED</Token><Safe>visible</Safe><Token/></Root>" {
            return Err("well-formed XML redaction changed nonsecret text");
        }
        let ordinary = "<Safe>visible[provider error body truncated]";
        if redact_secrets(ordinary) != ordinary {
            return Err("truncated nonsecret XML was changed");
        }
        Ok(())
    }
}
