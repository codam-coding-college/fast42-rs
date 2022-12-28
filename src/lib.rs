//! A super fast 42 API connector
//!
//! Makes it easy to fetch data from the 42 API.
//! Main features:
//! - Rate Limited
//! - Async (easily fetch all pages of an endpoint!)
//! - Fast 🚀

#![forbid(unsafe_code)]
#![warn(
    anonymous_parameters,
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    nonstandard_style,
    rust_2018_idioms,
    single_use_lifetimes,
    trivial_casts,
    trivial_numeric_casts,
    unreachable_pub,
    unused_extern_crates,
    unused_qualifications,
    variant_size_differences
)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, instrument, trace};

use futures::future::try_join_all;
pub use reqwest::Response;
use reqwest::{Client, Method, Request};
use retainer::entry::CacheReadGuard;
use retainer::Cache;
use secrecy::{ExposeSecret, SecretString};
use tokio::task::JoinHandle;
use tower::buffer::Buffer;
use tower::limit::RateLimit;
pub use tower::BoxError;
use tower::ServiceExt;
use tower::{Service, ServiceBuilder};

#[cfg(test)]
use mockito;

/// Type alias for the 42 Client Id.
type ClientId = String;

/// Struct holding the 42 API credentials.
/// Note that the client_secret is a `secrecy` SecretString.
/// This is to prevent it from showing up in logs and debug output.
#[derive(Debug)]
struct ApiCredentials {
    client_id: ClientId,
    client_secret: SecretString,
    scope: String,
}

/// Struct holding the response from the token endpoint.
/// Not all fields are used.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct AccessToken {
    access_token: SecretString,
    token_type: String,
    expires_in: u64,
    scope: String,
    created_at: u64,
}

/// Struct holding everything needed to perform requests. Can only be instantiated using the `new()` method.
///
/// # Example
/// ```
/// # use fast42::Fast42;
/// use secrecy::SecretString;
///
/// # #[tokio::main]
/// # async fn main() {
/// let secret = SecretString::new("SECRET".to_owned());
/// let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
/// # }
/// ```
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

fn format_options_from_map<D>(options: D) -> String
where
    D: IntoIterator<Item = (String, String)>,
{
    options
        .into_iter()
        .enumerate()
        .map(|(i, options)| {
            if i == 0 {
                format!("?{}={}", options.0, options.1)
            } else {
                format!("&{}={}", options.0, options.1)
            }
        })
        .collect::<String>()
}

fn get_options_string_and_page_size<D>(options: D) -> (String, u32)
where
    D: IntoIterator<Item = HttpOption> + Clone + std::fmt::Debug,
{
    let mut page_size: u32 = 100;
    let mut optionsmap: HashMap<String, String> = HashMap::new();
    optionsmap.insert("page[size]".into(), "100".into());
    for op in options {
        optionsmap.insert(op.key, op.value);
    }
    if let Some(value) = optionsmap.get("page[size]") {
        if let Ok(new_size) = value.parse() {
            page_size = new_size
        } else {
            error!("Invalid Page Size HTTP option, ignoring.")
        }
    }
    let http_options = format_options_from_map(optionsmap);
    (http_options, page_size)
}

/// Key/value pair holder to represent HTTP options. Best constructed using the `new()` method.
///
/// ## Example
/// ```
/// # use fast42::HttpOption;
/// #
/// let options = [
///     HttpOption::new("campus_id", "14"),
///     HttpOption::new("cursus_id", "21"),
/// ];
/// ```
#[derive(Clone, Debug)]
pub struct HttpOption {
    key: String,
    value: String,
}

impl HttpOption {
    /// Creates a new HttpOption.
    ///
    /// ## Example
    /// ```
    /// # use fast42::HttpOption;
    /// let option = HttpOption::new("key", "value");
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
    /// Constructs a new [Fast42].
    ///
    /// Note: scopes are `%20` separated. See example.
    ///
    /// # Example
    /// ```
    /// # use fast42::Fast42;
    /// use secrecy::SecretString;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let secret = SecretString::new("SECRET".to_owned());
    /// let fast42 = Fast42::new("UID", &secret, 1400, 8, "public%20projects");
    /// # }
    /// ```
    #[instrument]
    pub fn new(
        key: &str,
        secret: &SecretString,
        ratelimit_per_hour: u64,
        ratelimit_per_second: u64,
        scope: &str,
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
                scope: scope.into(),
            },
            cache,
            service,
            monitor,
            root_url: root_url.to_string(),
            version: version.to_string(),
        }
    }

    /// Makes a single get request to the 42 API.
    ///
    /// Endpoint need to be a String, starting with a `/` like: `/users`.
    /// Options are the HTTP options. It takes any `IntoIterator` over `HttpOption` items. like: `[HttpOption::new("key", "value")]` or `vec![HttpOption::new("key", "value")]`.
    ///
    /// ## Example
    /// ```
    /// use fast42::{Fast42, HttpOption, Response, BoxError};
    /// use secrecy::SecretString;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let secret = SecretString::new("SECRET".to_owned());
    ///     let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
    ///
    ///     let response: Result<Response, BoxError> = fast42.get("/users", [HttpOption::new("campus_id", "14")]).await;
    ///     match response {
    ///         Ok(res) => {
    ///             print!("{}", res.text().await.unwrap())
    ///         },
    ///         Err(e) => {
    ///             println!("{}", e)
    ///         }
    ///     }
    /// }
    /// ```
    #[instrument]
    pub async fn get<D>(&self, endpoint: &str, options: D) -> Result<Response, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + std::fmt::Debug,
    {
        let http_options = format_options(options);
        let endpoint = format!("{}{}", endpoint, http_options);
        info!("Getting {}", endpoint);
        let res = self.make_api_req(Method::GET, endpoint.clone(), "").await;
        match res {
            Ok(res) => {
                trace!("request succeeded");
                Ok(res)
            }
            Err(e) => {
                error!("failed making request: {}", e);
                Err(e)
            }
        }
    }

    /// Makes a single POST request to the 42 API.
    ///
    /// Endpoint needs to be a String, starting with a `/` like: `/users`.
    /// Options are the HTTP options. It takes any `IntoIterator` over `HttpOption` items. like: `[HttpOption::new("key", "value")]` or `vec![HttpOption::new("key", "value")]`.
    /// Json is anything that can be serialized into JSON (using serde).
    ///
    /// ## Example
    /// ```
    /// use fast42::{Fast42, Response, BoxError};
    /// use secrecy::SecretString;
    /// use std::collections::HashMap;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let secret = SecretString::new("SECRET".to_owned());
    ///     let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
    ///
    ///     let mut json_body = HashMap::new();
    ///     json_body.insert("id", 1);
    ///     let response: Result<Response, BoxError> = fast42.post("/users", [], json_body).await;
    ///     let response: Result<Response, BoxError> = fast42.post("/users", [], r#"{"id": 1}"#).await;
    /// }
    /// ```
    #[instrument]
    pub async fn post<D, T>(
        &self,
        endpoint: &str,
        options: D,
        json: T,
    ) -> Result<Response, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + std::fmt::Debug,
        T: Serialize + std::fmt::Debug,
    {
        let http_options = format_options(options);
        let endpoint = format!("{}{}", endpoint, http_options);
        info!("Getting {}", endpoint);
        let res = self
            .make_api_req(Method::POST, endpoint.clone(), json)
            .await;
        match res {
            Ok(res) => {
                trace!("request succeeded");
                Ok(res)
            }
            Err(e) => {
                error!("failed making request: {}", e);
                Err(e)
            }
        }
    }

    /// Makes a single PUT request to the 42 API.
    ///
    /// Endpoint needs to be a String, starting with a `/` like: `/users`.
    /// Options are the HTTP options. It takes any `IntoIterator` over `HttpOption` items. like: `[HttpOption::new("key", "value")]` or `vec![HttpOption::new("key", "value")]`.
    /// Json is anything that can be serialized into JSON (using serde).
    ///
    /// ## Example
    /// ```
    /// use fast42::{Fast42, Response, BoxError};
    /// use secrecy::SecretString;
    /// use std::collections::HashMap;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let secret = SecretString::new("SECRET".to_owned());
    ///     let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
    ///
    ///     let mut json_body = HashMap::new();
    ///     json_body.insert("id", 1);
    ///     let response: Result<Response, BoxError> = fast42.put("/users", [], json_body).await;
    ///     let response: Result<Response, BoxError> = fast42.put("/users", [], r#"{"id": 1}"#).await;
    /// }
    /// ```
    #[instrument]
    pub async fn put<D, T>(&self, endpoint: &str, options: D, json: T) -> Result<Response, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + std::fmt::Debug,
        T: Serialize + std::fmt::Debug,
    {
        let http_options = format_options(options);
        let endpoint = format!("{}{}", endpoint, http_options);
        info!("Getting {}", endpoint);
        let res = self.make_api_req(Method::PUT, endpoint.clone(), json).await;
        match res {
            Ok(res) => {
                trace!("request succeeded");
                Ok(res)
            }
            Err(e) => {
                error!("failed making request: {}", e);
                Err(e)
            }
        }
    }

    /// Makes a single PATCH request to the 42 API.
    ///
    /// Endpoint needs to be a String, starting with a `/` like: `/users`.
    /// Options are the HTTP options. It takes any `IntoIterator` over `HttpOption` items. like: `[HttpOption::new("key", "value")]` or `vec![HttpOption::new("key", "value")]`.
    /// Json is anything that can be serialized into JSON (using serde).
    ///
    /// ## Example
    /// ```
    /// use fast42::{Fast42, Response, BoxError};
    /// use secrecy::SecretString;
    /// use std::collections::HashMap;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let secret = SecretString::new("SECRET".to_owned());
    ///     let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
    ///
    ///     let mut json_body = HashMap::new();
    ///     json_body.insert("id", "1");
    ///     json_body.insert("name", "fast42");
    ///     let response: Result<Response, BoxError> = fast42.patch("/users", [], json_body).await;
    ///     let response: Result<Response, BoxError> = fast42.patch("/users", [], r#"{"id": "1", "name": "fast42"}"#).await;
    /// }
    /// ```
    #[instrument]
    pub async fn patch<D, T>(
        &self,
        endpoint: &str,
        options: D,
        json: T,
    ) -> Result<Response, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + std::fmt::Debug,
        T: Serialize + std::fmt::Debug,
    {
        let http_options = format_options(options);
        let endpoint = format!("{}{}", endpoint, http_options);
        info!("Getting {}", endpoint);
        let res = self
            .make_api_req(Method::PATCH, endpoint.clone(), json)
            .await;
        match res {
            Ok(res) => {
                trace!("request succeeded");
                Ok(res)
            }
            Err(e) => {
                error!("failed making request: {}", e);
                Err(e)
            }
        }
    }

    /// Makes a single DELETE request to the 42 API.
    ///
    /// Endpoint needs to be a String, starting with a `/` like: `/users`.
    /// Options are the HTTP options. It takes any `IntoIterator` over `HttpOption` items. like: `[HttpOption::new("key", "value")]` or `vec![HttpOption::new("key", "value")]`.
    /// Json is anything that can be serialized into JSON (using serde).
    ///
    /// ## Example
    /// ```
    /// use fast42::{Fast42, HttpOption, Response, BoxError};
    /// use secrecy::SecretString;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let secret = SecretString::new("SECRET".to_owned());
    ///     let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
    ///
    ///     let response: Result<Response, BoxError> = fast42.delete("/users", [HttpOption::new("id", "1")], "").await;
    /// }
    /// ```
    #[instrument]
    pub async fn delete<D, T>(
        &self,
        endpoint: &str,
        options: D,
        json: T,
    ) -> Result<Response, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + std::fmt::Debug,
        T: Serialize + std::fmt::Debug,
    {
        let http_options = format_options(options);
        let endpoint = format!("{}{}", endpoint, http_options);
        info!("Getting {}", endpoint);
        let res = self
            .make_api_req(Method::DELETE, endpoint.clone(), json)
            .await;
        match res {
            Ok(res) => {
                trace!("request succeeded");
                Ok(res)
            }
            Err(e) => {
                error!("failed making request: {}", e);
                Err(e)
            }
        }
    }

    /// Gets all the pages of an endpoint of the 42 API.
    /// This function first awaits the request of the first page to find the total number of pages. Then it fetches the other pages async.
    /// If all goes well you'll get a `Vec<Response>`. If any of the pages fail, you get an Error.
    ///
    /// ## Example
    /// ```
    /// use fast42::{Fast42, HttpOption, Response, BoxError};
    /// use secrecy::SecretString;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let secret = SecretString::new("SECRET".to_owned());
    ///     let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
    ///
    ///     let response: Result<Vec<Response>, BoxError> = fast42.get_all_pages("/users", [HttpOption::new("campus_id", "14")]).await;
    ///     match response {
    ///         Ok(res) => {
    ///             let page_count = res.len();
    ///             print!("Fetched {} pages", page_count)
    ///         },
    ///         Err(e) => {
    ///             println!("{}", e)
    ///         }
    ///     }
    /// }
    /// ```
    pub async fn get_all_pages<D>(
        &self,
        endpoint: &str,
        options: D,
    ) -> Result<Vec<Response>, BoxError>
    where
        D: IntoIterator<Item = HttpOption> + Clone + std::fmt::Debug,
    {
        let page_number: u32 = 1;
        let (options_string, page_size) = get_options_string_and_page_size(options);
        let query = format!("{}{}&page[number]={}", endpoint, &options_string, page_number);
        trace!("GET page {}", page_number);
        let res = self.make_api_req(Method::GET, query, "").await?;
        let headers = res.headers();
        let total_pages = headers.get("x-total").unwrap().to_str()?.parse::<u32>()? / page_size;
        info!("Getting {} pages in total for {}", total_pages, endpoint);
        let all_pages = (page_number..=total_pages)
            .map(
                |page_number| -> Pin<
                    Box<
                        dyn Future<
                            Output = Result<Response, Box<dyn Error + Send + Sync + 'static>>,
                        >,
                    >,
                > {
                    let query = format!("{}{}&page[number]={}", endpoint, &options_string, page_number);
                    trace!("GET page {}", page_number);
                    let res = self.make_api_req(Method::GET, query, "");
                    Box::pin(res)
                },
            )
            .collect::<Vec<_>>();
        let pages = try_join_all(all_pages).await;
        pages
    }

    async fn make_api_req<T: Serialize>(
        &self,
        method: Method,
        endpoint: String,
        json: T,
    ) -> Result<Response, BoxError> {
        let client = Client::new();
        let access_token = self.get_access_token().await;
        match access_token {
            Ok(at) => {
                trace!("got access_token, making request...");
                let token = at.access_token.expose_secret();
                let req = client
                    .request(
                        method,
                        format!("{}/{}{}", &self.root_url, &self.version, endpoint),
                    )
                    .header("Authorization", format!("Bearer {}", token))
                    .header("Content-Type", "application/json")
                    .json(&json)
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
            Err(e) => {
                error!("failed to get access token");
                Err(e)
            }
        }
    }

    async fn fetch_access_token(&self) -> Result<AccessToken, BoxError> {
        let client = Client::new();
        let req = client
            .post(format!("{}/oauth/token", &self.root_url))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type=client_credentials&client_id={}&client_secret={}&scope={}",
                self.credentials.client_id,
                self.credentials.client_secret.expose_secret(),
                self.credentials.scope
            ))
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
    use reqwest::StatusCode;
    use secrecy::Secret;

    use super::*;

    #[tokio::test]
    async fn test_new() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
        assert_eq!(fast42.credentials.client_id, "UID");
        assert_eq!(fast42.credentials.client_secret.expose_secret(), "SECRET");
        assert!(!fast42.root_url.is_empty());
    }

    #[tokio::test]
    async fn test_get() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
        let _m = mock("GET", "/v2/users?id=1")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"id":1}"#)
            .create();
        let _m = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let res = fast42
            .get("/users", [HttpOption::new("id", "1")])
            .await
            .unwrap();
        #[derive(Deserialize)]
        struct User {
            id: u64,
        }
        let json: User = res.json().await.unwrap();
        assert_eq!(json.id, 1);
    }

    #[tokio::test]
    async fn test_get_all_pages() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
        let _m1 = mock("GET", "/v2/users?page[size]=100&page[number]=1")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_header("x-total", "300")
            .with_body(r#"{"id":1}"#)
            .create();
        let _m2 = mock("GET", "/v2/users?page[size]=100&page[number]=2")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_header("x-total", "300")
            .with_body(r#"{"id":2}"#)
            .create();
        let _m3 = mock("GET", "/v2/users?page[size]=100&page[number]=3")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_header("x-total", "300")
            .with_body(r#"{"id":3}"#)
            .create();
        let _m4 = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let mut res = fast42.get_all_pages("/users", []).await.unwrap();
        #[derive(Deserialize)]
        struct User {
            id: u64,
        }
        assert_eq!(res.pop().unwrap().json::<User>().await.unwrap().id, 3);
        assert_eq!(res.pop().unwrap().json::<User>().await.unwrap().id, 2);
        assert_eq!(res.pop().unwrap().json::<User>().await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn test_post() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
        let _m = mock("POST", "/v2/users")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"id":1}"#)
            .create();
        let _m = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let res = fast42.post("/users", [], r#"{"id": 1}"#).await.unwrap();
        #[derive(Deserialize)]
        struct User {
            id: u64,
        }
        let json: User = res.json().await.unwrap();
        assert_eq!(json.id, 1);
    }

    #[tokio::test]
    async fn test_put() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
        let _m = mock("PUT", "/v2/users")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"id":1}"#)
            .create();
        let _m = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let res = fast42.put("/users", [], r#"{"id": 1}"#).await.unwrap();
        #[derive(Deserialize)]
        struct User {
            id: u64,
        }
        let json: User = res.json().await.unwrap();
        assert_eq!(json.id, 1);
    }

    #[tokio::test]
    async fn test_patch() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
        let _m = mock("PATCH", "/v2/users")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"id": 1, "name": "fast42"}"#)
            .create();
        let _m = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let res = fast42
            .patch("/users", [], r#"{"id": 1, "name": "fast42"}"#)
            .await
            .unwrap();
        #[derive(Deserialize)]
        struct User {
            id: u64,
            name: String,
        }
        let json: User = res.json().await.unwrap();
        assert_eq!(json.id, 1);
        assert_eq!(json.name, "fast42");
    }

    #[tokio::test]
    async fn test_delete() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
        let _m = mock("DELETE", "/v2/users?id=1").with_status(204).create();
        let _m = mock("POST", "/oauth/token")
            .with_status(200)
            .with_header("content-type", "application/json; charset=utf-8")
            .with_body(r#"{"access_token":"TOKEN","token_type":"bearer","expires_in":7200,"scope":"public","created_at":1}"#)
            .create();
        let res = fast42
            .delete("/users", [HttpOption::new("id", "1")], "")
            .await
            .unwrap();
        let status = res.status();
        assert_eq!(status, StatusCode::NO_CONTENT)
    }

    #[tokio::test]
    async fn test_fetch_access_token_ok() {
        let secret = SecretString::new("SECRET".to_owned());
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
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
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
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
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
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
        let fast42 = Fast42::new("UID", &secret, 1400, 8, "public");
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
