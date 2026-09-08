//! Request-bound CDP authentication. Provider response bodies and credentials
//! are deliberately excluded from errors and Debug output.
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use x402_axum::facilitator_client::FacilitatorClient;
use x402_types::{
    facilitator::Facilitator,
    proto::{SettleRequest, SettleResponse, SupportedResponse, VerifyRequest, VerifyResponse},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid payment authorization")]
    InvalidPayment,
    #[error("payment journal unavailable")]
    Journal,
    #[error("payment outcome is uncertain; do not create a replacement authorization")]
    Uncertain,
    #[error("invalid facilitator configuration")]
    Configuration,
    #[error("cannot create facilitator authentication")]
    Authentication,
    #[error("facilitator request failed")]
    Transport,
    #[error("facilitator returned HTTP {0}")]
    Status(u16),
    #[error("invalid facilitator response")]
    Response,
}

#[derive(Deserialize)]
pub struct Credentials {
    pub api_key_id: String,
    pub api_key_secret: String,
}

pub struct Cdp {
    client: Client,
    key_id: String,
    signing_key: SigningKey,
}
impl std::fmt::Debug for Cdp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cdp([redacted])")
    }
}
#[derive(Clone, Debug)]
pub enum ClientKind {
    Plain(Arc<FacilitatorClient>),
    Cdp(Arc<Cdp>),
}

impl Cdp {
    pub fn new(credentials: Credentials) -> Result<Self, Error> {
        let key = STANDARD
            .decode(&credentials.api_key_secret)
            .map_err(|_| Error::Configuration)?;
        let pair: [u8; 64] = key.try_into().map_err(|_| Error::Configuration)?;
        SigningKey::from_keypair_bytes(&pair).map_err(|_| Error::Configuration)?;
        if credentials.api_key_id.is_empty() {
            return Err(Error::Configuration);
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::Configuration)?;
        Ok(Self {
            client,
            key_id: credentials.api_key_id,
            signing_key: SigningKey::from_keypair_bytes(&pair).map_err(|_| Error::Configuration)?,
        })
    }
    fn token(&self, method: &str, path: &str) -> Result<String, Error> {
        #[derive(Serialize)]
        struct Claims<'a> {
            sub: &'a str,
            iss: &'a str,
            aud: [&'a str; 1],
            nbf: u64,
            exp: u64,
            uri: String,
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Authentication)?
            .as_secs();
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| Error::Authentication)?;
        let header = serde_json::json!({"alg":"EdDSA","typ":"JWT","kid":self.key_id,"nonce":nonce.iter().map(|b|format!("{b:02x}")).collect::<String>()});
        let claims = Claims {
            sub: &self.key_id,
            iss: "cdp",
            aud: ["cdp_service"],
            nbf: now,
            exp: now + 120,
            uri: format!("{method} api.cdp.coinbase.com{path}"),
        };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let signing_input = format!(
            "{}.{}",
            b64.encode(serde_json::to_vec(&header).map_err(|_| Error::Authentication)?),
            b64.encode(serde_json::to_vec(&claims).map_err(|_| Error::Authentication)?)
        );
        let signature = self.signing_key.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            b64.encode(signature.to_bytes())
        ))
    }
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        operation: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, Error> {
        let path = format!("/platform/v2/x402/{operation}");
        let token = self.token(method.as_str(), &path)?;
        let mut request = self
            .client
            .request(method, format!("https://api.cdp.coinbase.com{path}"))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Status(response.status().as_u16()));
        }
        response.json().await.map_err(|_| Error::Response)
    }
}
impl ClientKind {
    pub fn new(url: &str, credentials: Option<Credentials>) -> Result<Self, Error> {
        if let Some(credentials) = credentials {
            let url = Url::parse(url).map_err(|_| Error::Configuration)?;
            if url.as_str().trim_end_matches('/') != "https://api.cdp.coinbase.com/platform/v2/x402"
            {
                return Err(Error::Configuration);
            }
            Ok(Self::Cdp(Arc::new(Cdp::new(credentials)?)))
        } else {
            let client = FacilitatorClient::try_from(url)
                .map_err(|_| Error::Configuration)?
                .with_timeout(Duration::from_secs(60));
            Ok(Self::Plain(Arc::new(client)))
        }
    }
}
impl Facilitator for ClientKind {
    type Error = Error;
    async fn verify(&self, request: &VerifyRequest) -> Result<VerifyResponse, Error> {
        match self {
            Self::Plain(c) => c.verify(request).await.map_err(|_| Error::Transport),
            Self::Cdp(c) => c.call(Method::POST, "verify", Some(request)).await,
        }
    }
    async fn settle(&self, request: &SettleRequest) -> Result<SettleResponse, Error> {
        match self {
            Self::Plain(c) => c.settle(request).await.map_err(|_| Error::Transport),
            Self::Cdp(c) => c.call(Method::POST, "settle", Some(request)).await,
        }
    }
    async fn supported(&self) -> Result<SupportedResponse, Error> {
        match self {
            Self::Plain(c) => c.supported().await.map_err(|_| Error::Transport),
            Self::Cdp(c) => c.call(Method::GET, "supported", None::<&()>).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_token_binds_request_and_hides_credentials() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let cdp = Cdp::new(Credentials {
            api_key_id: "test-key".into(),
            api_key_secret: STANDARD.encode(key.to_keypair_bytes()),
        })
        .unwrap();
        let token = cdp.token("POST", "/platform/v2/x402/verify").unwrap();
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, jsonwebtoken::Algorithm::EdDSA);
        assert_eq!(header.kid.as_deref(), Some("test-key"));
        assert_ne!(
            token,
            cdp.token("POST", "/platform/v2/x402/verify").unwrap()
        );
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
        validation.set_audience(&["cdp_service"]);
        let decoded = jsonwebtoken::decode::<serde_json::Value>(
            &token,
            &jsonwebtoken::DecodingKey::from_ed_components(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(key.verifying_key().as_bytes()),
            )
            .unwrap(),
            &validation,
        )
        .unwrap();
        assert_eq!(
            decoded.claims["uri"],
            "POST api.cdp.coinbase.com/platform/v2/x402/verify"
        );
        assert_eq!(format!("{cdp:?}"), "Cdp([redacted])");
    }
    #[test]
    fn rejects_wrong_key_or_destination() {
        assert!(
            Cdp::new(Credentials {
                api_key_id: "id".into(),
                api_key_secret: STANDARD.encode([1; 64])
            })
            .is_err()
        );
        assert!(
            ClientKind::new(
                "https://example.com",
                Some(Credentials {
                    api_key_id: "id".into(),
                    api_key_secret: String::new()
                })
            )
            .is_err()
        );
    }
}
