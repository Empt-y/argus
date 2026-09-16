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
    /// One request-per-second budget per host, shared by every source that
    /// happens to point at it.
    ///
    /// This lives here rather than in a driver because the thing that gets a
    /// provider rate-limited is the *total* offered load, and no driver can see
    /// that. Three separate 429s in one afternoon were all this shape: the
    /// flights chain and the Europe sweep both reaching adsb.fi, each
    /// individually polite and jointly over the line — and the chain answers a
    /// 429 by sidelining a provider for fifteen minutes, so the cost of one
    /// impolite burst is a quarter of an hour of missing coverage.
    ///
    /// Keyed by host, so pacing adsb.fi never delays a call to CelesTrak.
    pacer: std::sync::Arc<governor::DefaultKeyedRateLimiter<String>>,
}

/// A response body with its headers.
pub struct Page {
    pub body: Vec<u8>,
    pub headers: reqwest::header::HeaderMap,
}

impl Page {
    /// The URL in a `Link` header with `rel="next"`, if there is one.
    pub fn next_link(&self) -> Option<String> {
        let link = self.headers.get(reqwest::header::LINK)?.to_str().ok()?;
        link.split(',')
            .filter(|part| part.contains("rel=\"next\""))
            .find_map(|part| {
                let start = part.find('<')? + 1;
                let end = part[start..].find('>')? + start;
                Some(part[start..end].to_string())
            })
    }
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
            // One a second, which is what the community aggregators ask for and
            // the strictest budget any provider here publishes. Sources that
            // poll once a cadence never notice it; only a source issuing a
            // burst — a tiled area sweep — is ever actually paced.
            pacer: std::sync::Arc::new(governor::RateLimiter::keyed(governor::Quota::per_second(
                std::num::NonZeroU32::new(1).expect("1 is not zero"),
            ))),
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
        self.pace(url).await;
        let response = self
            .inner
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(|e| SourceError::Transport(describe(&e)))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SourceError::Auth(format!("login rejected ({status})")));
        }
        if !status.is_success() {
            return Err(SourceError::Transport(format!("login failed ({status})")));
        }
        Ok(())
    }

    /// Wait until this host's budget allows another request.
    ///
    /// A URL with no host — which reqwest will reject anyway — is not paced;
    /// inventing a key for it would put every malformed URL in the system into
    /// one shared bucket.
    async fn pace(&self, url: &str) {
        if let Ok(parsed) = reqwest::Url::parse(url)
            && let Some(host) = parsed.host_str()
        {
            self.pacer.until_key_ready(&host.to_string()).await;
        }
    }

    /// GET a URL, enforcing the size cap while the body streams rather than
    /// after it has already been buffered.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SourceError> {
        Ok(self.get_page(url).await?.body)
    }

    /// GET a URL and keep the response headers as well as the body, for the
    /// upstreams that page with `Link: <…>; rel="next"` and nothing in the
    /// body says where the next page is.
    pub async fn get_page(&self, url: &str) -> Result<Page, SourceError> {
        self.pace(url).await;
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
            return Err(SourceError::Forbidden(format!(
                "upstream returned {status}"
            )));
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

        let headers = response.headers().clone();
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
        Ok(Page { body: buf, headers })
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
    fn the_next_page_is_read_from_the_link_header_as_satnogs_sends_it() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<https://x/api/?cursor=abc&format=json>; rel=\"next\", <https://x/api/?cursor=zzz>; rel=\"prev\""
                .parse()
                .unwrap(),
        );
        let page = Page {
            body: Vec::new(),
            headers,
        };
        assert_eq!(
            page.next_link().as_deref(),
            Some("https://x/api/?cursor=abc&format=json")
        );

        let last = Page {
            body: Vec::new(),
            headers: reqwest::header::HeaderMap::new(),
        };
        assert_eq!(last.next_link(), None, "no header, no next page");
    }

    #[test]
    fn clients_build_with_a_finite_default_cap() {
        let c = HttpClient::new(Duration::from_secs(5)).expect("client builds");
        assert_eq!(c.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(c.with_max_bytes(1024).max_bytes, 1024);
    }
}
