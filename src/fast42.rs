use serde::Deserialize;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Body, Client, Method, Request, Response};
use retainer::entry::CacheReadGuard;
use retainer::Cache;
use secrecy::{ExposeSecret, SecretString};
use tokio::task::JoinHandle;
use tower::buffer::Buffer;
use tower::limit::RateLimit;
use tower::ServiceExt;
use tower::{BoxError, Service, ServiceBuilder};
use futures::future::{self, try_join_all};

type ClientId = String;

struct ApiCredentials {
    client_id: ClientId,
    client_secret: SecretString,
}

#[derive(Debug, Deserialize)]
struct AccessToken {
    access_token: SecretString,
    token_type: String,
    expires_in: u64,
    scope: String,
    created_at: u64,
}

struct Fast42 {
    credentials: ApiCredentials,
    cache: Arc<Cache<ClientId, AccessToken>>,
    service: Buffer<RateLimit<RateLimit<Client>>, Request>,
    monitor: JoinHandle<()>,
}

#[derive(Clone)]
struct HttpOptions {
    key: String,
    value: String,
}

impl HttpOptions {
    fn new(key: String, value: String) -> Self {
        HttpOptions {
            key,
            value
        }
    }
}

const API_ROOT_URL: &str = "https://api.intra.42.fr/v2";

impl Fast42 {
    pub fn new(
        key: &str,
        secret: &str,
        ratelimit_per_hour: u64,
        ratelimit_per_second: u64,
    ) -> Fast42 {
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
                client_secret: SecretString::new(secret.into()),
            },
            cache,
            service,
            monitor,
        }
    }

    pub async fn get<D>(&self, endpoint: String, options: D) -> Result<Response, BoxError>
    where
        D: IntoIterator<Item = HttpOptions>,
    {
        let http_options = self.format_options(options);
        let endpoint = format!("{}{}", endpoint, http_options);
        let res = self.make_api_req(Method::GET, endpoint.clone()).await?;
        Ok(res)
    }

    pub async fn get_all_pages<D>(&'static self, endpoint: String, options: D) -> Result<Vec<Pin<Box<dyn Future<Output = Result<Response, Box<dyn Error + Send + Sync + '_>>>>>>, BoxError>
    where
        D: IntoIterator<Item = HttpOptions> + Clone
    {
        let mut page: u32 = 1;
        let page_options = vec![HttpOptions::new("page[size]".to_string(), "100".to_string()), HttpOptions::new("page[number]".to_string(), page.to_string())];
        let mut param_options = options.clone().into_iter();
        let all_options = page_options.into_iter().chain(&mut param_options).collect::<Vec<HttpOptions>>().into_iter();
        let http_options = self.format_options(all_options);
        let endpoint = format!("{}{}", endpoint, http_options);
        let res = self.make_api_req(Method::GET, endpoint.clone()).await?;
        let headers = res.headers();
        let total_pages = headers.get("x-total").unwrap().to_str()?.parse::<u32>()?;
        let all_pages  = (page..=total_pages).map(|p| -> Pin<Box<dyn Future<Output = Result<Response, Box<dyn Error + Send + Sync + 'static>>>>> {
            let page_options = vec![HttpOptions::new("page[size]".to_string(), "100".to_string()), HttpOptions::new("page[number]".to_string(), p.to_string())];
            let mut param_options = options.clone().into_iter();
            let all_options = page_options.into_iter().chain(&mut param_options).collect::<Vec<HttpOptions>>().into_iter();
            let http_options = self.format_options(all_options);
            let endpoint = format!("{}{}", endpoint, http_options);
            let res = self.make_api_req(Method::GET, endpoint);
            Box::pin(res)
        }).collect::<Vec<Pin<Box<dyn Future<Output = Result<Response, Box<dyn Error + Send + Sync + 'static>>>>>>>();
        Ok(all_pages)
    }

    fn format_options<'a, D>(&self, options: D) -> String
    where
        D: IntoIterator<Item = HttpOptions>,
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

    async fn make_api_req(&self, method: Method, endpoint: String) -> Result<Response, BoxError> {
        let client = Client::new();
        let access_token = self.get_access_token().await;
        match access_token {
            Ok(at) => {
                let token = at.access_token.expose_secret();
                let req = client
                    .request(method, format!("{}{}", API_ROOT_URL, endpoint))
                    .header("Authorization", format!("Bearer {}", token))
                    .header("Content-Type", "application/json")
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

    async fn make_api_req_with_body<T: Into<Body>>(
        &self,
        method: Method,
        endpoint: &str,
        body: T,
    ) -> Result<String, BoxError> {
        let client = Client::new();
        let access_token = self.get_access_token().await;
        match access_token {
            Ok(at) => {
                let token = at.access_token.expose_secret();
                let req = client
                    .request(method, format!("{}{}", API_ROOT_URL, endpoint))
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
                    .await?
                    .text()
                    .await?;
                Ok(res)
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_access_token(&self) -> Result<AccessToken, BoxError> {
        let client = Client::new();
        let req = client.post("https://api.intra.42.fr/oauth/token")
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

    async fn get_access_token(&self) -> Result<CacheReadGuard<AccessToken>, BoxError> {
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
    use super::*;

    #[tokio::test]
    async fn it_works() {
        // let fast42 = Fast42::new(
            // "UID",
            //"SECRET",
        //     14000,
        //     8,
        // );
        let result = fast42
            .get("/users".to_string(), [HttpOptions::new("filter[primary_campus_id]".to_string(), "14".to_string())])
            .await;
        match result {
            Ok(r) => {
                let text = r.text().await.unwrap();
                print!("{}", &text);
                assert_ne!(&text, "");
            }
            Err(e) => {
                println!("{}", e);
                assert!(false);
            }
        }
    }

    #[tokio::test]
    async fn get_all_pages() {
        let result = fast42
            .get_all_pages("/users".to_string(), vec![HttpOptions::new("filter[primary_campus_id]".to_string(), "14".to_string())])
            .await;
        match result {
            Ok(r) => {
                let pages: Vec<_> = try_join_all(r).await.unwrap();
                let last_index = pages.len() - 1;
                let last_page: String = pages.get(last_index).unwrap().text().await.unwrap();
                print!("{:?}", &last_page);
                assert_ne!(last_page, "");
            }
            Err(e) => {
                println!("{}", e);
                assert!(false);
            }
        }
    }
}
