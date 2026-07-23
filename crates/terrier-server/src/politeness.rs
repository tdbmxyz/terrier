//! Per-source politeness as a `tower::Layer`: a minimum delay between
//! requests plus a concurrency cap. One layer instance per source; shared
//! by clone. Reusable over any tower `Service` (here: the reqwest adapter).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

#[derive(Clone)]
pub struct PolitenessLayer {
    state: Arc<State>,
}

struct State {
    min_delay: Duration,
    semaphore: Arc<Semaphore>,
    last_request: Mutex<Option<Instant>>,
}

impl PolitenessLayer {
    pub fn new(min_delay: Duration, max_concurrency: usize) -> Self {
        Self {
            state: Arc::new(State {
                min_delay,
                semaphore: Arc::new(Semaphore::new(max_concurrency.max(1))),
                last_request: Mutex::new(None),
            }),
        }
    }
}

impl<S> tower::Layer<S> for PolitenessLayer {
    type Service = Politeness<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Politeness {
            inner,
            state: self.state.clone(),
        }
    }
}

#[derive(Clone)]
pub struct Politeness<S> {
    inner: S,
    state: Arc<State>,
}

impl<S, R> tower::Service<R> for Politeness<S>
where
    S: tower::Service<R> + Clone + Send + 'static,
    S::Future: Send,
    R: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let state = self.state.clone();
        // Take the ready inner service, leave a fresh clone behind
        // (standard tower pattern for boxed futures).
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let _permit = state
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("politeness semaphore closed");
            {
                let mut last = state.last_request.lock().await;
                if let Some(prev) = *last {
                    let elapsed = prev.elapsed();
                    if elapsed < state.min_delay {
                        tokio::time::sleep(state.min_delay - elapsed).await;
                    }
                }
                *last = Some(Instant::now());
            }
            inner.call(req).await
        })
    }
}

/// reqwest as a tower `Service` so politeness (and any future middleware)
/// can wrap the outbound scraping client.
#[derive(Clone)]
pub struct HttpService {
    client: reqwest::Client,
}

impl HttpService {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("terrier/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("building scrape http client"),
        }
    }
}

impl tower::Service<reqwest::Request> for HttpService {
    type Response = reqwest::Response;
    type Error = reqwest::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: reqwest::Request) -> Self::Future {
        let client = self.client.clone();
        Box::pin(async move { client.execute(req).await })
    }
}

/// The polite per-source scrape client: politeness layer over reqwest.
pub type ScrapeClient = Politeness<HttpService>;

pub fn scrape_client(min_delay: Duration, max_concurrency: usize) -> ScrapeClient {
    use tower::Layer as _;
    PolitenessLayer::new(min_delay, max_concurrency).layer(HttpService::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::time::Duration;
    use tokio::time::Instant;
    use tower::{Layer, Service, ServiceExt, service_fn};

    #[tokio::test(start_paused = true)]
    async fn enforces_min_delay_between_calls() {
        let inner = service_fn(|_req: ()| async { Ok::<_, Infallible>(()) });
        let mut svc = PolitenessLayer::new(Duration::from_millis(500), 1).layer(inner);

        let start = Instant::now();
        svc.ready().await.unwrap().call(()).await.unwrap();
        let first = start.elapsed();
        svc.ready().await.unwrap().call(()).await.unwrap();
        let second = start.elapsed();

        assert!(first < Duration::from_millis(10), "first call is immediate");
        assert!(
            second >= Duration::from_millis(500),
            "second call waited the delay, got {second:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn caps_concurrency() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (fl, pk) = (in_flight.clone(), peak.clone());
        let inner = service_fn(move |_req: ()| {
            let (fl, pk) = (fl.clone(), pk.clone());
            async move {
                let now = fl.fetch_add(1, Ordering::SeqCst) + 1;
                pk.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                fl.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, Infallible>(())
            }
        });
        let svc = PolitenessLayer::new(Duration::ZERO, 2).layer(inner);

        let futs: Vec<_> = (0..6)
            .map(|_| {
                let mut svc = svc.clone();
                tokio::spawn(async move { svc.ready().await.unwrap().call(()).await.unwrap() })
            })
            .collect();
        for f in futs {
            f.await.unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 2, "peak concurrency ≤ cap");
    }
}
