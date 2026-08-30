//! A shared HTTP client with the guards every driver needs and none should have
//! to write.
//!
//! Drivers get a client that already enforces timeouts, response size caps and a
//! polite user agent. They do not get to disable those, because "just this one
//! feed" is how a single malformed upstream response ends up buffering a
//! gigabyte into the daemon's heap.

use argus_core::source::SourceError;
use std::time::Duration;

/// Flatten an error and its causes into one line.
///
/// `reqwest`'s Display is only the outermost layer — "error sending request for
/// url (...)" with the actual reason (DNS failure, TLS handshake, connection
/// refused) one or more levels down. Logging just the top line sends whoever is
/// debugging looking for a network outage when the real cause is a missing root
/// certificate store.
fn describe(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

/// Default ceiling on a response body. Deliberately generous — Overpass and
/// STAC replies are genuinely large — but finite.
pub const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

const USER_AGENT: &str = concat!(
    "argus/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/Empt-y/argus)"
);

#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    max_bytes: usize,
}

impl HttpClient {
    pub fn new(timeout: Duration) -> Result<Self, SourceError> {
        Self::build(timeout, false)
    }

    /// A client that keeps cookies, for the one provider that authenticates
    /// with a session rather than a header.
    ///
    /// Not the default, and separate on purpose: a shared cookie jar across
    /// thirty unrelated providers is a way for one host's state to follow
    /// requests to another. Only the driver that needs it gets one.
    pub fn with_session(timeout: Duration) -> Result<Self, SourceError> {
        Self::build(timeout, true)
    }

    fn build(timeout: Duration, cookies: bool) -> Result<Self, SourceError> {
        let inner = reqwest::Client::builder()
            .cookie_store(cookies)
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(15))
            // Several providers redirect between CDN hosts; a couple of hops is
            // normal, an unbounded chain is a loop.
            .redirect(reqwest::redirect::Policy::limited(4))
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| SourceError::Transport(describe(&e)))?;
        Ok(Self {
            inner,
            max_bytes: DEFAULT_MAX_BYTES,
        })
    }

    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// POST a form and discard the body, keeping whatever session cookie came
    /// back. Used for login exchanges, which answer with a cookie and nothing
    /// worth reading.
    ///
    /// The form values are never logged, here or in an error: they are the
    /// credentials themselves.
    pub async fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(), SourceError> {
        let response = self
            .inner
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(|e| SourceError::Transport(describe(&e)))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(SourceError::Auth(format!("login rejected ({status})")));
        }
        if !status.is_success() {
            return Err(SourceError::Transport(format!("login failed ({status})")));
        }
        Ok(())
    }

    /// GET a URL, enforcing the size cap while the body streams rather than
    /// after it has already been buffered.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SourceError> {
        let response = self
            .inner
            .get(url)
            .send()
            .await
            .map_err(|e| SourceError::Transport(describe(&e)))?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(SourceError::RateLimited { retry_after });
        }
        // 401 is unambiguous. 403 is not — see SourceError::Forbidden.
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SourceError::Auth(format!("upstream returned {status}")));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(SourceError::Forbidden(format!("upstream returned {status}")));
        }
        if !status.is_success() {
            return Err(SourceError::Transport(format!(
                "upstream returned {status}"
            )));
        }

        // Trust Content-Length when it is present and over the cap: no reason to
        // stream a body we already know we will reject.
        if let Some(len) = response.content_length()
            && len as usize > self.max_bytes
        {
            return Err(SourceError::ResponseTooLarge {
                limit: self.max_bytes,
            });
        }

        use futures::StreamExt;
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| SourceError::Transport(describe(&e)))?;
            // Check before extending, so a hostile or broken upstream cannot
            // push us one whole chunk past the limit.
            if buf.len() + chunk.len() > self.max_bytes {
                return Err(SourceError::ResponseTooLarge {
                    limit: self.max_bytes,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    /// GET and deserialise JSON.
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<T, SourceError> {
        let bytes = self.get_bytes(url).await?;
        serde_json::from_slice(&bytes).map_err(|e| SourceError::Decode(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_identifies_the_project() {
        // Several of the free upstreams (Overpass, Radio Browser, Nominatim)
        // ask for a contactable UA and throttle or block generic ones.
        assert!(USER_AGENT.starts_with("argus/"));
        assert!(USER_AGENT.contains("github.com"));
    }

    #[test]
    fn clients_build_with_a_finite_default_cap() {
        let c = HttpClient::new(Duration::from_secs(5)).expect("client builds");
        assert_eq!(c.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(c.with_max_bytes(1024).max_bytes, 1024);
    }
}
