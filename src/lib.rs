#![forbid(unsafe_code)]
// #![ warn
// (
//    anonymous_parameters          ,
//    missing_copy_implementations  ,
//    missing_debug_implementations ,
//    missing_docs                  ,
//    nonstandard_style             ,
//    rust_2018_idioms              ,
//    single_use_lifetimes          ,
//    trivial_casts                 ,
//    trivial_numeric_casts         ,
//    unreachable_pub               ,
//    unused_extern_crates          ,
//    unused_qualifications         ,
//    variant_size_differences      ,
// )]

use serde::Deserialize;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tracing::{instrument, info, trace};

use futures::future::try_join_all;
use reqwest::{Body, Client, Method, Request, Response};
use retainer::entry::CacheReadGuard;
use retainer::Cache;
use secrecy::{ExposeSecret, SecretString};
use tokio::task::JoinHandle;
use tower::buffer::Buffer;
use tower::limit::RateLimit;
use tower::ServiceExt;
use tower::{BoxError, Service, ServiceBuilder};

#[cfg(test)]
use mockito;

type ClientId = String;

#[derive(Debug)]
struct ApiCredentials {
    client_id: ClientId,
    client_secret: SecretString,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct AccessToken {
    access_token: SecretString,
    token_type: String,
    expires_in: u64,
    scope: String,
    created_at: u64,
}

#[non_exhaustive]
pub struct Fast42 {
    credentials: ApiCredentials,
    cache: Arc<Cache<ClientId, AccessToken>>,
    service: Buffer<RateLimit<RateLimit<Client>>, Request>,
    monitor: JoinHandle<()>,
    root_url: String,
    version: String,
}

impl fmt::Debug for Fast42 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fast42")
            .field("credentials", &self.credentials)
            .field("root_url", &self.root_url)
            .field("version", &self.version)
            .finish()
    }
}

fn format_options<D>(options: D) -> String
where
    D: IntoIterator<Item = HttpOption>,
{
    options
        .into_iter()
        .enumerate()
        .map(|(i, options)| {
            if i == 0 {
                format!("?{}={}", options.key, options.value)
            } else {
                format!("&{}={}", options.key, options.value)
            }
        })
        .collect::<String>()
}

#[derive(Clone, Debug)]
pub struct HttpOption {
    key: String,
    value: String,
}

impl HttpOption {
    pub fn new<T>(key: T, value: T) -> Self
    where
        T: Into<String>,
    {
        HttpOption {
            key: key.into(),
            value: value.into(),
        }
    }
}

impl Fast42 {

    #[instrument]
    pub fn new(
        key: &str,
        secret: &SecretString,
        ratelimit_per_hour: u64,
        ratelimit_per_second: u64,
    ) -> Fast42 {
        trace!("Initializing struct Fast42");
        #[cfg(not(test))]
        let root_url: &str = "https://api.intra.42.fr";

        #[cfg(test)]
        let root_url: &str = &mockito::server_url();

        let version = "v2";

        let client = Client::new();

        let service = ServiceBuilder::new()
            .buffer(100)
            .rate_limit(ratelimit_per_hour, Duration::from_secs(3600))
            .rate_limit(ratelimit_per_second, Duration::from_secs(1))
            .service(client);

        let cache: Arc<Cache<ClientId, AccessToken>> = Arc::new(Cache::new());
        let clone = cache.clone();

        let monitor =
            tokio::spawn(async move { clone.monitor(4, 0.25, Duration::from_secs(2)).await });

        Fast42 {
            credentials: ApiCredentials {
                client_id: key.into(),
                client_secret: secret.to_owned(),
            },
            cache,
            service,
            monitor,
            root_url: root_url.to_string(),
            version: version.to_string(),
        }
    }

    #[instrument]
    pub async fn get<D>(&self, endpoint: String, options: D) -> Result<Response, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + std::fmt::Debug,
    {
        let http_options = format_options(options);
        let endpoint = format!("{}{}", endpoint, http_options);
        info!("Getting {}", endpoint);
        let res = self.make_api_req(Method::GET, endpoint.clone(), "").await?;
        Ok(res)
    }

    #[instrument]
    pub async fn get_all_pages<D>(
        &self,
        endpoint: String,
        options: D,
    ) -> Result<Vec<Response>, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + Clone + std::fmt::Debug,
    {
        let page: u32 = 1;
        let page_options = vec![
            HttpOption::new("page[size]".to_string(), "100".to_string()),
            HttpOption::new("page[number]".to_string(), page.to_string()),
        ];
        let mut param_options = options.clone().into_iter();
        let all_options = page_options
            .into_iter()
            .chain(&mut param_options)
            .collect::<Vec<HttpOption>>()
            .into_iter();
        let http_options = format_options(all_options);
        let endpoint = format!("{}{}", endpoint, http_options);
        trace!("GET page {}", page);
        let res = self.make_api_req(Method::GET, endpoint.clone(), "").await?;
        let headers = res.headers();
        let total_pages = headers.get("x-total").unwrap().to_str()?.parse::<u32>()? / 100;
        info!("Getting {} pages in total for {}", total_pages, endpoint);
        let all_pages = (page..=total_pages)
            .map(
                |p| -> Pin<
                    Box<
                        dyn Future<
                            Output = Result<Response, Box<dyn Error + Send + Sync + 'static>>,
                        >,
                    >,
                > {
                    let page_options = vec![
                        HttpOption::new("page[size]".to_string(), "100".to_string()),
                        HttpOption::new("page[number]".to_string(), p.to_string()),
                    ];
                    let mut param_options = options.clone().into_iter();
                    let all_options = page_options
                        .into_iter()
                        .chain(&mut param_options)
                        .collect::<Vec<HttpOption>>()
                        .into_iter();
                    let http_options = format_options(all_options);
                    let endpoint = format!("{}{}", endpoint, http_options);
                    trace!("GET page {}", p);
                    let res = self.make_api_req(Method::GET, endpoint, "");
                    Box::pin(res)
                },
            )
            .collect::<Vec<_>>();
        let pages = try_join_all(all_pages).await;
        pages
    }

    async fn make_api_req<T: Into<Body>>(
        &self,
        method: Method,
        endpoint: String,
        body: T,
    ) -> Result<Response, BoxError> {
        let client = Client::new();
        let access_token = self.get_access_token().await;
        match access_token {
            Ok(at) => {
                let token = at.access_token.expose_secret();
                let req = client
                    .request(
                        method,
                        format!("{}/{}{}", &self.root_url, &self.version, endpoint),
                    )
                    .header("Authorization", format!("Bearer {}", token))
                    .header("Content-Type", "application/json")
                    .body(body)
                    .build()?;
                let res = self
                    .service
                    .clone()
                    .ready_oneshot()
                    .await?
                    .call(req)
                    .await?;
                Ok(res)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_access_token(&self) -> Result<AccessToken, BoxError> {
        let client = Client::new();
        let req = client.post(format!("{}/oauth/token", &self.root_url))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("grant_type=client_credentials&client_id={}&client_secret={}&scope=projects%20public", self.credentials.client_id, self.credentials.client_secret.expose_secret()))
            .build()?;
        let res: AccessToken = self
            .service
            .clone()
            .ready_oneshot()
            .await?
            .call(req)
            .await?
            .json()
            .await?;
        Ok(res)
    }

    async fn store_access_token(&self, client_id: ClientId, token: AccessToken) {
        let expiry = &token.expires_in.clone() - 10;
        self.cache
            .insert(client_id, token, Duration::from_secs(expiry - 10))
            .await;
    }

    async fn get_access_token(&self) -> Result<CacheReadGuard<'_, AccessToken>, BoxError> {
        let token = self.cache.get(&self.credentials.client_id).await;
        match token {
            Some(t) => Ok(t),
            None => {
                let new_access_token = self.fetch_access_token().await?;
                self.store_access_token(self.credentials.client_id.clone(), new_access_token)
                    .await;
                let access = self.cache.get(&self.credentials.client_id).await.unwrap();
                Ok(access)
            }
        }
    }
}

impl Drop for Fast42 {
    fn drop(&mut self) {
        self.monitor.abort();
    }
}

#[cfg(test)]
mod tests {
    use mockito::mock;
    use secrecy::Secret;

    use super::*;

    #[tokio::test]
    async fn test_new() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8);
        assert_eq!(fast42.credentials.client_id, "UID");
        assert_eq!(fast42.credentials.client_secret.expose_secret(), "SECRET");
        assert!(!fast42.root_url.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_access_token_ok() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8);
        let _m = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let access_token = fast42.fetch_access_token().await;
        assert!(access_token.is_ok());
        let access_token = access_token.unwrap();
        assert_eq!(access_token.access_token.expose_secret(), "TOKEN");
        assert_eq!(access_token.token_type, "bearer");
        assert_eq!(access_token.expires_in, 7200);
        assert_eq!(access_token.scope, "public");
        assert_eq!(access_token.created_at, 1);
    }

    #[tokio::test]
    async fn test_store_access_token_ok() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8);
        let client_id = "UID".to_string();
        let access_token = AccessToken {
            access_token: Secret::new("TOKEN".to_string()),
            token_type: "bearer".to_string(),
            expires_in: 7200,
            scope: "public".to_string(),
            created_at: 1,
        };
        fast42
            .store_access_token(client_id.clone(), access_token)
            .await;
        let token = fast42.cache.get(&client_id).await;
        assert!(token.is_some());
        let token = token.unwrap();
        assert_eq!(token.access_token.expose_secret(), "TOKEN");
    }

    #[tokio::test]
    async fn test_get_access_token_cache_hit() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8);
        let client_id = "UID".to_string();
        let token = AccessToken {
            access_token: Secret::new("TOKEN".to_string()),
            token_type: "bearer".to_string(),
            expires_in: 7200,
            scope: "public".to_string(),
            created_at: 1,
        };
        let expiry = &token.expires_in.clone() - 10;
        fast42
            .cache
            .insert(client_id, token, Duration::from_secs(expiry - 10))
            .await;
        let access_token = fast42.get_access_token().await;
        assert!(access_token.is_ok());
        let access_token = access_token.unwrap();
        assert_eq!(access_token.access_token.expose_secret(), "TOKEN");
    }

    #[tokio::test]
    async fn test_get_access_token_cache_empty() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8);
        // Should mock fetch_access_token and store_access_token but for now I'm lazy
        let _m = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let access_token = fast42.get_access_token().await;
        assert!(access_token.is_ok());
        let access_token = access_token.unwrap();
        assert_eq!(access_token.access_token.expose_secret(), "TOKEN");
    }

    #[test]
    fn test_format_options_none() {
        let options = format_options([]);
        assert_eq!(options, "");
    }

    #[test]
    fn test_format_options_one() {
        let options = format_options([HttpOption::new("key", "value")]);
        assert_eq!(options, "?key=value");
    }

    #[test]
    fn test_format_options_multiple() {
        let options = format_options([
            HttpOption::new("key", "value"),
            HttpOption::new("key2", "value2"),
        ]);
        assert_eq!(options, "?key=value&key2=value2");
    }
}
