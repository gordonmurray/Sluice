//! Optional, bounded GoatCounter events for final gateway outcomes.
//! Only configured route labels and coarse client classes leave the gateway.
use axum::{extract::Request, response::Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{cell::Cell, future::Future, sync::Arc, time::Duration};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Outcome {
    #[default]
    None,
    Purchase,
    FirstPurchase,
    RepeatPurchase,
    Replay,
    Rejected,
    Unknown,
}
tokio::task_local! { static OUTCOME: Cell<Outcome>; }
pub fn mark(outcome: Outcome) {
    let _ = OUTCOME.try_with(|value| value.set(outcome));
}
pub fn replay() {
    let _ = OUTCOME.try_with(|value| {
        if value.get() == Outcome::None {
            value.set(Outcome::Replay);
        }
    });
}
pub async fn observe(future: impl Future<Output = Response>) -> (Response, Outcome) {
    OUTCOME
        .scope(Cell::new(Outcome::None), async {
            let response = future.await;
            let outcome = OUTCOME.with(Cell::get);
            (response, outcome)
        })
        .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    site: String,
    environment: String,
    routes: Vec<Route>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    path: String,
    label: String,
    #[serde(default)]
    prefix: bool,
}
fn label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
impl Config {
    fn validate(&self) -> anyhow::Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.site)?;
        anyhow::ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "analytics site must be a plain HTTPS origin"
        );
        anyhow::ensure!(
            label(&self.environment) && self.routes.len() <= 64,
            "invalid analytics environment or route count"
        );
        for route in &self.routes {
            anyhow::ensure!(
                label(&route.label)
                    && route.path.starts_with('/')
                    && route.path.len() <= 256
                    && !route.path.contains(['?', '#'])
                    && (!route.prefix || route.path.ends_with('/')),
                "invalid analytics route label or prefix boundary"
            );
        }
        url.set_path("/api/v0/count");
        Ok(url)
    }
    fn route(&self, path: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|r| {
                if r.prefix {
                    path.starts_with(&r.path)
                } else {
                    path == r.path
                }
            })
            .map(|r| r.label.as_str())
    }
}
#[derive(Clone, Default)]
pub struct Analytics(Option<Arc<Inner>>);
struct Inner {
    config: Config,
    sender: mpsc::Sender<Value>,
}
pub struct Context {
    route: String,
    audience: &'static str,
    signed: bool,
}
impl Analytics {
    pub fn from_env() -> anyhow::Result<Self> {
        let Ok(path) = std::env::var("GOATCOUNTER_CONFIG_PATH") else {
            return Ok(Self::default());
        };
        let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
        let url = config.validate()?;
        let token = std::fs::read_to_string(std::env::var("GOATCOUNTER_TOKEN_PATH")?)?;
        anyhow::ensure!(!token.trim().is_empty(), "empty analytics token");
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim()))?;
        authorization.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, authorization);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        let (sender, mut receiver) = mpsc::channel::<Value>(1024);
        tokio::spawn(async move {
            // One batch per tick also bounds API traffic under load.
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let mut hits = Vec::with_capacity(100);
                while hits.len() < 100 {
                    match receiver.try_recv() {
                        Ok(hit) => hits.push(hit),
                        Err(_) => break,
                    }
                }
                if hits.is_empty() {
                    if receiver.is_closed() {
                        break;
                    }
                    continue;
                }
                let count = hits.len() as u64;
                let result = client
                    .post(url.clone())
                    .json(&json!({"no_sessions":true,"hits":hits}))
                    .send()
                    .await;
                let delivered = result.as_ref().is_ok_and(|r| r.status().is_success());
                crate::metrics()
                    .analytics_delivery
                    .with_label_values(&[if delivered { "delivered" } else { "failed" }])
                    .inc_by(count);
                if !delivered {
                    tracing::warn!("analytics batch unavailable; events dropped");
                }
                // No automatic retry: an ambiguous API response could duplicate events.
            }
        });
        Ok(Self(Some(Arc::new(Inner { config, sender }))))
    }
    pub fn context(&self, request: &Request) -> Option<Context> {
        let inner = self.0.as_ref()?;
        let route = inner.config.route(request.uri().path())?.to_string();
        let agent = request
            .headers()
            .get("user-agent")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let test = request
            .headers()
            .get("x-sluice-test")
            .is_some_and(|h| h == "1");
        Some(Context {
            route,
            audience: audience(agent, test),
            signed: request.headers().contains_key("payment-signature")
                || request.headers().contains_key("x-payment"),
        })
    }
    pub fn finish(&self, context: Option<Context>, response: &Response, outcome: Outcome) {
        let (Some(inner), Some(context)) = (&self.0, context) else {
            return;
        };
        let outcome = classify(outcome, response.status().as_u16(), context.signed);
        let path = format!(
            "/gateway/{}/{}/{}/{}/{}",
            inner.config.environment,
            context.audience,
            context.route,
            outcome,
            response.status().as_u16()
        );
        let event =
            json!({"path":path,"title":format!("{}: {}", context.route, outcome),"event":true});
        if inner.sender.try_send(event).is_err() {
            crate::metrics()
                .analytics_delivery
                .with_label_values(&["queue-full"])
                .inc();
        }
    }
    #[cfg(test)]
    pub fn capture() -> (Self, mpsc::Receiver<Value>) {
        let (sender, receiver) = mpsc::channel(100);
        let config = Config {
            site: "https://example.test".into(),
            environment: "test".into(),
            routes: vec![Route {
                path: "/paid/".into(),
                label: "lookup".into(),
                prefix: true,
            }],
        };
        (Self(Some(Arc::new(Inner { config, sender }))), receiver)
    }
}
fn classify(outcome: Outcome, status: u16, signed: bool) -> &'static str {
    match outcome {
        Outcome::Purchase => "purchase",
        Outcome::FirstPurchase => "purchase-first",
        Outcome::RepeatPurchase => "purchase-repeat",
        Outcome::Replay => "replay",
        Outcome::Rejected => "payment-rejected",
        Outcome::Unknown => "payment-unknown",
        Outcome::None => match (status, signed) {
            (402, false) => "quote",
            (402, true) => "payment-rejected",
            (429, _) => "rate-limited",
            (400..=499, _) => "request-rejected",
            (500..=599, _) => "service-error",
            _ => "response",
        },
    }
}
fn audience(agent: &str, test: bool) -> &'static str {
    if test {
        return "test";
    }
    let agent = agent.to_ascii_lowercase();
    if [
        "gptbot",
        "chatgpt",
        "oai-searchbot",
        "claude",
        "anthropic",
        "perplexity",
        "cohere",
        "google-extended",
        "bytespider",
    ]
    .iter()
    .any(|s| agent.contains(s))
    {
        "llm-reported"
    } else if ["bot", "crawler", "spider"]
        .iter()
        .any(|s| agent.contains(s))
    {
        "crawler-reported"
    } else if agent.contains("mozilla/") {
        "browser"
    } else {
        "api-other"
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_quotes_failures_and_claimed_audiences() {
        assert_eq!(classify(Outcome::None, 402, false), "quote");
        assert_eq!(classify(Outcome::None, 402, true), "payment-rejected");
        assert_eq!(classify(Outcome::Unknown, 503, true), "payment-unknown");
        assert_eq!(classify(Outcome::None, 200, false), "response");
        assert_eq!(audience("ChatGPT-User", false), "llm-reported");
        assert_eq!(audience("ChatGPT-User", true), "test");
    }
    #[tokio::test]
    async fn private_inputs_never_enter_event_and_queue_is_bounded() {
        let (analytics, mut receiver) = Analytics::capture();
        let request = Request::builder()
            .uri("/paid/private-company?secret-query=1")
            .header("payment-signature", "secret-signature")
            .header("authorization", "secret-api-key")
            .header("user-agent", "ChatGPT private-client")
            .body(axum::body::Body::empty())
            .unwrap();
        analytics.finish(
            analytics.context(&request),
            &Response::new(axum::body::Body::empty()),
            Outcome::FirstPurchase,
        );
        let hit = receiver.recv().await.unwrap().to_string();
        assert!(hit.contains("/gateway/test/llm-reported/lookup/purchase-first/200"));
        for private in [
            "private-company",
            "secret-query",
            "secret-signature",
            "secret-api-key",
            "private-client",
        ] {
            assert!(!hit.contains(private));
        }
        for _ in 0..200 {
            analytics.finish(
                analytics.context(&request),
                &Response::new(axum::body::Body::empty()),
                Outcome::Replay,
            );
        }
        assert_eq!(receiver.len(), 100);
    }
}
