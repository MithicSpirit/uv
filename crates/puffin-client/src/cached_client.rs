use std::fmt::Debug;
use std::future::Future;
use std::time::SystemTime;

use futures::FutureExt;
use http_cache_semantics::{AfterResponse, BeforeRequest, CachePolicy};
use reqwest::{Request, Response};
use reqwest_middleware::ClientWithMiddleware;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::{debug, info_span, instrument, trace, warn, Instrument};

use puffin_cache::{CacheEntry, Freshness};
use puffin_fs::write_atomic;

use crate::{cache_headers::CacheHeaders, Error, ErrorKind};

pub trait Cacheable: Sized + Send {
    type Target;

    fn from_bytes(bytes: Vec<u8>) -> Result<Self::Target, crate::Error>;
    fn to_bytes(&self) -> Result<Vec<u8>, crate::Error>;
    fn into_target(self) -> Self::Target;
}

/// A wrapper type that makes anything with Serde support automatically
/// implement Cacheable.
#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SerdeCacheable<T> {
    inner: T,
}

impl<T: Send + Serialize + DeserializeOwned> Cacheable for SerdeCacheable<T> {
    type Target = T;

    fn from_bytes(bytes: Vec<u8>) -> Result<T, Error> {
        Ok(rmp_serde::from_slice::<T>(&bytes).map_err(ErrorKind::Decode)?)
    }

    fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(rmp_serde::to_vec(&self.inner).map_err(ErrorKind::Encode)?)
    }

    fn into_target(self) -> Self::Target {
        self.inner
    }
}

/// Either a cached client error or a (user specified) error from the callback
#[derive(Debug)]
pub enum CachedClientError<CallbackError> {
    Client(Error),
    Callback(CallbackError),
}

impl<CallbackError> From<Error> for CachedClientError<CallbackError> {
    fn from(error: Error) -> Self {
        CachedClientError::Client(error)
    }
}

impl<CallbackError> From<ErrorKind> for CachedClientError<CallbackError> {
    fn from(error: ErrorKind) -> Self {
        CachedClientError::Client(error.into())
    }
}

impl<E: Into<Error>> From<CachedClientError<E>> for Error {
    fn from(error: CachedClientError<E>) -> Error {
        match error {
            CachedClientError::Client(error) => error,
            CachedClientError::Callback(error) => error.into(),
        }
    }
}

#[derive(Debug)]
enum CachedResponse {
    /// The cached response is fresh without an HTTP request (e.g. immutable)
    FreshCache(Vec<u8>),
    /// The cached response is fresh after an HTTP request (e.g. 304 not modified)
    NotModified(DataWithCachePolicy),
    /// There was no prior cached response or the cache was outdated
    ///
    /// The cache policy is `None` if it isn't storable
    ModifiedOrNew(Response, Option<Box<CachePolicy>>),
}

/// Serialize the actual payload together with its caching information.
#[derive(Debug, Deserialize, Serialize)]
pub struct DataWithCachePolicy {
    pub data: Vec<u8>,
    /// Whether the response should be considered immutable.
    immutable: bool,
    /// The [`CachePolicy`] is used to determine if the response is fresh or stale.
    /// The policy is large (448 bytes at time of writing), so we reduce the stack size by
    /// boxing it.
    cache_policy: Box<CachePolicy>,
}

/// Custom caching layer over [`reqwest::Client`] using `http-cache-semantics`.
///
/// The implementation takes inspiration from the `http-cache` crate, but adds support for running
/// an async callback on the response before caching. We use this to e.g. store a
/// parsed version of the wheel metadata and for our remote zip reader. In the latter case, we want
/// to read a single file from a remote zip using range requests (so we don't have to download the
/// entire file). We send a HEAD request in the caching layer to check if the remote file has
/// changed (and if range requests are supported), and in the callback we make the actual range
/// requests if required.
///
/// Unlike `http-cache`, all outputs must be serde-able. Currently everything is json, but we can
/// transparently switch to a faster/smaller format.
///
/// Again unlike `http-cache`, the caller gets full control over the cache key with the assumption
/// that it's a file.
#[derive(Debug, Clone)]
pub struct CachedClient(ClientWithMiddleware);

impl CachedClient {
    pub fn new(client: ClientWithMiddleware) -> Self {
        Self(client)
    }

    /// The middleware is the retry strategy
    pub fn uncached(&self) -> ClientWithMiddleware {
        self.0.clone()
    }

    /// Make a cached request with a custom response transformation
    ///
    /// If a new response was received (no prior cached response or modified on the remote), the
    /// response is passed through `response_callback` and only the result is cached and returned.
    /// The `response_callback` is allowed to make subsequent requests, e.g. through the uncached
    /// client.
    #[instrument(skip_all)]
    pub async fn get_cached_with_callback<
        Payload: Serialize + DeserializeOwned + Send,
        CallBackError,
        Callback,
        CallbackReturn,
    >(
        &self,
        req: Request,
        cache_entry: &CacheEntry,
        cache_control: CacheControl,
        response_callback: Callback,
    ) -> Result<Payload, CachedClientError<CallBackError>>
    where
        Callback: FnOnce(Response) -> CallbackReturn,
        CallbackReturn: Future<Output = Result<Payload, CallBackError>>,
    {
        let payload = self
            .get_cached_with_callback2(req, cache_entry, cache_control, move |resp| async {
                let payload = response_callback(resp).await?;
                Ok(SerdeCacheable { inner: payload })
            })
            .await?;
        Ok(payload)
    }

    #[instrument(skip_all)]
    pub async fn get_cached_with_callback2<
        Payload: Cacheable,
        CallBackError,
        Callback,
        CallbackReturn,
    >(
        &self,
        req: Request,
        cache_entry: &CacheEntry,
        cache_control: CacheControl,
        response_callback: Callback,
    ) -> Result<Payload::Target, CachedClientError<CallBackError>>
    where
        Callback: FnOnce(Response) -> CallbackReturn,
        CallbackReturn: Future<Output = Result<Payload, CallBackError>>,
    {
        let read_span = info_span!("read_cache", file = %cache_entry.path().display());
        let read_result = fs_err::tokio::read(cache_entry.path())
            .instrument(read_span)
            .await;
        let cached = if let Ok(cached) = read_result {
            let parse_span = info_span!(
                "parse_cache",
                path = %cache_entry.path().display()
            );
            let parse_result =
                parse_span.in_scope(|| rmp_serde::from_slice::<DataWithCachePolicy>(&cached));
            match parse_result {
                Ok(data) => Some(data),
                Err(err) => {
                    warn!(
                        "Broken cache entry at {}, removing: {err}",
                        cache_entry.path().display()
                    );
                    let _ = fs_err::tokio::remove_file(&cache_entry.path()).await;
                    None
                }
            }
        } else {
            None
        };

        let cached_response = self.send_cached(req, cache_control, cached).boxed().await?;

        let write_cache = info_span!("write_cache", file = %cache_entry.path().display());
        match cached_response {
            CachedResponse::FreshCache(data) => Ok(Payload::from_bytes(data)?),
            CachedResponse::NotModified(data_with_cache_policy) => {
                async {
                    let data =
                        rmp_serde::to_vec(&data_with_cache_policy).map_err(ErrorKind::Encode)?;
                    write_atomic(cache_entry.path(), data)
                        .await
                        .map_err(ErrorKind::CacheWrite)?;
                    Ok(Payload::from_bytes(data_with_cache_policy.data)?)
                }
                .instrument(write_cache)
                .await
            }
            CachedResponse::ModifiedOrNew(res, cache_policy) => {
                let headers = CacheHeaders::from_response(res.headers().get_all("cache-control"));
                let immutable = headers.is_immutable();

                let data = response_callback(res)
                    .await
                    .map_err(|err| CachedClientError::Callback(err))?;
                if let Some(cache_policy) = cache_policy {
                    let data_with_cache_policy = DataWithCachePolicy {
                        data: data.to_bytes()?,
                        immutable,
                        cache_policy,
                    };
                    async {
                        fs_err::tokio::create_dir_all(cache_entry.dir())
                            .await
                            .map_err(ErrorKind::CacheWrite)?;
                        let envelope = rmp_serde::to_vec(&data_with_cache_policy)
                            .map_err(ErrorKind::Encode)?;
                        write_atomic(cache_entry.path(), envelope)
                            .await
                            .map_err(ErrorKind::CacheWrite)?;
                        Ok(data.into_target())
                    }
                    .instrument(write_cache)
                    .await
                } else {
                    Ok(data.into_target())
                }
            }
        }
    }

    /// `http-cache-semantics` to `reqwest` wrapper
    async fn send_cached(
        &self,
        mut req: Request,
        cache_control: CacheControl,
        cached: Option<DataWithCachePolicy>,
    ) -> Result<CachedResponse, Error> {
        // The converted types are from the specific `reqwest` types to the more generic `http`
        // types.
        let mut converted_req = http::Request::try_from(
            req.try_clone()
                .expect("You can't use streaming request bodies with this function"),
        )
        .map_err(ErrorKind::RequestError)?;

        let url = req.url().clone();
        let cached_response = if let Some(cached) = cached {
            // Avoid sending revalidation requests for immutable responses.
            if cached.immutable && !cached.cache_policy.is_stale(SystemTime::now()) {
                debug!("Found immutable response for: {url}");
                return Ok(CachedResponse::FreshCache(cached.data));
            }

            // Apply the cache control header, if necessary.
            match cache_control {
                CacheControl::None => {}
                CacheControl::MustRevalidate => {
                    converted_req.headers_mut().insert(
                        http::header::CACHE_CONTROL,
                        http::HeaderValue::from_static("max-age=0, must-revalidate"),
                    );
                }
            }

            match cached
                .cache_policy
                .before_request(&converted_req, SystemTime::now())
            {
                BeforeRequest::Fresh(_) => {
                    debug!("Found fresh response for: {url}");
                    CachedResponse::FreshCache(cached.data)
                }
                BeforeRequest::Stale { request, matches } => {
                    if !matches {
                        // This shouldn't happen; if it does, we'll override the cache.
                        warn!("Cached request doesn't match current request for: {url}");
                        return self.fresh_request(req, converted_req).await;
                    }

                    debug!("Sending revalidation request for: {url}");
                    for header in &request.headers {
                        req.headers_mut().insert(header.0.clone(), header.1.clone());
                        converted_req
                            .headers_mut()
                            .insert(header.0.clone(), header.1.clone());
                    }
                    let res = self
                        .0
                        .execute(req)
                        .instrument(info_span!("revalidation_request", url = url.as_str()))
                        .await
                        .map_err(ErrorKind::RequestMiddlewareError)?
                        .error_for_status()
                        .map_err(ErrorKind::RequestError)?;
                    let mut converted_res = http::Response::new(());
                    *converted_res.status_mut() = res.status();
                    for header in res.headers() {
                        converted_res.headers_mut().insert(
                            http::HeaderName::from(header.0),
                            http::HeaderValue::from(header.1),
                        );
                    }
                    let after_response = cached.cache_policy.after_response(
                        &converted_req,
                        &converted_res,
                        SystemTime::now(),
                    );
                    match after_response {
                        AfterResponse::NotModified(new_policy, _parts) => {
                            debug!("Found not-modified response for: {url}");
                            let headers =
                                CacheHeaders::from_response(res.headers().get_all("cache-control"));
                            let immutable = headers.is_immutable();
                            CachedResponse::NotModified(DataWithCachePolicy {
                                data: cached.data,
                                immutable,
                                cache_policy: Box::new(new_policy),
                            })
                        }
                        AfterResponse::Modified(new_policy, _parts) => {
                            debug!("Found modified response for: {url}");
                            CachedResponse::ModifiedOrNew(
                                res,
                                new_policy.is_storable().then(|| Box::new(new_policy)),
                            )
                        }
                    }
                }
            }
        } else {
            debug!("No cache entry for: {url}");
            self.fresh_request(req, converted_req).await?
        };
        Ok(cached_response)
    }

    #[instrument(skip_all, fields(url = req.url().as_str()))]
    async fn fresh_request(
        &self,
        req: Request,
        converted_req: http::Request<reqwest::Body>,
    ) -> Result<CachedResponse, Error> {
        trace!("{} {}", req.method(), req.url());
        let res = self
            .0
            .execute(req)
            .await
            .map_err(ErrorKind::RequestMiddlewareError)?
            .error_for_status()
            .map_err(ErrorKind::RequestError)?;
        let mut converted_res = http::Response::new(());
        *converted_res.status_mut() = res.status();
        for header in res.headers() {
            converted_res.headers_mut().insert(
                http::HeaderName::from(header.0),
                http::HeaderValue::from(header.1),
            );
        }
        let cache_policy =
            CachePolicy::new(&converted_req.into_parts().0, &converted_res.into_parts().0);
        Ok(CachedResponse::ModifiedOrNew(
            res,
            cache_policy.is_storable().then(|| Box::new(cache_policy)),
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CacheControl {
    /// Respect the `cache-control` header from the response.
    None,
    /// Apply `max-age=0, must-revalidate` to the request.
    MustRevalidate,
}

impl From<Freshness> for CacheControl {
    fn from(value: Freshness) -> Self {
        match value {
            Freshness::Fresh => CacheControl::None,
            Freshness::Stale => CacheControl::MustRevalidate,
            Freshness::Missing => CacheControl::None,
        }
    }
}
