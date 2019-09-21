use super::{
    service::{layer_fn, BoxedIo},
    tls::{Cert, TlsAcceptor},
};
use crate::BoxBody;
use futures_core::Stream;
use futures_util::{ready, try_future::MapErr, TryFutureExt, TryStreamExt};
use http::{Request, Response};
use hyper::server::{accept::Accept, conn};
use hyper::Body;
use std::sync::Arc;
use std::{
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tower::layer::util::Stack;
use tower::layer::Layer;
use tower::util::Either;
use tower_make::MakeService;
use tower_service::Service;

type BoxService = tower::util::BoxService<Request<Body>, Response<BoxBody>, crate::Error>;
type Interceptor = Arc<dyn Layer<BoxService, Service = BoxService> + Send + Sync + 'static>;

#[derive(Debug)]
pub struct Server {}

impl Server {
    pub fn builder() -> Builder {
        Builder::new()
    }
}

#[derive(Default)]
pub struct Builder {
    tls: Option<(Vec<u8>, Vec<u8>)>,
    interceptor: Option<Interceptor>,
}

impl Builder {
    fn new() -> Self {
        Default::default()
    }

    pub fn tls(&mut self, pem: Vec<u8>, key: Vec<u8>) -> &mut Self {
        self.tls = Some((pem, key));
        self
    }

    // pub fn concurrency_limit(&mut self, limit: usize) -> &mut Self {
    // }

    pub fn interceptor_fn<F, Out>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&mut BoxService, Request<Body>) -> Out + Send + Sync + 'static,
        Out: Future<Output = Result<Response<BoxBody>, crate::Error>> + Send + 'static,
    {
        let f = Arc::new(f);
        let interceptor = layer_fn(move |mut s| {
            let f = f.clone();
            tower::service_fn(move |req| f(&mut s, req))
        });
        let layer = Stack::new(interceptor, layer_fn(|s| BoxService::new(s)));
        self.interceptor = Some(Arc::new(layer));
        self
    }

    pub async fn serve<M, S>(self, addr: SocketAddr, svc: M) -> Result<(), super::Error>
    where
        M: Service<(), Response = S>,
        M::Error: Into<crate::Error> + Send + 'static,
        M::Future: Send + 'static,
        S: Service<Request<Body>, Response = Response<BoxBody>> + Send + 'static,
        S::Future: Send + 'static,
        S::Error: Into<crate::Error> + Send,
    {
        let tls = if let Some(tls) = self.tls {
            let cert = Cert {
                ca: tls.0,
                key: Some(tls.1),
                domain: String::new(),
            };

            Some(TlsAcceptor::new(cert).unwrap())
        } else {
            None
        };

        let incoming = hyper::server::accept::from_stream(incoming(addr, tls));

        let svc = MakeSvc {
            inner: svc,
            interceptor: self.interceptor.clone(),
        };

        hyper::Server::builder(incoming)
            .http2_only(true)
            .serve(svc)
            .await
            .unwrap();

        Ok(())
    }
}

impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Builder").finish()
    }
}

fn incoming(
    addr: SocketAddr,
    tls: Option<TlsAcceptor>,
) -> impl futures_core::Stream<Item = Result<BoxedIo, crate::Error>> {
    async_stream::try_stream! {
        let mut tcp = TcpIncoming::bind(addr)?;

        while let Some(stream) = tcp.try_next().await? {
            if let Some(tls) = &tls {
                let io = tls.connect(stream.into_inner()).await?;
                yield BoxedIo::new(io);
            } else {
                yield BoxedIo::new(stream);
            }
        }
    }
}

#[derive(Debug)]
struct TcpIncoming {
    inner: conn::AddrIncoming,
}

impl TcpIncoming {
    fn bind(addr: SocketAddr) -> Result<Self, crate::Error> {
        let inner = conn::AddrIncoming::bind(&addr).map_err(Box::new)?;

        Ok(Self { inner })
    }
}

impl Stream for TcpIncoming {
    type Item = Result<conn::AddrStream, crate::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(Accept::poll_accept(Pin::new(&mut self.inner), cx)) {
            Some(Ok(s)) => Poll::Ready(Some(Ok(s))),
            Some(Err(e)) => Poll::Ready(Some(Err(e.into()))),
            None => Poll::Ready(None),
        }
    }
}

// TODO: add custom tracing here
#[derive(Debug)]
struct Svc<S>(S);

impl<S> Service<Request<Body>> for Svc<S>
where
    S: Service<Request<Body>, Response = Response<BoxBody>>,
    S::Error: Into<crate::Error>,
{
    type Response = Response<BoxBody>;
    type Error = crate::Error;
    type Future = MapErr<S::Future, fn(S::Error) -> crate::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        self.0.call(req).map_err(|e| e.into())
    }
}

struct MakeSvc<M> {
    interceptor: Option<Interceptor>,
    inner: M,
}

impl<M, S, T> Service<T> for MakeSvc<M>
where
    M: Service<(), Response = S>,
    M::Error: Into<crate::Error> + Send,
    M::Future: Send + 'static,
    S: Service<Request<Body>, Response = Response<BoxBody>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<crate::Error> + Send,
{
    type Response = Either<Svc<S>, BoxService>;
    type Error = crate::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        MakeService::poll_ready(&mut self.inner, cx).map_err(Into::into)
    }

    fn call(&mut self, _: T) -> Self::Future {
        // self.inner.make_service(()).map_ok(|s| Svc(s))
        let interceptor = self.interceptor.clone();
        // self.inner.make_service(()).map_ok(|s| intercept.layer(BoxService::new(Svc(s))))
        let make = self.inner.make_service(());
        Box::pin(async move {
            let svc = make.await.map_err(Into::into)?;

            if let Some(interceptor) = interceptor {
                let layered = interceptor.layer(BoxService::new(Svc(svc)));
                Ok(Either::B(layered))
            } else {
                Ok(Either::A(Svc(svc)))
            }
        })
    }
}