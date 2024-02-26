#![feature(prelude_import)]
//! Asynchronous [Language Server Protocol (LSP)][lsp] framework based on [tower].
//!
//! See project [README] for a general overview.
//!
//! [README]: https://github.com/oxalica/async-lsp#readme
//! [lsp]: https://microsoft.github.io/language-server-protocol/overviews/lsp/overview/
//! [tower]: https://github.com/tower-rs/tower
//!
//! This project is centered at a core service trait [`LspService`] for either Language Servers or
//! Language Clients. The main loop driver [`MainLoop`] executes the service. The additional
//! features, called middleware, are pluggable can be layered using the [`tower_layer`]
//! abstraction. This crate defines several common middlewares for various mandatory or optional
//! LSP functionalities, see their documentations for details.
//! - [`concurrency::Concurrency`]: Incoming request multiplexing and cancellation.
//! - [`panic::CatchUnwind`]: Turn panics into errors.
//! - [`tracing::Tracing`]: Logger spans with methods instrumenting handlers.
//! - [`server::Lifecycle`]: Server initialization, shutting down, and exit handling.
//! - [`client_monitor::ClientProcessMonitor`]: Client process monitor.
//! - [`router::Router`]: "Root" service to dispatch requests, notifications and events.
//!
//! Users are free to select and layer middlewares to run a Language Server or Language Client.
//! They can also implement their own middlewares for like timeout, metering, request
//! transformation and etc.
//!
//! ## Usages
//!
//! There are two main ways to define a [`Router`](router::Router) root service: one is via its
//! builder API, and the other is to construct via implementing the omnitrait [`LanguageServer`] or
//! [`LanguageClient`] for a state struct. The former is more flexible, while the latter has a
//! more similar API as [`tower-lsp`](https://crates.io/crates/tower-lsp).
//!
//! The examples for both builder-API and omnitrait, cross Language Server and Language Client, can
//! be seen under
//![`examples`](https://github.com/oxalica/async-lsp/tree/v0.2.0/examples)
//! directory.
//!
//! ## Cargo features
//!
//! - `client-monitor`: Client process monitor middleware [`client_monitor`].
//!   *Enabled by default.*
//! - `omni-trait`: Mega traits of all standard requests and notifications, namely
//!   [`LanguageServer`] and [`LanguageClient`].
//!   *Enabled by default.*
//! - `stdio`: Utilities to deal with pipe-like stdin/stdout communication channel for Language
//!   Servers.
//!   *Enabled by default.*
//! - `tracing`: Integration with crate [`tracing`][::tracing] and the [`tracing`] middleware.
//!   *Enabled by default.*
//! - `forward`: Impl [`LspService`] for `{Client,Server}Socket`. This collides some method names
//!   but allows easy service forwarding. See `examples/inspector.rs` for a possible use case.
//!   *Disabled by default.*
//! - `tokio`: Enable compatible methods for [`tokio`](https://crates.io/crates/tokio) runtime.
//!   *Disabled by default.*
#![warn(missing_docs)]
#[prelude_import]
use std::prelude::rust_2021::*;
#[macro_use]
extern crate std;
use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::{fmt, io};
use futures::channel::{mpsc, oneshot};
use futures::io::BufReader;
use futures::stream::FuturesUnordered;
use futures::{
    pin_mut, select_biased, AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt,
    AsyncWrite, AsyncWriteExt, FutureExt, SinkExt, StreamExt,
};
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use lsp_types::NumberOrString;
use pin_project_lite::pin_project;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;
use tower_service::Service;
/// Re-export of the [`lsp_types`] dependency of this crate.
pub use lsp_types;
pub mod concurrency {
    //! Incoming request multiplexing limits and cancellation.
    //!
    //! *Applies to both Language Servers and Language Clients.*
    //!
    //! Note that the [`crate::MainLoop`] can poll multiple ongoing requests
    //! out-of-box, while this middleware is to provides these additional features:
    //! 1. Limit concurrent incoming requests to at most `max_concurrency`.
    //! 2. Cancellation of incoming requests via client notification `$/cancelRequest`.
    use std::collections::HashMap;
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::ops::ControlFlow;
    use std::pin::Pin;
    use std::sync::{Arc, Weak};
    use std::task::{Context, Poll};
    use std::thread::available_parallelism;
    use futures::stream::{AbortHandle, Abortable};
    use futures::task::AtomicWaker;
    use lsp_types::notification::{self, Notification};
    use pin_project_lite::pin_project;
    use tower_layer::Layer;
    use tower_service::Service;
    use crate::{
        AnyEvent, AnyNotification, AnyRequest, ErrorCode, LspService, RequestId,
        ResponseError, Result,
    };
    /// The middleware for incoming request multiplexing limits and cancellation.
    ///
    /// See [module level documentations](self) for details.
    pub struct Concurrency<S> {
        service: S,
        max_concurrency: NonZeroUsize,
        /// A specialized single-acquire-multiple-release semaphore, using `Arc::weak_count` as tokens.
        semaphore: Arc<AtomicWaker>,
        ongoing: HashMap<RequestId, AbortHandle>,
    }
    impl<S> Concurrency<S> {
        /// Get a reference to the inner service.
        #[must_use]
        pub fn get_ref(&self) -> &S {
            &self.service
        }
        /// Get a mutable reference to the inner service.
        #[must_use]
        pub fn get_mut(&mut self) -> &mut S {
            &mut self.service
        }
        /// Consume self, returning the inner service.
        #[must_use]
        pub fn into_inner(self) -> S {
            self.service
        }
    }
    impl<S: LspService> Service<AnyRequest> for Concurrency<S>
    where
        S::Error: From<ResponseError>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future>;
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if Arc::weak_count(&self.semaphore) >= self.max_concurrency.get() {
                self.semaphore.register(cx.waker());
                if Arc::weak_count(&self.semaphore) >= self.max_concurrency.get() {
                    return Poll::Pending;
                }
            }
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: AnyRequest) -> Self::Future {
            let guard = SemaphoreGuard(Arc::downgrade(&self.semaphore));
            if true {
                if !(Arc::weak_count(&self.semaphore) <= self.max_concurrency.get()) {
                    {
                        ::core::panicking::panic_fmt(
                            format_args!("`poll_ready` is not called before `call`"),
                        );
                    }
                }
            }
            let (handle, registration) = AbortHandle::new_pair();
            if self.ongoing.len() >= self.max_concurrency.get() * 2 {
                self.ongoing.retain(|_, handle| !handle.is_aborted());
            }
            self.ongoing.insert(req.id.clone(), handle.clone());
            let fut = self.service.call(req);
            let fut = Abortable::new(fut, registration);
            ResponseFuture {
                fut,
                _abort_on_drop: AbortOnDrop(handle),
                _guard: guard,
            }
        }
    }
    struct SemaphoreGuard(Weak<AtomicWaker>);
    impl Drop for SemaphoreGuard {
        fn drop(&mut self) {
            if let Some(sema) = self.0.upgrade() {
                if let Some(waker) = sema.take() {
                    drop(sema);
                    waker.wake();
                }
            }
        }
    }
    /// By default, the `AbortHandle` only transfers information from it to `Abortable<_>`, not in
    /// reverse. But we want to set the flag on drop (either success or failure), so that the `ongoing`
    /// map can be purged regularly without bloating indefinitely.
    struct AbortOnDrop(AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    /// The [`Future`] type used by the [`Concurrency`] middleware.
    pub struct ResponseFuture<Fut> {
        fut: Abortable<Fut>,
        _abort_on_drop: AbortOnDrop,
        _guard: SemaphoreGuard,
    }
    #[allow(explicit_outlives_requirements)]
    #[allow(single_use_lifetimes)]
    #[allow(clippy::unknown_clippy_lints)]
    #[allow(clippy::redundant_pub_crate)]
    #[allow(clippy::used_underscore_binding)]
    const _: () = {
        #[doc(hidden)]
        #[allow(dead_code)]
        #[allow(single_use_lifetimes)]
        #[allow(clippy::unknown_clippy_lints)]
        #[allow(clippy::mut_mut)]
        #[allow(clippy::redundant_pub_crate)]
        #[allow(clippy::ref_option_ref)]
        #[allow(clippy::type_repetition_in_bounds)]
        pub(crate) struct Projection<'__pin, Fut>
        where
            ResponseFuture<Fut>: '__pin,
        {
            fut: ::pin_project_lite::__private::Pin<&'__pin mut (Abortable<Fut>)>,
            _abort_on_drop: &'__pin mut (AbortOnDrop),
            _guard: &'__pin mut (SemaphoreGuard),
        }
        #[doc(hidden)]
        #[allow(dead_code)]
        #[allow(single_use_lifetimes)]
        #[allow(clippy::unknown_clippy_lints)]
        #[allow(clippy::mut_mut)]
        #[allow(clippy::redundant_pub_crate)]
        #[allow(clippy::ref_option_ref)]
        #[allow(clippy::type_repetition_in_bounds)]
        pub(crate) struct ProjectionRef<'__pin, Fut>
        where
            ResponseFuture<Fut>: '__pin,
        {
            fut: ::pin_project_lite::__private::Pin<&'__pin (Abortable<Fut>)>,
            _abort_on_drop: &'__pin (AbortOnDrop),
            _guard: &'__pin (SemaphoreGuard),
        }
        impl<Fut> ResponseFuture<Fut> {
            #[doc(hidden)]
            #[inline]
            pub(crate) fn project<'__pin>(
                self: ::pin_project_lite::__private::Pin<&'__pin mut Self>,
            ) -> Projection<'__pin, Fut> {
                unsafe {
                    let Self { fut, _abort_on_drop, _guard } = self.get_unchecked_mut();
                    Projection {
                        fut: ::pin_project_lite::__private::Pin::new_unchecked(fut),
                        _abort_on_drop: _abort_on_drop,
                        _guard: _guard,
                    }
                }
            }
            #[doc(hidden)]
            #[inline]
            pub(crate) fn project_ref<'__pin>(
                self: ::pin_project_lite::__private::Pin<&'__pin Self>,
            ) -> ProjectionRef<'__pin, Fut> {
                unsafe {
                    let Self { fut, _abort_on_drop, _guard } = self.get_ref();
                    ProjectionRef {
                        fut: ::pin_project_lite::__private::Pin::new_unchecked(fut),
                        _abort_on_drop: _abort_on_drop,
                        _guard: _guard,
                    }
                }
            }
        }
        #[allow(non_snake_case)]
        pub struct __Origin<'__pin, Fut> {
            __dummy_lifetime: ::pin_project_lite::__private::PhantomData<&'__pin ()>,
            fut: Abortable<Fut>,
            _abort_on_drop: ::pin_project_lite::__private::AlwaysUnpin<AbortOnDrop>,
            _guard: ::pin_project_lite::__private::AlwaysUnpin<SemaphoreGuard>,
        }
        impl<'__pin, Fut> ::pin_project_lite::__private::Unpin for ResponseFuture<Fut>
        where
            __Origin<'__pin, Fut>: ::pin_project_lite::__private::Unpin,
        {}
        trait MustNotImplDrop {}
        #[allow(clippy::drop_bounds, drop_bounds)]
        impl<T: ::pin_project_lite::__private::Drop> MustNotImplDrop for T {}
        impl<Fut> MustNotImplDrop for ResponseFuture<Fut> {}
        #[forbid(unaligned_references, safe_packed_borrows)]
        fn __assert_not_repr_packed<Fut>(this: &ResponseFuture<Fut>) {
            let _ = &this.fut;
            let _ = &this._abort_on_drop;
            let _ = &this._guard;
        }
    };
    impl<Fut, Response, Error> Future for ResponseFuture<Fut>
    where
        Fut: Future<Output = Result<Response, Error>>,
        Error: From<ResponseError>,
    {
        type Output = Fut::Output;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.project().fut.poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(inner_ret)) => Poll::Ready(inner_ret),
                Poll::Ready(Err(_aborted)) => {
                    Poll::Ready(
                        Err(
                            ResponseError {
                                code: ErrorCode::REQUEST_CANCELLED,
                                message: "Client cancelled the request".into(),
                                data: None,
                            }
                                .into(),
                        ),
                    )
                }
            }
        }
    }
    impl<S: LspService> LspService for Concurrency<S>
    where
        S::Error: From<ResponseError>,
    {
        fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
            if notif.method == notification::Cancel::METHOD {
                if let Ok(params) = serde_json::from_value::<
                    lsp_types::CancelParams,
                >(notif.params) {
                    self.ongoing.remove(&params.id);
                }
                return ControlFlow::Continue(());
            }
            self.service.notify(notif)
        }
        fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
            self.service.emit(event)
        }
    }
    /// The builder of [`Concurrency`] middleware.
    ///
    /// It's [`Default`] configuration has `max_concurrency` of the result of
    /// [`std::thread::available_parallelism`], fallback to `1` if it fails.
    ///
    /// See [module level documentations](self) for details.
    #[must_use]
    pub struct ConcurrencyBuilder {
        max_concurrency: NonZeroUsize,
    }
    #[automatically_derived]
    impl ::core::clone::Clone for ConcurrencyBuilder {
        #[inline]
        fn clone(&self) -> ConcurrencyBuilder {
            ConcurrencyBuilder {
                max_concurrency: ::core::clone::Clone::clone(&self.max_concurrency),
            }
        }
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for ConcurrencyBuilder {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field1_finish(
                f,
                "ConcurrencyBuilder",
                "max_concurrency",
                &&self.max_concurrency,
            )
        }
    }
    impl Default for ConcurrencyBuilder {
        fn default() -> Self {
            Self::new(available_parallelism().unwrap_or(NonZeroUsize::new(1).unwrap()))
        }
    }
    impl ConcurrencyBuilder {
        /// Create the middleware with concurrency limit `max_concurrency`.
        pub fn new(max_concurrency: NonZeroUsize) -> Self {
            Self { max_concurrency }
        }
    }
    /// A type alias of [`ConcurrencyBuilder`] conforming to the naming convention of [`tower_layer`].
    pub type ConcurrencyLayer = ConcurrencyBuilder;
    impl<S> Layer<S> for ConcurrencyBuilder {
        type Service = Concurrency<S>;
        fn layer(&self, inner: S) -> Self::Service {
            Concurrency {
                service: inner,
                max_concurrency: self.max_concurrency,
                semaphore: Arc::new(AtomicWaker::new()),
                ongoing: HashMap::with_capacity(
                    self
                        .max_concurrency
                        .get()
                        .checked_mul(2)
                        .expect("max_concurrency overflow"),
                ),
            }
        }
    }
}
pub mod panic {
    //! Catch panics of underlying handlers and turn them into error responses.
    //!
    //! *Applies to both Language Servers and Language Clients.*
    use std::any::Any;
    use std::future::Future;
    use std::ops::ControlFlow;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use pin_project_lite::pin_project;
    use tower_layer::Layer;
    use tower_service::Service;
    use crate::{
        AnyEvent, AnyNotification, AnyRequest, ErrorCode, LspService, ResponseError,
        Result,
    };
    /// The middleware catching panics of underlying handlers and turn them into error responses.
    ///
    /// See [module level documentations](self) for details.
    pub struct CatchUnwind<S: LspService> {
        service: S,
        handler: Handler<S::Error>,
    }
    impl<S: LspService> CatchUnwind<S> {
        /// Get a reference to the inner service.
        #[must_use]
        pub fn get_ref(&self) -> &S {
            &self.service
        }
        /// Get a mutable reference to the inner service.
        #[must_use]
        pub fn get_mut(&mut self) -> &mut S {
            &mut self.service
        }
        /// Consume self, returning the inner service.
        #[must_use]
        pub fn into_inner(self) -> S {
            self.service
        }
    }
    type Handler<E> = fn(method: &str, payload: Box<dyn Any + Send>) -> E;
    fn default_handler(method: &str, payload: Box<dyn Any + Send>) -> ResponseError {
        let msg = match payload.downcast::<String>() {
            Ok(msg) => *msg,
            Err(payload) => {
                match payload.downcast::<&'static str>() {
                    Ok(msg) => (*msg).into(),
                    Err(_payload) => "unknown".into(),
                }
            }
        };
        ResponseError {
            code: ErrorCode::INTERNAL_ERROR,
            message: {
                let res = ::alloc::fmt::format(
                    format_args!("Request handler of {0} panicked: {1}", method, msg),
                );
                res
            },
            data: None,
        }
    }
    impl<S: LspService> Service<AnyRequest> for CatchUnwind<S> {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future, S::Error>;
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.service.poll_ready(cx)
        }
        fn call(&mut self, req: AnyRequest) -> Self::Future {
            let method = req.method.clone();
            match catch_unwind(AssertUnwindSafe(|| self.service.call(req)))
                .map_err(|err| (self.handler)(&method, err))
            {
                Ok(fut) => {
                    ResponseFuture {
                        inner: ResponseFutureInner::Future {
                            fut,
                            method,
                            handler: self.handler,
                        },
                    }
                }
                Err(err) => {
                    ResponseFuture {
                        inner: ResponseFutureInner::Ready {
                            err: Some(err),
                        },
                    }
                }
            }
        }
    }
    /// The [`Future`] type used by the [`CatchUnwind`] middleware.
    pub struct ResponseFuture<Fut, Error> {
        inner: ResponseFutureInner<Fut, Error>,
    }
    #[allow(explicit_outlives_requirements)]
    #[allow(single_use_lifetimes)]
    #[allow(clippy::unknown_clippy_lints)]
    #[allow(clippy::redundant_pub_crate)]
    #[allow(clippy::used_underscore_binding)]
    const _: () = {
        #[doc(hidden)]
        #[allow(dead_code)]
        #[allow(single_use_lifetimes)]
        #[allow(clippy::unknown_clippy_lints)]
        #[allow(clippy::mut_mut)]
        #[allow(clippy::redundant_pub_crate)]
        #[allow(clippy::ref_option_ref)]
        #[allow(clippy::type_repetition_in_bounds)]
        pub(crate) struct Projection<'__pin, Fut, Error>
        where
            ResponseFuture<Fut, Error>: '__pin,
        {
            inner: ::pin_project_lite::__private::Pin<
                &'__pin mut (ResponseFutureInner<Fut, Error>),
            >,
        }
        #[doc(hidden)]
        #[allow(dead_code)]
        #[allow(single_use_lifetimes)]
        #[allow(clippy::unknown_clippy_lints)]
        #[allow(clippy::mut_mut)]
        #[allow(clippy::redundant_pub_crate)]
        #[allow(clippy::ref_option_ref)]
        #[allow(clippy::type_repetition_in_bounds)]
        pub(crate) struct ProjectionRef<'__pin, Fut, Error>
        where
            ResponseFuture<Fut, Error>: '__pin,
        {
            inner: ::pin_project_lite::__private::Pin<
                &'__pin (ResponseFutureInner<Fut, Error>),
            >,
        }
        impl<Fut, Error> ResponseFuture<Fut, Error> {
            #[doc(hidden)]
            #[inline]
            pub(crate) fn project<'__pin>(
                self: ::pin_project_lite::__private::Pin<&'__pin mut Self>,
            ) -> Projection<'__pin, Fut, Error> {
                unsafe {
                    let Self { inner } = self.get_unchecked_mut();
                    Projection {
                        inner: ::pin_project_lite::__private::Pin::new_unchecked(inner),
                    }
                }
            }
            #[doc(hidden)]
            #[inline]
            pub(crate) fn project_ref<'__pin>(
                self: ::pin_project_lite::__private::Pin<&'__pin Self>,
            ) -> ProjectionRef<'__pin, Fut, Error> {
                unsafe {
                    let Self { inner } = self.get_ref();
                    ProjectionRef {
                        inner: ::pin_project_lite::__private::Pin::new_unchecked(inner),
                    }
                }
            }
        }
        #[allow(non_snake_case)]
        pub struct __Origin<'__pin, Fut, Error> {
            __dummy_lifetime: ::pin_project_lite::__private::PhantomData<&'__pin ()>,
            inner: ResponseFutureInner<Fut, Error>,
        }
        impl<'__pin, Fut, Error> ::pin_project_lite::__private::Unpin
        for ResponseFuture<Fut, Error>
        where
            __Origin<'__pin, Fut, Error>: ::pin_project_lite::__private::Unpin,
        {}
        trait MustNotImplDrop {}
        #[allow(clippy::drop_bounds, drop_bounds)]
        impl<T: ::pin_project_lite::__private::Drop> MustNotImplDrop for T {}
        impl<Fut, Error> MustNotImplDrop for ResponseFuture<Fut, Error> {}
        #[forbid(unaligned_references, safe_packed_borrows)]
        fn __assert_not_repr_packed<Fut, Error>(this: &ResponseFuture<Fut, Error>) {
            let _ = &this.inner;
        }
    };
    enum ResponseFutureInner<Fut, Error> {
        Future { fut: Fut, method: String, handler: Handler<Error> },
        Ready { err: Option<Error> },
    }
    #[doc(hidden)]
    #[allow(dead_code)]
    #[allow(single_use_lifetimes)]
    #[allow(clippy::unknown_clippy_lints)]
    #[allow(clippy::mut_mut)]
    #[allow(clippy::redundant_pub_crate)]
    #[allow(clippy::ref_option_ref)]
    #[allow(clippy::type_repetition_in_bounds)]
    enum ResponseFutureProj<'__pin, Fut, Error>
    where
        ResponseFutureInner<Fut, Error>: '__pin,
    {
        Future {
            fut: ::pin_project_lite::__private::Pin<&'__pin mut (Fut)>,
            method: &'__pin mut (String),
            handler: &'__pin mut (Handler<Error>),
        },
        Ready { err: &'__pin mut (Option<Error>) },
    }
    #[allow(single_use_lifetimes)]
    #[allow(clippy::unknown_clippy_lints)]
    #[allow(clippy::used_underscore_binding)]
    const _: () = {
        impl<Fut, Error> ResponseFutureInner<Fut, Error> {
            #[doc(hidden)]
            #[inline]
            fn project<'__pin>(
                self: ::pin_project_lite::__private::Pin<&'__pin mut Self>,
            ) -> ResponseFutureProj<'__pin, Fut, Error> {
                unsafe {
                    match self.get_unchecked_mut() {
                        Self::Future { fut, method, handler } => {
                            ResponseFutureProj::Future {
                                fut: ::pin_project_lite::__private::Pin::new_unchecked(fut),
                                method: method,
                                handler: handler,
                            }
                        }
                        Self::Ready { err } => {
                            ResponseFutureProj::Ready {
                                err: err,
                            }
                        }
                    }
                }
            }
        }
        #[allow(non_snake_case)]
        struct __Origin<'__pin, Fut, Error> {
            __dummy_lifetime: ::pin_project_lite::__private::PhantomData<&'__pin ()>,
            Future: (
                Fut,
                ::pin_project_lite::__private::AlwaysUnpin<String>,
                ::pin_project_lite::__private::AlwaysUnpin<Handler<Error>>,
            ),
            Ready: (::pin_project_lite::__private::AlwaysUnpin<Option<Error>>),
        }
        impl<'__pin, Fut, Error> ::pin_project_lite::__private::Unpin
        for ResponseFutureInner<Fut, Error>
        where
            __Origin<'__pin, Fut, Error>: ::pin_project_lite::__private::Unpin,
        {}
        trait MustNotImplDrop {}
        #[allow(clippy::drop_bounds, drop_bounds)]
        impl<T: ::pin_project_lite::__private::Drop> MustNotImplDrop for T {}
        impl<Fut, Error> MustNotImplDrop for ResponseFutureInner<Fut, Error> {}
    };
    impl<Response, Fut, Error> Future for ResponseFuture<Fut, Error>
    where
        Fut: Future<Output = Result<Response, Error>>,
    {
        type Output = Fut::Output;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.project().inner.project() {
                ResponseFutureProj::Future { fut, method, handler } => {
                    match catch_unwind(AssertUnwindSafe(|| fut.poll(cx))) {
                        Ok(poll) => poll,
                        Err(payload) => Poll::Ready(Err(handler(method, payload))),
                    }
                }
                ResponseFutureProj::Ready { err } => {
                    Poll::Ready(Err(err.take().expect("Completed")))
                }
            }
        }
    }
    impl<S: LspService> LspService for CatchUnwind<S> {
        fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
            self.service.notify(notif)
        }
        fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
            self.service.emit(event)
        }
    }
    /// The builder of [`CatchUnwind`] middleware.
    ///
    /// It's [`Default`] configuration tries to downcast the panic payload into `String` or `&str`, and
    /// fallback to format it via [`std::fmt::Display`], as the error message.
    /// The error code is set to [`ErrorCode::INTERNAL_ERROR`].
    #[must_use]
    pub struct CatchUnwindBuilder<Error = ResponseError> {
        handler: Handler<Error>,
    }
    #[automatically_derived]
    impl<Error: ::core::clone::Clone> ::core::clone::Clone
    for CatchUnwindBuilder<Error> {
        #[inline]
        fn clone(&self) -> CatchUnwindBuilder<Error> {
            CatchUnwindBuilder {
                handler: ::core::clone::Clone::clone(&self.handler),
            }
        }
    }
    impl Default for CatchUnwindBuilder<ResponseError> {
        fn default() -> Self {
            Self::new_with_handler(default_handler)
        }
    }
    impl<Error> CatchUnwindBuilder<Error> {
        /// Create the builder of [`CatchUnwind`] middleware with a custom handler converting panic
        /// payloads into [`ResponseError`].
        pub fn new_with_handler(handler: Handler<Error>) -> Self {
            Self { handler }
        }
    }
    /// A type alias of [`CatchUnwindBuilder`] conforming to the naming convention of [`tower_layer`].
    pub type CatchUnwindLayer<Error = ResponseError> = CatchUnwindBuilder<Error>;
    impl<S: LspService> Layer<S> for CatchUnwindBuilder<S::Error> {
        type Service = CatchUnwind<S>;
        fn layer(&self, inner: S) -> Self::Service {
            CatchUnwind {
                service: inner,
                handler: self.handler,
            }
        }
    }
}
pub mod router {
    //! Dispatch requests and notifications to individual handlers.
    use std::any::TypeId;
    use std::collections::HashMap;
    use std::future::{ready, Future};
    use std::ops::ControlFlow;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use lsp_types::notification::Notification;
    use lsp_types::request::Request;
    use tower_service::Service;
    use crate::{
        AnyEvent, AnyNotification, AnyRequest, ErrorCode, JsonValue, LspService,
        ResponseError, Result,
    };
    /// A router dispatching requests and notifications to individual handlers.
    pub struct Router<St, Error = ResponseError> {
        state: St,
        req_handlers: HashMap<&'static str, BoxReqHandler<St, Error>>,
        notif_handlers: HashMap<&'static str, BoxNotifHandler<St>>,
        event_handlers: HashMap<TypeId, BoxEventHandler<St>>,
        unhandled_req: BoxReqHandler<St, Error>,
        unhandled_notif: BoxNotifHandler<St>,
        unhandled_event: BoxEventHandler<St>,
    }
    type BoxReqFuture<Error> = Pin<
        Box<dyn Future<Output = Result<JsonValue, Error>> + Send>,
    >;
    type BoxReqHandler<St, Error> = Box<
        dyn Fn(&mut St, AnyRequest) -> BoxReqFuture<Error> + Send,
    >;
    type BoxNotifHandler<St> = Box<
        dyn Fn(&mut St, AnyNotification) -> ControlFlow<Result<()>> + Send,
    >;
    type BoxEventHandler<St> = Box<
        dyn Fn(&mut St, AnyEvent) -> ControlFlow<Result<()>> + Send,
    >;
    impl<St, Error> Default for Router<St, Error>
    where
        St: Default,
        Error: From<ResponseError> + Send + 'static,
    {
        fn default() -> Self {
            Self::new(St::default())
        }
    }
    impl<St, Error> Router<St, Error>
    where
        Error: From<ResponseError> + Send + 'static,
    {
        /// Create a empty `Router`.
        #[must_use]
        pub fn new(state: St) -> Self {
            Self {
                state,
                req_handlers: HashMap::new(),
                notif_handlers: HashMap::new(),
                event_handlers: HashMap::new(),
                unhandled_req: Box::new(|_, req| {
                    Box::pin(
                        ready(
                            Err(
                                ResponseError {
                                    code: ErrorCode::METHOD_NOT_FOUND,
                                    message: {
                                        let res = ::alloc::fmt::format(
                                            format_args!("No such method {0}", req.method),
                                        );
                                        res
                                    },
                                    data: None,
                                }
                                    .into(),
                            ),
                        ),
                    )
                }),
                unhandled_notif: Box::new(|_, notif| {
                    if notif.method.starts_with("$/") {
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(
                            Err(
                                crate::Error::Routing({
                                    let res = ::alloc::fmt::format(
                                        format_args!("Unhandled notification: {0}", notif.method),
                                    );
                                    res
                                }),
                            ),
                        )
                    }
                }),
                unhandled_event: Box::new(|_, event| {
                    ControlFlow::Break(
                        Err(
                            crate::Error::Routing({
                                let res = ::alloc::fmt::format(
                                    format_args!("Unhandled event: {0:?}", event),
                                );
                                res
                            }),
                        ),
                    )
                }),
            }
        }
        /// Add an asynchronous request handler for a specific LSP request `R`.
        ///
        /// If handler for the method already exists, it replaces the old one.
        pub fn request<R: Request, Fut>(
            &mut self,
            handler: impl Fn(&mut St, R::Params) -> Fut + Send + 'static,
        ) -> &mut Self
        where
            Fut: Future<Output = Result<R::Result, Error>> + Send + 'static,
        {
            self.req_handlers
                .insert(
                    R::METHOD,
                    Box::new(move |state, req| match serde_json::from_value::<
                        R::Params,
                    >(req.params) {
                        Ok(params) => {
                            let fut = handler(state, params);
                            Box::pin(async move {
                                Ok(
                                    serde_json::to_value(fut.await?)
                                        .expect("Serialization failed"),
                                )
                            })
                        }
                        Err(err) => {
                            Box::pin(
                                ready(
                                    Err(
                                        ResponseError {
                                            code: ErrorCode::INVALID_PARAMS,
                                            message: {
                                                let res = ::alloc::fmt::format(
                                                    format_args!("Failed to deserialize parameters: {0}", err),
                                                );
                                                res
                                            },
                                            data: None,
                                        }
                                            .into(),
                                    ),
                                ),
                            )
                        }
                    }),
                );
            self
        }
        /// Add a synchronous request handler for a specific LSP notification `N`.
        ///
        /// If handler for the method already exists, it replaces the old one.
        pub fn notification<N: Notification>(
            &mut self,
            handler: impl Fn(
                &mut St,
                N::Params,
            ) -> ControlFlow<Result<()>> + Send + 'static,
        ) -> &mut Self {
            self.notif_handlers
                .insert(
                    N::METHOD,
                    Box::new(move |state, notif| match serde_json::from_value::<
                        N::Params,
                    >(notif.params) {
                        Ok(params) => handler(state, params),
                        Err(err) => ControlFlow::Break(Err(err.into())),
                    }),
                );
            self
        }
        /// Add a synchronous event handler for event type `E`.
        ///
        /// If handler for the method already exists, it replaces the old one.
        pub fn event<E: Send + 'static>(
            &mut self,
            handler: impl Fn(&mut St, E) -> ControlFlow<Result<()>> + Send + 'static,
        ) -> &mut Self {
            self.event_handlers
                .insert(
                    TypeId::of::<E>(),
                    Box::new(move |state, event| {
                        let event = event.downcast::<E>().expect("Checked TypeId");
                        handler(state, event)
                    }),
                );
            self
        }
        /// Set an asynchronous catch-all request handler for any requests with no corresponding handler
        /// for its `method`.
        ///
        /// There can only be a single catch-all request handler. New ones replace old ones.
        ///
        /// The default handler is to respond a error response with code
        /// [`ErrorCode::METHOD_NOT_FOUND`].
        pub fn unhandled_request<Fut>(
            &mut self,
            handler: impl Fn(&mut St, AnyRequest) -> Fut + Send + 'static,
        ) -> &mut Self
        where
            Fut: Future<Output = Result<JsonValue, Error>> + Send + 'static,
        {
            self
                .unhandled_req = Box::new(move |state, req| Box::pin(
                handler(state, req),
            ));
            self
        }
        /// Set a synchronous catch-all notification handler for any notifications with no
        /// corresponding handler for its `method`.
        ///
        /// There can only be a single catch-all notification handler. New ones replace old ones.
        ///
        /// The default handler is to do nothing for methods starting with `$/`, and break the main
        /// loop with [`Error::Routing`][crate::Error::Routing] for other methods. Typically
        /// notifications are critical and
        /// losing them can break state synchronization, easily leading to catastrophic failures after
        /// incorrect incremental changes.
        pub fn unhandled_notification(
            &mut self,
            handler: impl Fn(
                &mut St,
                AnyNotification,
            ) -> ControlFlow<Result<()>> + Send + 'static,
        ) -> &mut Self {
            self.unhandled_notif = Box::new(handler);
            self
        }
        /// Set a synchronous catch-all event handler for any notifications with no
        /// corresponding handler for its type.
        ///
        /// There can only be a single catch-all event handler. New ones replace old ones.
        ///
        /// The default handler is to break the main loop with
        /// [`Error::Routing`][crate::Error::Routing]. Since events are
        /// emitted internally, mishandling are typically logic errors.
        pub fn unhandled_event(
            &mut self,
            handler: impl Fn(
                &mut St,
                AnyEvent,
            ) -> ControlFlow<Result<()>> + Send + 'static,
        ) -> &mut Self {
            self.unhandled_event = Box::new(handler);
            self
        }
    }
    impl<St, Error> Service<AnyRequest> for Router<St, Error> {
        type Response = JsonValue;
        type Error = Error;
        type Future = BoxReqFuture<Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: AnyRequest) -> Self::Future {
            let h = self.req_handlers.get(&*req.method).unwrap_or(&self.unhandled_req);
            h(&mut self.state, req)
        }
    }
    impl<St> LspService for Router<St> {
        fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
            let h = self
                .notif_handlers
                .get(&*notif.method)
                .unwrap_or(&self.unhandled_notif);
            h(&mut self.state, notif)
        }
        fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
            let h = self
                .event_handlers
                .get(&event.inner_type_id())
                .unwrap_or(&self.unhandled_event);
            h(&mut self.state, event)
        }
    }
}
pub mod server {
    //! Language Server lifecycle.
    //!
    //! *Only applies to Language Servers.*
    //!
    //! This middleware handles
    //! [the lifecycle of Language Servers](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#lifeCycleMessages),
    //! specifically:
    //! - Exit the main loop with `ControlFlow::Break(Ok(()))` on `exit` notification.
    //! - Responds unrelated requests with errors and ignore unrelated notifications during
    //!   initialization and shutting down.
    use std::future::{ready, Future, Ready};
    use std::ops::ControlFlow;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use futures::future::Either;
    use lsp_types::notification::{self, Notification};
    use lsp_types::request::{self, Request};
    use pin_project_lite::pin_project;
    use tower_layer::Layer;
    use tower_service::Service;
    use crate::{
        AnyEvent, AnyNotification, AnyRequest, Error, ErrorCode, LspService,
        ResponseError, Result,
    };
    enum State {
        #[default]
        Uninitialized,
        Initializing,
        Ready,
        ShuttingDown,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for State {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::write_str(
                f,
                match self {
                    State::Uninitialized => "Uninitialized",
                    State::Initializing => "Initializing",
                    State::Ready => "Ready",
                    State::ShuttingDown => "ShuttingDown",
                },
            )
        }
    }
    #[automatically_derived]
    impl ::core::default::Default for State {
        #[inline]
        fn default() -> State {
            Self::Uninitialized
        }
    }
    #[automatically_derived]
    impl ::core::clone::Clone for State {
        #[inline]
        fn clone(&self) -> State {
            *self
        }
    }
    #[automatically_derived]
    impl ::core::marker::Copy for State {}
    #[automatically_derived]
    impl ::core::marker::StructuralPartialEq for State {}
    #[automatically_derived]
    impl ::core::cmp::PartialEq for State {
        #[inline]
        fn eq(&self, other: &State) -> bool {
            let __self_tag = ::core::intrinsics::discriminant_value(self);
            let __arg1_tag = ::core::intrinsics::discriminant_value(other);
            __self_tag == __arg1_tag
        }
    }
    #[automatically_derived]
    impl ::core::marker::StructuralEq for State {}
    #[automatically_derived]
    impl ::core::cmp::Eq for State {
        #[inline]
        #[doc(hidden)]
        #[coverage(off)]
        fn assert_receiver_is_total_eq(&self) -> () {}
    }
    /// The middleware handling Language Server lifecycle.
    ///
    /// See [module level documentations](self) for details.
    pub struct Lifecycle<S> {
        service: S,
        state: State,
    }
    #[automatically_derived]
    impl<S: ::core::fmt::Debug> ::core::fmt::Debug for Lifecycle<S> {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field2_finish(
                f,
                "Lifecycle",
                "service",
                &self.service,
                "state",
                &&self.state,
            )
        }
    }
    #[automatically_derived]
    impl<S: ::core::default::Default> ::core::default::Default for Lifecycle<S> {
        #[inline]
        fn default() -> Lifecycle<S> {
            Lifecycle {
                service: ::core::default::Default::default(),
                state: ::core::default::Default::default(),
            }
        }
    }
    impl<S> Lifecycle<S> {
        /// Get a reference to the inner service.
        #[must_use]
        pub fn get_ref(&self) -> &S {
            &self.service
        }
        /// Get a mutable reference to the inner service.
        #[must_use]
        pub fn get_mut(&mut self) -> &mut S {
            &mut self.service
        }
        /// Consume self, returning the inner service.
        #[must_use]
        pub fn into_inner(self) -> S {
            self.service
        }
    }
    impl<S> Lifecycle<S> {
        /// Creating the `Lifecycle` middleware in uninitialized state.
        #[must_use]
        pub fn new(service: S) -> Self {
            Self {
                service,
                state: State::Uninitialized,
            }
        }
    }
    impl<S: LspService> Service<AnyRequest> for Lifecycle<S>
    where
        S::Error: From<ResponseError>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = ResponseFuture<S::Future>;
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.service.poll_ready(cx)
        }
        fn call(&mut self, req: AnyRequest) -> Self::Future {
            let inner = match (self.state, &*req.method) {
                (State::Uninitialized, request::Initialize::METHOD) => {
                    self.state = State::Initializing;
                    Either::Left(self.service.call(req))
                }
                (State::Uninitialized | State::Initializing, _) => {
                    Either::Right(
                        ready(
                            Err(
                                ResponseError {
                                    code: ErrorCode::SERVER_NOT_INITIALIZED,
                                    message: "Server is not initialized yet".into(),
                                    data: None,
                                }
                                    .into(),
                            ),
                        ),
                    )
                }
                (_, request::Initialize::METHOD) => {
                    Either::Right(
                        ready(
                            Err(
                                ResponseError {
                                    code: ErrorCode::INVALID_REQUEST,
                                    message: "Server is already initialized".into(),
                                    data: None,
                                }
                                    .into(),
                            ),
                        ),
                    )
                }
                (State::Ready, _) => {
                    if req.method == request::Shutdown::METHOD {
                        self.state = State::ShuttingDown;
                    }
                    Either::Left(self.service.call(req))
                }
                (State::ShuttingDown, _) => {
                    Either::Right(
                        ready(
                            Err(
                                ResponseError {
                                    code: ErrorCode::INVALID_REQUEST,
                                    message: "Server is shutting down".into(),
                                    data: None,
                                }
                                    .into(),
                            ),
                        ),
                    )
                }
            };
            ResponseFuture { inner }
        }
    }
    impl<S: LspService> LspService for Lifecycle<S>
    where
        S::Error: From<ResponseError>,
    {
        fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
            match &*notif.method {
                notification::Exit::METHOD => {
                    self.service.notify(notif)?;
                    ControlFlow::Break(Ok(()))
                }
                notification::Initialized::METHOD => {
                    if self.state != State::Initializing {
                        return ControlFlow::Break(
                            Err(
                                Error::Protocol({
                                    let res = ::alloc::fmt::format(
                                        format_args!(
                                            "Unexpected initialized notification on state {0:?}",
                                            self.state,
                                        ),
                                    );
                                    res
                                }),
                            ),
                        );
                    }
                    self.state = State::Ready;
                    self.service.notify(notif)?;
                    ControlFlow::Continue(())
                }
                _ => self.service.notify(notif),
            }
        }
        fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
            self.service.emit(event)
        }
    }
    /// The [`Future`] type used by the [`Lifecycle`] middleware.
    pub struct ResponseFuture<Fut: Future> {
        inner: Either<Fut, Ready<Fut::Output>>,
    }
    #[allow(explicit_outlives_requirements)]
    #[allow(single_use_lifetimes)]
    #[allow(clippy::unknown_clippy_lints)]
    #[allow(clippy::redundant_pub_crate)]
    #[allow(clippy::used_underscore_binding)]
    const _: () = {
        #[doc(hidden)]
        #[allow(dead_code)]
        #[allow(single_use_lifetimes)]
        #[allow(clippy::unknown_clippy_lints)]
        #[allow(clippy::mut_mut)]
        #[allow(clippy::redundant_pub_crate)]
        #[allow(clippy::ref_option_ref)]
        #[allow(clippy::type_repetition_in_bounds)]
        pub(crate) struct Projection<'__pin, Fut: Future>
        where
            ResponseFuture<Fut>: '__pin,
        {
            inner: ::pin_project_lite::__private::Pin<
                &'__pin mut (Either<Fut, Ready<Fut::Output>>),
            >,
        }
        #[doc(hidden)]
        #[allow(dead_code)]
        #[allow(single_use_lifetimes)]
        #[allow(clippy::unknown_clippy_lints)]
        #[allow(clippy::mut_mut)]
        #[allow(clippy::redundant_pub_crate)]
        #[allow(clippy::ref_option_ref)]
        #[allow(clippy::type_repetition_in_bounds)]
        pub(crate) struct ProjectionRef<'__pin, Fut: Future>
        where
            ResponseFuture<Fut>: '__pin,
        {
            inner: ::pin_project_lite::__private::Pin<
                &'__pin (Either<Fut, Ready<Fut::Output>>),
            >,
        }
        impl<Fut: Future> ResponseFuture<Fut> {
            #[doc(hidden)]
            #[inline]
            pub(crate) fn project<'__pin>(
                self: ::pin_project_lite::__private::Pin<&'__pin mut Self>,
            ) -> Projection<'__pin, Fut> {
                unsafe {
                    let Self { inner } = self.get_unchecked_mut();
                    Projection {
                        inner: ::pin_project_lite::__private::Pin::new_unchecked(inner),
                    }
                }
            }
            #[doc(hidden)]
            #[inline]
            pub(crate) fn project_ref<'__pin>(
                self: ::pin_project_lite::__private::Pin<&'__pin Self>,
            ) -> ProjectionRef<'__pin, Fut> {
                unsafe {
                    let Self { inner } = self.get_ref();
                    ProjectionRef {
                        inner: ::pin_project_lite::__private::Pin::new_unchecked(inner),
                    }
                }
            }
        }
        #[allow(non_snake_case)]
        pub struct __Origin<'__pin, Fut: Future> {
            __dummy_lifetime: ::pin_project_lite::__private::PhantomData<&'__pin ()>,
            inner: Either<Fut, Ready<Fut::Output>>,
        }
        impl<'__pin, Fut: Future> ::pin_project_lite::__private::Unpin
        for ResponseFuture<Fut>
        where
            __Origin<'__pin, Fut>: ::pin_project_lite::__private::Unpin,
        {}
        trait MustNotImplDrop {}
        #[allow(clippy::drop_bounds, drop_bounds)]
        impl<T: ::pin_project_lite::__private::Drop> MustNotImplDrop for T {}
        impl<Fut: Future> MustNotImplDrop for ResponseFuture<Fut> {}
        #[forbid(unaligned_references, safe_packed_borrows)]
        fn __assert_not_repr_packed<Fut: Future>(this: &ResponseFuture<Fut>) {
            let _ = &this.inner;
        }
    };
    impl<Fut: Future> Future for ResponseFuture<Fut> {
        type Output = Fut::Output;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.project().inner.poll(cx)
        }
    }
    /// A [`tower_layer::Layer`] which builds [`Lifecycle`].
    #[must_use]
    pub struct LifecycleLayer {
        _private: (),
    }
    #[automatically_derived]
    impl ::core::clone::Clone for LifecycleLayer {
        #[inline]
        fn clone(&self) -> LifecycleLayer {
            LifecycleLayer {
                _private: ::core::clone::Clone::clone(&self._private),
            }
        }
    }
    #[automatically_derived]
    impl ::core::default::Default for LifecycleLayer {
        #[inline]
        fn default() -> LifecycleLayer {
            LifecycleLayer {
                _private: ::core::default::Default::default(),
            }
        }
    }
    impl<S> Layer<S> for LifecycleLayer {
        type Service = Lifecycle<S>;
        fn layer(&self, inner: S) -> Self::Service {
            Lifecycle::new(inner)
        }
    }
}
#[cfg(feature = "omni-trait")]
mod omni_trait {
    use std::future::ready;
    use std::ops::ControlFlow;
    use futures::future::BoxFuture;
    use lsp_types::notification::{self, Notification};
    use lsp_types::request::{self, Request};
    use lsp_types::{lsp_notification, lsp_request};
    use crate::router::Router;
    use crate::{ClientSocket, ErrorCode, ResponseError, Result, ServerSocket};
    use self::sealed::NotifyResult;
    mod sealed {
        use super::*;
        pub trait NotifyResult {
            fn fallback<N: Notification>() -> Self;
        }
        impl NotifyResult for ControlFlow<crate::Result<()>> {
            fn fallback<N: Notification>() -> Self {
                if N::METHOD.starts_with("$/") || N::METHOD == notification::Exit::METHOD
                    || N::METHOD == notification::Initialized::METHOD
                {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(
                        Err(
                            crate::Error::Routing({
                                let res = ::alloc::fmt::format(
                                    format_args!("Unhandled notification: {0}", N::METHOD),
                                );
                                res
                            }),
                        ),
                    )
                }
            }
        }
        impl NotifyResult for crate::Result<()> {
            fn fallback<N: Notification>() -> Self {
                ::core::panicking::panic("internal error: entered unreachable code")
            }
        }
    }
    type ResponseFuture<R, E> = BoxFuture<'static, Result<<R as Request>::Result, E>>;
    fn method_not_found<R, E>() -> ResponseFuture<R, E>
    where
        R: Request,
        R::Result: Send + 'static,
        E: From<ResponseError> + Send + 'static,
    {
        Box::pin(
            ready(
                Err(
                    ResponseError {
                        code: ErrorCode::METHOD_NOT_FOUND,
                        message: {
                            let res = ::alloc::fmt::format(
                                format_args!("No such method: {0}", R::METHOD),
                            );
                            res
                        },
                        data: None,
                    }
                        .into(),
                ),
            ),
        )
    }
    /// The omnitrait defining all standard LSP requests and notifications supported by
    /// [`lsp_types`] for a Language Server.
    #[allow(missing_docs)]
    pub trait LanguageServer {
        /// Should always be defined to [`ResponseError`] for user implementations.
        type Error: From<ResponseError> + Send + 'static;
        /// Should always be defined to `ControlFlow<Result<()>>` for user implementations.
        type NotifyResult: NotifyResult;
        #[must_use]
        fn initialize(
            &mut self,
            params: <request::Initialize as Request>::Params,
        ) -> ResponseFuture<request::Initialize, Self::Error>;
        #[must_use]
        fn shutdown(
            &mut self,
            (): <request::Shutdown as Request>::Params,
        ) -> ResponseFuture<request::Shutdown, Self::Error> {
            Box::pin(ready(Ok(())))
        }
        #[must_use]
        fn implementation(
            &mut self,
            params: <::lsp_types::request::GotoImplementation as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoImplementation, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::GotoImplementation, _>()
        }
        #[must_use]
        fn type_definition(
            &mut self,
            params: <::lsp_types::request::GotoTypeDefinition as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoTypeDefinition, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::GotoTypeDefinition, _>()
        }
        #[must_use]
        fn document_color(
            &mut self,
            params: <::lsp_types::request::DocumentColor as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentColor, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::DocumentColor, _>()
        }
        #[must_use]
        fn color_presentation(
            &mut self,
            params: <::lsp_types::request::ColorPresentationRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::ColorPresentationRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::ColorPresentationRequest, _>()
        }
        #[must_use]
        fn folding_range(
            &mut self,
            params: <::lsp_types::request::FoldingRangeRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::FoldingRangeRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::FoldingRangeRequest, _>()
        }
        #[must_use]
        fn declaration(
            &mut self,
            params: <::lsp_types::request::GotoDeclaration as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoDeclaration, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::GotoDeclaration, _>()
        }
        #[must_use]
        fn selection_range(
            &mut self,
            params: <::lsp_types::request::SelectionRangeRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SelectionRangeRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::SelectionRangeRequest, _>()
        }
        #[must_use]
        fn prepare_call_hierarchy(
            &mut self,
            params: <::lsp_types::request::CallHierarchyPrepare as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CallHierarchyPrepare, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::CallHierarchyPrepare, _>()
        }
        #[must_use]
        fn incoming_calls(
            &mut self,
            params: <::lsp_types::request::CallHierarchyIncomingCalls as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CallHierarchyIncomingCalls,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::CallHierarchyIncomingCalls, _>()
        }
        #[must_use]
        fn outgoing_calls(
            &mut self,
            params: <::lsp_types::request::CallHierarchyOutgoingCalls as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CallHierarchyOutgoingCalls,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::CallHierarchyOutgoingCalls, _>()
        }
        #[must_use]
        fn semantic_tokens_full(
            &mut self,
            params: <::lsp_types::request::SemanticTokensFullRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensFullRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::SemanticTokensFullRequest, _>()
        }
        #[must_use]
        fn semantic_tokens_full_delta(
            &mut self,
            params: <::lsp_types::request::SemanticTokensFullDeltaRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensFullDeltaRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::SemanticTokensFullDeltaRequest, _>()
        }
        #[must_use]
        fn semantic_tokens_range(
            &mut self,
            params: <::lsp_types::request::SemanticTokensRangeRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensRangeRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::SemanticTokensRangeRequest, _>()
        }
        #[must_use]
        fn linked_editing_range(
            &mut self,
            params: <::lsp_types::request::LinkedEditingRange as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::LinkedEditingRange, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::LinkedEditingRange, _>()
        }
        #[must_use]
        fn will_create_files(
            &mut self,
            params: <::lsp_types::request::WillCreateFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillCreateFiles, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WillCreateFiles, _>()
        }
        #[must_use]
        fn will_rename_files(
            &mut self,
            params: <::lsp_types::request::WillRenameFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillRenameFiles, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WillRenameFiles, _>()
        }
        #[must_use]
        fn will_delete_files(
            &mut self,
            params: <::lsp_types::request::WillDeleteFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillDeleteFiles, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WillDeleteFiles, _>()
        }
        #[must_use]
        fn moniker(
            &mut self,
            params: <::lsp_types::request::MonikerRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::MonikerRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::MonikerRequest, _>()
        }
        #[must_use]
        fn prepare_type_hierarchy(
            &mut self,
            params: <::lsp_types::request::TypeHierarchyPrepare as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchyPrepare, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::TypeHierarchyPrepare, _>()
        }
        #[must_use]
        fn supertypes(
            &mut self,
            params: <::lsp_types::request::TypeHierarchySupertypes as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchySupertypes, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::TypeHierarchySupertypes, _>()
        }
        #[must_use]
        fn subtypes(
            &mut self,
            params: <::lsp_types::request::TypeHierarchySubtypes as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchySubtypes, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::TypeHierarchySubtypes, _>()
        }
        #[must_use]
        fn inline_value(
            &mut self,
            params: <::lsp_types::request::InlineValueRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlineValueRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::InlineValueRequest, _>()
        }
        #[must_use]
        fn inlay_hint(
            &mut self,
            params: <::lsp_types::request::InlayHintRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::InlayHintRequest, _>()
        }
        #[must_use]
        fn inlay_hint_resolve(
            &mut self,
            params: <::lsp_types::request::InlayHintResolveRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintResolveRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::InlayHintResolveRequest, _>()
        }
        #[must_use]
        fn document_diagnostic(
            &mut self,
            params: <::lsp_types::request::DocumentDiagnosticRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::DocumentDiagnosticRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::DocumentDiagnosticRequest, _>()
        }
        #[must_use]
        fn workspace_diagnostic(
            &mut self,
            params: <::lsp_types::request::WorkspaceDiagnosticRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::WorkspaceDiagnosticRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::WorkspaceDiagnosticRequest, _>()
        }
        #[must_use]
        fn will_save_wait_until(
            &mut self,
            params: <::lsp_types::request::WillSaveWaitUntil as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillSaveWaitUntil, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WillSaveWaitUntil, _>()
        }
        #[must_use]
        fn completion(
            &mut self,
            params: <::lsp_types::request::Completion as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Completion, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::Completion, _>()
        }
        #[must_use]
        fn completion_item_resolve(
            &mut self,
            params: <::lsp_types::request::ResolveCompletionItem as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ResolveCompletionItem, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::ResolveCompletionItem, _>()
        }
        #[must_use]
        fn hover(
            &mut self,
            params: <::lsp_types::request::HoverRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::HoverRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::HoverRequest, _>()
        }
        #[must_use]
        fn signature_help(
            &mut self,
            params: <::lsp_types::request::SignatureHelpRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SignatureHelpRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::SignatureHelpRequest, _>()
        }
        #[must_use]
        fn definition(
            &mut self,
            params: <::lsp_types::request::GotoDefinition as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoDefinition, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::GotoDefinition, _>()
        }
        #[must_use]
        fn references(
            &mut self,
            params: <::lsp_types::request::References as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::References, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::References, _>()
        }
        #[must_use]
        fn document_highlight(
            &mut self,
            params: <::lsp_types::request::DocumentHighlightRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::DocumentHighlightRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::DocumentHighlightRequest, _>()
        }
        #[must_use]
        fn document_symbol(
            &mut self,
            params: <::lsp_types::request::DocumentSymbolRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentSymbolRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::DocumentSymbolRequest, _>()
        }
        #[must_use]
        fn code_action(
            &mut self,
            params: <::lsp_types::request::CodeActionRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeActionRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::CodeActionRequest, _>()
        }
        #[must_use]
        fn code_action_resolve(
            &mut self,
            params: <::lsp_types::request::CodeActionResolveRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CodeActionResolveRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::CodeActionResolveRequest, _>()
        }
        #[must_use]
        fn symbol(
            &mut self,
            params: <::lsp_types::request::WorkspaceSymbolRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceSymbolRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WorkspaceSymbolRequest, _>()
        }
        #[must_use]
        fn workspace_symbol_resolve(
            &mut self,
            params: <::lsp_types::request::WorkspaceSymbolResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceSymbolResolve, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WorkspaceSymbolResolve, _>()
        }
        #[must_use]
        fn code_lens(
            &mut self,
            params: <::lsp_types::request::CodeLensRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::CodeLensRequest, _>()
        }
        #[must_use]
        fn code_lens_resolve(
            &mut self,
            params: <::lsp_types::request::CodeLensResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensResolve, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::CodeLensResolve, _>()
        }
        #[must_use]
        fn document_link(
            &mut self,
            params: <::lsp_types::request::DocumentLinkRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentLinkRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::DocumentLinkRequest, _>()
        }
        #[must_use]
        fn document_link_resolve(
            &mut self,
            params: <::lsp_types::request::DocumentLinkResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentLinkResolve, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::DocumentLinkResolve, _>()
        }
        #[must_use]
        fn formatting(
            &mut self,
            params: <::lsp_types::request::Formatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Formatting, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::Formatting, _>()
        }
        #[must_use]
        fn range_formatting(
            &mut self,
            params: <::lsp_types::request::RangeFormatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::RangeFormatting, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::RangeFormatting, _>()
        }
        #[must_use]
        fn on_type_formatting(
            &mut self,
            params: <::lsp_types::request::OnTypeFormatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::OnTypeFormatting, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::OnTypeFormatting, _>()
        }
        #[must_use]
        fn rename(
            &mut self,
            params: <::lsp_types::request::Rename as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Rename, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::Rename, _>()
        }
        #[must_use]
        fn prepare_rename(
            &mut self,
            params: <::lsp_types::request::PrepareRenameRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::PrepareRenameRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::PrepareRenameRequest, _>()
        }
        #[must_use]
        fn execute_command(
            &mut self,
            params: <::lsp_types::request::ExecuteCommand as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ExecuteCommand, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::ExecuteCommand, _>()
        }
        #[must_use]
        fn initialized(
            &mut self,
            params: <notification::Initialized as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<notification::Initialized>()
        }
        #[must_use]
        fn exit(
            &mut self,
            (): <notification::Exit as Notification>::Params,
        ) -> Self::NotifyResult {
            Self::NotifyResult::fallback::<notification::Exit>()
        }
        #[must_use]
        fn did_change_workspace_folders(
            &mut self,
            params: <::lsp_types::notification::DidChangeWorkspaceFolders as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::DidChangeWorkspaceFolders,
            >()
        }
        #[must_use]
        fn work_done_progress_cancel(
            &mut self,
            params: <::lsp_types::notification::WorkDoneProgressCancel as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::WorkDoneProgressCancel,
            >()
        }
        #[must_use]
        fn did_create_files(
            &mut self,
            params: <::lsp_types::notification::DidCreateFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::DidCreateFiles>()
        }
        #[must_use]
        fn did_rename_files(
            &mut self,
            params: <::lsp_types::notification::DidRenameFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::DidRenameFiles>()
        }
        #[must_use]
        fn did_delete_files(
            &mut self,
            params: <::lsp_types::notification::DidDeleteFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::DidDeleteFiles>()
        }
        #[must_use]
        fn did_change_configuration(
            &mut self,
            params: <::lsp_types::notification::DidChangeConfiguration as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::DidChangeConfiguration,
            >()
        }
        #[must_use]
        fn did_open(
            &mut self,
            params: <::lsp_types::notification::DidOpenTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::DidOpenTextDocument,
            >()
        }
        #[must_use]
        fn did_change(
            &mut self,
            params: <::lsp_types::notification::DidChangeTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::DidChangeTextDocument,
            >()
        }
        #[must_use]
        fn did_close(
            &mut self,
            params: <::lsp_types::notification::DidCloseTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::DidCloseTextDocument,
            >()
        }
        #[must_use]
        fn did_save(
            &mut self,
            params: <::lsp_types::notification::DidSaveTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::DidSaveTextDocument,
            >()
        }
        #[must_use]
        fn will_save(
            &mut self,
            params: <::lsp_types::notification::WillSaveTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::WillSaveTextDocument,
            >()
        }
        #[must_use]
        fn did_change_watched_files(
            &mut self,
            params: <::lsp_types::notification::DidChangeWatchedFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::DidChangeWatchedFiles,
            >()
        }
        #[must_use]
        fn set_trace(
            &mut self,
            params: <::lsp_types::notification::SetTrace as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::SetTrace>()
        }
        #[must_use]
        fn cancel_request(
            &mut self,
            params: <::lsp_types::notification::Cancel as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::Cancel>()
        }
        #[must_use]
        fn progress(
            &mut self,
            params: <::lsp_types::notification::Progress as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::Progress>()
        }
    }
    impl LanguageServer for ServerSocket {
        type Error = crate::Error;
        type NotifyResult = crate::Result<()>;
        fn initialize(
            &mut self,
            params: <request::Initialize as Request>::Params,
        ) -> ResponseFuture<request::Initialize, Self::Error> {
            Box::pin(self.0.request::<request::Initialize>(params))
        }
        fn shutdown(
            &mut self,
            (): <request::Shutdown as Request>::Params,
        ) -> ResponseFuture<request::Shutdown, Self::Error> {
            Box::pin(self.0.request::<request::Shutdown>(()))
        }
        fn implementation(
            &mut self,
            params: <::lsp_types::request::GotoImplementation as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoImplementation, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoImplementation>(params))
        }
        fn type_definition(
            &mut self,
            params: <::lsp_types::request::GotoTypeDefinition as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoTypeDefinition, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoTypeDefinition>(params))
        }
        fn document_color(
            &mut self,
            params: <::lsp_types::request::DocumentColor as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentColor, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::DocumentColor>(params))
        }
        fn color_presentation(
            &mut self,
            params: <::lsp_types::request::ColorPresentationRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::ColorPresentationRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::ColorPresentationRequest>(params),
            )
        }
        fn folding_range(
            &mut self,
            params: <::lsp_types::request::FoldingRangeRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::FoldingRangeRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::FoldingRangeRequest>(params))
        }
        fn declaration(
            &mut self,
            params: <::lsp_types::request::GotoDeclaration as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoDeclaration, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoDeclaration>(params))
        }
        fn selection_range(
            &mut self,
            params: <::lsp_types::request::SelectionRangeRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SelectionRangeRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::SelectionRangeRequest>(params),
            )
        }
        fn prepare_call_hierarchy(
            &mut self,
            params: <::lsp_types::request::CallHierarchyPrepare as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CallHierarchyPrepare, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::CallHierarchyPrepare>(params),
            )
        }
        fn incoming_calls(
            &mut self,
            params: <::lsp_types::request::CallHierarchyIncomingCalls as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CallHierarchyIncomingCalls,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::CallHierarchyIncomingCalls>(params),
            )
        }
        fn outgoing_calls(
            &mut self,
            params: <::lsp_types::request::CallHierarchyOutgoingCalls as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CallHierarchyOutgoingCalls,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::CallHierarchyOutgoingCalls>(params),
            )
        }
        fn semantic_tokens_full(
            &mut self,
            params: <::lsp_types::request::SemanticTokensFullRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensFullRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::SemanticTokensFullRequest>(params),
            )
        }
        fn semantic_tokens_full_delta(
            &mut self,
            params: <::lsp_types::request::SemanticTokensFullDeltaRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensFullDeltaRequest,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<
                        ::lsp_types::request::SemanticTokensFullDeltaRequest,
                    >(params),
            )
        }
        fn semantic_tokens_range(
            &mut self,
            params: <::lsp_types::request::SemanticTokensRangeRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensRangeRequest,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::SemanticTokensRangeRequest>(params),
            )
        }
        fn linked_editing_range(
            &mut self,
            params: <::lsp_types::request::LinkedEditingRange as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::LinkedEditingRange, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::LinkedEditingRange>(params))
        }
        fn will_create_files(
            &mut self,
            params: <::lsp_types::request::WillCreateFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillCreateFiles, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillCreateFiles>(params))
        }
        fn will_rename_files(
            &mut self,
            params: <::lsp_types::request::WillRenameFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillRenameFiles, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillRenameFiles>(params))
        }
        fn will_delete_files(
            &mut self,
            params: <::lsp_types::request::WillDeleteFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillDeleteFiles, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillDeleteFiles>(params))
        }
        fn moniker(
            &mut self,
            params: <::lsp_types::request::MonikerRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::MonikerRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::MonikerRequest>(params))
        }
        fn prepare_type_hierarchy(
            &mut self,
            params: <::lsp_types::request::TypeHierarchyPrepare as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchyPrepare, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::TypeHierarchyPrepare>(params),
            )
        }
        fn supertypes(
            &mut self,
            params: <::lsp_types::request::TypeHierarchySupertypes as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchySupertypes, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::TypeHierarchySupertypes>(params),
            )
        }
        fn subtypes(
            &mut self,
            params: <::lsp_types::request::TypeHierarchySubtypes as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchySubtypes, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::TypeHierarchySubtypes>(params),
            )
        }
        fn inline_value(
            &mut self,
            params: <::lsp_types::request::InlineValueRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlineValueRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::InlineValueRequest>(params))
        }
        fn inlay_hint(
            &mut self,
            params: <::lsp_types::request::InlayHintRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::InlayHintRequest>(params))
        }
        fn inlay_hint_resolve(
            &mut self,
            params: <::lsp_types::request::InlayHintResolveRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintResolveRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::InlayHintResolveRequest>(params),
            )
        }
        fn document_diagnostic(
            &mut self,
            params: <::lsp_types::request::DocumentDiagnosticRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::DocumentDiagnosticRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::DocumentDiagnosticRequest>(params),
            )
        }
        fn workspace_diagnostic(
            &mut self,
            params: <::lsp_types::request::WorkspaceDiagnosticRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::WorkspaceDiagnosticRequest,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::WorkspaceDiagnosticRequest>(params),
            )
        }
        fn will_save_wait_until(
            &mut self,
            params: <::lsp_types::request::WillSaveWaitUntil as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillSaveWaitUntil, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillSaveWaitUntil>(params))
        }
        fn completion(
            &mut self,
            params: <::lsp_types::request::Completion as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Completion, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::Completion>(params))
        }
        fn completion_item_resolve(
            &mut self,
            params: <::lsp_types::request::ResolveCompletionItem as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ResolveCompletionItem, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::ResolveCompletionItem>(params),
            )
        }
        fn hover(
            &mut self,
            params: <::lsp_types::request::HoverRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::HoverRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::HoverRequest>(params))
        }
        fn signature_help(
            &mut self,
            params: <::lsp_types::request::SignatureHelpRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SignatureHelpRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::SignatureHelpRequest>(params),
            )
        }
        fn definition(
            &mut self,
            params: <::lsp_types::request::GotoDefinition as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoDefinition, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoDefinition>(params))
        }
        fn references(
            &mut self,
            params: <::lsp_types::request::References as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::References, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::References>(params))
        }
        fn document_highlight(
            &mut self,
            params: <::lsp_types::request::DocumentHighlightRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::DocumentHighlightRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::DocumentHighlightRequest>(params),
            )
        }
        fn document_symbol(
            &mut self,
            params: <::lsp_types::request::DocumentSymbolRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentSymbolRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::DocumentSymbolRequest>(params),
            )
        }
        fn code_action(
            &mut self,
            params: <::lsp_types::request::CodeActionRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeActionRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeActionRequest>(params))
        }
        fn code_action_resolve(
            &mut self,
            params: <::lsp_types::request::CodeActionResolveRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CodeActionResolveRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::CodeActionResolveRequest>(params),
            )
        }
        fn symbol(
            &mut self,
            params: <::lsp_types::request::WorkspaceSymbolRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceSymbolRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceSymbolRequest>(params),
            )
        }
        fn workspace_symbol_resolve(
            &mut self,
            params: <::lsp_types::request::WorkspaceSymbolResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceSymbolResolve, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceSymbolResolve>(params),
            )
        }
        fn code_lens(
            &mut self,
            params: <::lsp_types::request::CodeLensRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeLensRequest>(params))
        }
        fn code_lens_resolve(
            &mut self,
            params: <::lsp_types::request::CodeLensResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensResolve, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeLensResolve>(params))
        }
        fn document_link(
            &mut self,
            params: <::lsp_types::request::DocumentLinkRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentLinkRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::DocumentLinkRequest>(params))
        }
        fn document_link_resolve(
            &mut self,
            params: <::lsp_types::request::DocumentLinkResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentLinkResolve, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::DocumentLinkResolve>(params))
        }
        fn formatting(
            &mut self,
            params: <::lsp_types::request::Formatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Formatting, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::Formatting>(params))
        }
        fn range_formatting(
            &mut self,
            params: <::lsp_types::request::RangeFormatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::RangeFormatting, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::RangeFormatting>(params))
        }
        fn on_type_formatting(
            &mut self,
            params: <::lsp_types::request::OnTypeFormatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::OnTypeFormatting, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::OnTypeFormatting>(params))
        }
        fn rename(
            &mut self,
            params: <::lsp_types::request::Rename as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Rename, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::Rename>(params))
        }
        fn prepare_rename(
            &mut self,
            params: <::lsp_types::request::PrepareRenameRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::PrepareRenameRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::PrepareRenameRequest>(params),
            )
        }
        fn execute_command(
            &mut self,
            params: <::lsp_types::request::ExecuteCommand as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ExecuteCommand, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ExecuteCommand>(params))
        }
        fn initialized(
            &mut self,
            params: <notification::Initialized as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<notification::Initialized>(params)
        }
        fn exit(
            &mut self,
            (): <notification::Exit as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<notification::Exit>(())
        }
        fn did_change_workspace_folders(
            &mut self,
            params: <::lsp_types::notification::DidChangeWorkspaceFolders as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeWorkspaceFolders>(params)
        }
        fn work_done_progress_cancel(
            &mut self,
            params: <::lsp_types::notification::WorkDoneProgressCancel as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::WorkDoneProgressCancel>(params)
        }
        fn did_create_files(
            &mut self,
            params: <::lsp_types::notification::DidCreateFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidCreateFiles>(params)
        }
        fn did_rename_files(
            &mut self,
            params: <::lsp_types::notification::DidRenameFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidRenameFiles>(params)
        }
        fn did_delete_files(
            &mut self,
            params: <::lsp_types::notification::DidDeleteFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidDeleteFiles>(params)
        }
        fn did_change_configuration(
            &mut self,
            params: <::lsp_types::notification::DidChangeConfiguration as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeConfiguration>(params)
        }
        fn did_open(
            &mut self,
            params: <::lsp_types::notification::DidOpenTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidOpenTextDocument>(params)
        }
        fn did_change(
            &mut self,
            params: <::lsp_types::notification::DidChangeTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeTextDocument>(params)
        }
        fn did_close(
            &mut self,
            params: <::lsp_types::notification::DidCloseTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidCloseTextDocument>(params)
        }
        fn did_save(
            &mut self,
            params: <::lsp_types::notification::DidSaveTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidSaveTextDocument>(params)
        }
        fn will_save(
            &mut self,
            params: <::lsp_types::notification::WillSaveTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::WillSaveTextDocument>(params)
        }
        fn did_change_watched_files(
            &mut self,
            params: <::lsp_types::notification::DidChangeWatchedFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeWatchedFiles>(params)
        }
        fn set_trace(
            &mut self,
            params: <::lsp_types::notification::SetTrace as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::SetTrace>(params)
        }
        fn cancel_request(
            &mut self,
            params: <::lsp_types::notification::Cancel as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Cancel>(params)
        }
        fn progress(
            &mut self,
            params: <::lsp_types::notification::Progress as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Progress>(params)
        }
    }
    impl LanguageServer for &'_ ServerSocket {
        type Error = crate::Error;
        type NotifyResult = crate::Result<()>;
        fn initialize(
            &mut self,
            params: <request::Initialize as Request>::Params,
        ) -> ResponseFuture<request::Initialize, Self::Error> {
            Box::pin(self.0.request::<request::Initialize>(params))
        }
        fn shutdown(
            &mut self,
            (): <request::Shutdown as Request>::Params,
        ) -> ResponseFuture<request::Shutdown, Self::Error> {
            Box::pin(self.0.request::<request::Shutdown>(()))
        }
        fn implementation(
            &mut self,
            params: <::lsp_types::request::GotoImplementation as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoImplementation, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoImplementation>(params))
        }
        fn type_definition(
            &mut self,
            params: <::lsp_types::request::GotoTypeDefinition as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoTypeDefinition, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoTypeDefinition>(params))
        }
        fn document_color(
            &mut self,
            params: <::lsp_types::request::DocumentColor as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentColor, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::DocumentColor>(params))
        }
        fn color_presentation(
            &mut self,
            params: <::lsp_types::request::ColorPresentationRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::ColorPresentationRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::ColorPresentationRequest>(params),
            )
        }
        fn folding_range(
            &mut self,
            params: <::lsp_types::request::FoldingRangeRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::FoldingRangeRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::FoldingRangeRequest>(params))
        }
        fn declaration(
            &mut self,
            params: <::lsp_types::request::GotoDeclaration as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoDeclaration, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoDeclaration>(params))
        }
        fn selection_range(
            &mut self,
            params: <::lsp_types::request::SelectionRangeRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SelectionRangeRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::SelectionRangeRequest>(params),
            )
        }
        fn prepare_call_hierarchy(
            &mut self,
            params: <::lsp_types::request::CallHierarchyPrepare as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CallHierarchyPrepare, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::CallHierarchyPrepare>(params),
            )
        }
        fn incoming_calls(
            &mut self,
            params: <::lsp_types::request::CallHierarchyIncomingCalls as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CallHierarchyIncomingCalls,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::CallHierarchyIncomingCalls>(params),
            )
        }
        fn outgoing_calls(
            &mut self,
            params: <::lsp_types::request::CallHierarchyOutgoingCalls as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CallHierarchyOutgoingCalls,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::CallHierarchyOutgoingCalls>(params),
            )
        }
        fn semantic_tokens_full(
            &mut self,
            params: <::lsp_types::request::SemanticTokensFullRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensFullRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::SemanticTokensFullRequest>(params),
            )
        }
        fn semantic_tokens_full_delta(
            &mut self,
            params: <::lsp_types::request::SemanticTokensFullDeltaRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensFullDeltaRequest,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<
                        ::lsp_types::request::SemanticTokensFullDeltaRequest,
                    >(params),
            )
        }
        fn semantic_tokens_range(
            &mut self,
            params: <::lsp_types::request::SemanticTokensRangeRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::SemanticTokensRangeRequest,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::SemanticTokensRangeRequest>(params),
            )
        }
        fn linked_editing_range(
            &mut self,
            params: <::lsp_types::request::LinkedEditingRange as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::LinkedEditingRange, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::LinkedEditingRange>(params))
        }
        fn will_create_files(
            &mut self,
            params: <::lsp_types::request::WillCreateFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillCreateFiles, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillCreateFiles>(params))
        }
        fn will_rename_files(
            &mut self,
            params: <::lsp_types::request::WillRenameFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillRenameFiles, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillRenameFiles>(params))
        }
        fn will_delete_files(
            &mut self,
            params: <::lsp_types::request::WillDeleteFiles as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillDeleteFiles, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillDeleteFiles>(params))
        }
        fn moniker(
            &mut self,
            params: <::lsp_types::request::MonikerRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::MonikerRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::MonikerRequest>(params))
        }
        fn prepare_type_hierarchy(
            &mut self,
            params: <::lsp_types::request::TypeHierarchyPrepare as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchyPrepare, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::TypeHierarchyPrepare>(params),
            )
        }
        fn supertypes(
            &mut self,
            params: <::lsp_types::request::TypeHierarchySupertypes as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchySupertypes, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::TypeHierarchySupertypes>(params),
            )
        }
        fn subtypes(
            &mut self,
            params: <::lsp_types::request::TypeHierarchySubtypes as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::TypeHierarchySubtypes, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::TypeHierarchySubtypes>(params),
            )
        }
        fn inline_value(
            &mut self,
            params: <::lsp_types::request::InlineValueRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlineValueRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::InlineValueRequest>(params))
        }
        fn inlay_hint(
            &mut self,
            params: <::lsp_types::request::InlayHintRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::InlayHintRequest>(params))
        }
        fn inlay_hint_resolve(
            &mut self,
            params: <::lsp_types::request::InlayHintResolveRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintResolveRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::InlayHintResolveRequest>(params),
            )
        }
        fn document_diagnostic(
            &mut self,
            params: <::lsp_types::request::DocumentDiagnosticRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::DocumentDiagnosticRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::DocumentDiagnosticRequest>(params),
            )
        }
        fn workspace_diagnostic(
            &mut self,
            params: <::lsp_types::request::WorkspaceDiagnosticRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::WorkspaceDiagnosticRequest,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::WorkspaceDiagnosticRequest>(params),
            )
        }
        fn will_save_wait_until(
            &mut self,
            params: <::lsp_types::request::WillSaveWaitUntil as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WillSaveWaitUntil, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::WillSaveWaitUntil>(params))
        }
        fn completion(
            &mut self,
            params: <::lsp_types::request::Completion as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Completion, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::Completion>(params))
        }
        fn completion_item_resolve(
            &mut self,
            params: <::lsp_types::request::ResolveCompletionItem as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ResolveCompletionItem, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::ResolveCompletionItem>(params),
            )
        }
        fn hover(
            &mut self,
            params: <::lsp_types::request::HoverRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::HoverRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::HoverRequest>(params))
        }
        fn signature_help(
            &mut self,
            params: <::lsp_types::request::SignatureHelpRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SignatureHelpRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::SignatureHelpRequest>(params),
            )
        }
        fn definition(
            &mut self,
            params: <::lsp_types::request::GotoDefinition as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::GotoDefinition, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::GotoDefinition>(params))
        }
        fn references(
            &mut self,
            params: <::lsp_types::request::References as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::References, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::References>(params))
        }
        fn document_highlight(
            &mut self,
            params: <::lsp_types::request::DocumentHighlightRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::DocumentHighlightRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::DocumentHighlightRequest>(params),
            )
        }
        fn document_symbol(
            &mut self,
            params: <::lsp_types::request::DocumentSymbolRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentSymbolRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::DocumentSymbolRequest>(params),
            )
        }
        fn code_action(
            &mut self,
            params: <::lsp_types::request::CodeActionRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeActionRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeActionRequest>(params))
        }
        fn code_action_resolve(
            &mut self,
            params: <::lsp_types::request::CodeActionResolveRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::CodeActionResolveRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::CodeActionResolveRequest>(params),
            )
        }
        fn symbol(
            &mut self,
            params: <::lsp_types::request::WorkspaceSymbolRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceSymbolRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceSymbolRequest>(params),
            )
        }
        fn workspace_symbol_resolve(
            &mut self,
            params: <::lsp_types::request::WorkspaceSymbolResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceSymbolResolve, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceSymbolResolve>(params),
            )
        }
        fn code_lens(
            &mut self,
            params: <::lsp_types::request::CodeLensRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeLensRequest>(params))
        }
        fn code_lens_resolve(
            &mut self,
            params: <::lsp_types::request::CodeLensResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensResolve, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeLensResolve>(params))
        }
        fn document_link(
            &mut self,
            params: <::lsp_types::request::DocumentLinkRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentLinkRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::DocumentLinkRequest>(params))
        }
        fn document_link_resolve(
            &mut self,
            params: <::lsp_types::request::DocumentLinkResolve as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::DocumentLinkResolve, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::DocumentLinkResolve>(params))
        }
        fn formatting(
            &mut self,
            params: <::lsp_types::request::Formatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Formatting, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::Formatting>(params))
        }
        fn range_formatting(
            &mut self,
            params: <::lsp_types::request::RangeFormatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::RangeFormatting, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::RangeFormatting>(params))
        }
        fn on_type_formatting(
            &mut self,
            params: <::lsp_types::request::OnTypeFormatting as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::OnTypeFormatting, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::OnTypeFormatting>(params))
        }
        fn rename(
            &mut self,
            params: <::lsp_types::request::Rename as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::Rename, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::Rename>(params))
        }
        fn prepare_rename(
            &mut self,
            params: <::lsp_types::request::PrepareRenameRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::PrepareRenameRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::PrepareRenameRequest>(params),
            )
        }
        fn execute_command(
            &mut self,
            params: <::lsp_types::request::ExecuteCommand as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ExecuteCommand, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ExecuteCommand>(params))
        }
        fn initialized(
            &mut self,
            params: <notification::Initialized as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<notification::Initialized>(params)
        }
        fn exit(
            &mut self,
            (): <notification::Exit as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<notification::Exit>(())
        }
        fn did_change_workspace_folders(
            &mut self,
            params: <::lsp_types::notification::DidChangeWorkspaceFolders as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeWorkspaceFolders>(params)
        }
        fn work_done_progress_cancel(
            &mut self,
            params: <::lsp_types::notification::WorkDoneProgressCancel as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::WorkDoneProgressCancel>(params)
        }
        fn did_create_files(
            &mut self,
            params: <::lsp_types::notification::DidCreateFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidCreateFiles>(params)
        }
        fn did_rename_files(
            &mut self,
            params: <::lsp_types::notification::DidRenameFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidRenameFiles>(params)
        }
        fn did_delete_files(
            &mut self,
            params: <::lsp_types::notification::DidDeleteFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidDeleteFiles>(params)
        }
        fn did_change_configuration(
            &mut self,
            params: <::lsp_types::notification::DidChangeConfiguration as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeConfiguration>(params)
        }
        fn did_open(
            &mut self,
            params: <::lsp_types::notification::DidOpenTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidOpenTextDocument>(params)
        }
        fn did_change(
            &mut self,
            params: <::lsp_types::notification::DidChangeTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeTextDocument>(params)
        }
        fn did_close(
            &mut self,
            params: <::lsp_types::notification::DidCloseTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidCloseTextDocument>(params)
        }
        fn did_save(
            &mut self,
            params: <::lsp_types::notification::DidSaveTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidSaveTextDocument>(params)
        }
        fn will_save(
            &mut self,
            params: <::lsp_types::notification::WillSaveTextDocument as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::WillSaveTextDocument>(params)
        }
        fn did_change_watched_files(
            &mut self,
            params: <::lsp_types::notification::DidChangeWatchedFiles as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::DidChangeWatchedFiles>(params)
        }
        fn set_trace(
            &mut self,
            params: <::lsp_types::notification::SetTrace as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::SetTrace>(params)
        }
        fn cancel_request(
            &mut self,
            params: <::lsp_types::notification::Cancel as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Cancel>(params)
        }
        fn progress(
            &mut self,
            params: <::lsp_types::notification::Progress as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Progress>(params)
        }
    }
    impl<S> Router<S>
    where
        S: LanguageServer<NotifyResult = ControlFlow<crate::Result<()>>>,
        ResponseError: From<S::Error>,
    {
        /// Create a [`Router`] using its implementation of [`LanguageServer`] as handlers.
        #[must_use]
        pub fn from_language_server(state: S) -> Self {
            let mut this = Self::new(state);
            this.request::<
                    request::Initialize,
                    _,
                >(|state, params| {
                let fut = state.initialize(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    request::Shutdown,
                    _,
                >(|state, params| {
                let fut = state.shutdown(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::GotoImplementation,
                    _,
                >(|state, params| {
                let fut = state.implementation(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::GotoTypeDefinition,
                    _,
                >(|state, params| {
                let fut = state.type_definition(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::DocumentColor,
                    _,
                >(|state, params| {
                let fut = state.document_color(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::ColorPresentationRequest,
                    _,
                >(|state, params| {
                let fut = state.color_presentation(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::FoldingRangeRequest,
                    _,
                >(|state, params| {
                let fut = state.folding_range(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::GotoDeclaration,
                    _,
                >(|state, params| {
                let fut = state.declaration(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::SelectionRangeRequest,
                    _,
                >(|state, params| {
                let fut = state.selection_range(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CallHierarchyPrepare,
                    _,
                >(|state, params| {
                let fut = state.prepare_call_hierarchy(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CallHierarchyIncomingCalls,
                    _,
                >(|state, params| {
                let fut = state.incoming_calls(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CallHierarchyOutgoingCalls,
                    _,
                >(|state, params| {
                let fut = state.outgoing_calls(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::SemanticTokensFullRequest,
                    _,
                >(|state, params| {
                let fut = state.semantic_tokens_full(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::SemanticTokensFullDeltaRequest,
                    _,
                >(|state, params| {
                let fut = state.semantic_tokens_full_delta(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::SemanticTokensRangeRequest,
                    _,
                >(|state, params| {
                let fut = state.semantic_tokens_range(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::LinkedEditingRange,
                    _,
                >(|state, params| {
                let fut = state.linked_editing_range(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WillCreateFiles,
                    _,
                >(|state, params| {
                let fut = state.will_create_files(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WillRenameFiles,
                    _,
                >(|state, params| {
                let fut = state.will_rename_files(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WillDeleteFiles,
                    _,
                >(|state, params| {
                let fut = state.will_delete_files(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::MonikerRequest,
                    _,
                >(|state, params| {
                let fut = state.moniker(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::TypeHierarchyPrepare,
                    _,
                >(|state, params| {
                let fut = state.prepare_type_hierarchy(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::TypeHierarchySupertypes,
                    _,
                >(|state, params| {
                let fut = state.supertypes(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::TypeHierarchySubtypes,
                    _,
                >(|state, params| {
                let fut = state.subtypes(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::InlineValueRequest,
                    _,
                >(|state, params| {
                let fut = state.inline_value(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::InlayHintRequest,
                    _,
                >(|state, params| {
                let fut = state.inlay_hint(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::InlayHintResolveRequest,
                    _,
                >(|state, params| {
                let fut = state.inlay_hint_resolve(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::DocumentDiagnosticRequest,
                    _,
                >(|state, params| {
                let fut = state.document_diagnostic(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WorkspaceDiagnosticRequest,
                    _,
                >(|state, params| {
                let fut = state.workspace_diagnostic(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WillSaveWaitUntil,
                    _,
                >(|state, params| {
                let fut = state.will_save_wait_until(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::Completion,
                    _,
                >(|state, params| {
                let fut = state.completion(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::ResolveCompletionItem,
                    _,
                >(|state, params| {
                let fut = state.completion_item_resolve(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::HoverRequest,
                    _,
                >(|state, params| {
                let fut = state.hover(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::SignatureHelpRequest,
                    _,
                >(|state, params| {
                let fut = state.signature_help(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::GotoDefinition,
                    _,
                >(|state, params| {
                let fut = state.definition(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::References,
                    _,
                >(|state, params| {
                let fut = state.references(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::DocumentHighlightRequest,
                    _,
                >(|state, params| {
                let fut = state.document_highlight(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::DocumentSymbolRequest,
                    _,
                >(|state, params| {
                let fut = state.document_symbol(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CodeActionRequest,
                    _,
                >(|state, params| {
                let fut = state.code_action(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CodeActionResolveRequest,
                    _,
                >(|state, params| {
                let fut = state.code_action_resolve(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WorkspaceSymbolRequest,
                    _,
                >(|state, params| {
                let fut = state.symbol(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WorkspaceSymbolResolve,
                    _,
                >(|state, params| {
                let fut = state.workspace_symbol_resolve(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CodeLensRequest,
                    _,
                >(|state, params| {
                let fut = state.code_lens(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CodeLensResolve,
                    _,
                >(|state, params| {
                let fut = state.code_lens_resolve(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::DocumentLinkRequest,
                    _,
                >(|state, params| {
                let fut = state.document_link(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::DocumentLinkResolve,
                    _,
                >(|state, params| {
                let fut = state.document_link_resolve(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::Formatting,
                    _,
                >(|state, params| {
                let fut = state.formatting(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::RangeFormatting,
                    _,
                >(|state, params| {
                let fut = state.range_formatting(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::OnTypeFormatting,
                    _,
                >(|state, params| {
                let fut = state.on_type_formatting(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::Rename,
                    _,
                >(|state, params| {
                let fut = state.rename(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::PrepareRenameRequest,
                    _,
                >(|state, params| {
                let fut = state.prepare_rename(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::ExecuteCommand,
                    _,
                >(|state, params| {
                let fut = state.execute_command(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.notification::<
                    notification::Initialized,
                >(|state, params| state.initialized(params));
            this.notification::<notification::Exit>(|state, params| state.exit(params));
            this.notification::<
                    ::lsp_types::notification::DidChangeWorkspaceFolders,
                >(|state, params| state.did_change_workspace_folders(params));
            this.notification::<
                    ::lsp_types::notification::WorkDoneProgressCancel,
                >(|state, params| state.work_done_progress_cancel(params));
            this.notification::<
                    ::lsp_types::notification::DidCreateFiles,
                >(|state, params| state.did_create_files(params));
            this.notification::<
                    ::lsp_types::notification::DidRenameFiles,
                >(|state, params| state.did_rename_files(params));
            this.notification::<
                    ::lsp_types::notification::DidDeleteFiles,
                >(|state, params| state.did_delete_files(params));
            this.notification::<
                    ::lsp_types::notification::DidChangeConfiguration,
                >(|state, params| state.did_change_configuration(params));
            this.notification::<
                    ::lsp_types::notification::DidOpenTextDocument,
                >(|state, params| state.did_open(params));
            this.notification::<
                    ::lsp_types::notification::DidChangeTextDocument,
                >(|state, params| state.did_change(params));
            this.notification::<
                    ::lsp_types::notification::DidCloseTextDocument,
                >(|state, params| state.did_close(params));
            this.notification::<
                    ::lsp_types::notification::DidSaveTextDocument,
                >(|state, params| state.did_save(params));
            this.notification::<
                    ::lsp_types::notification::WillSaveTextDocument,
                >(|state, params| state.will_save(params));
            this.notification::<
                    ::lsp_types::notification::DidChangeWatchedFiles,
                >(|state, params| state.did_change_watched_files(params));
            this.notification::<
                    ::lsp_types::notification::SetTrace,
                >(|state, params| state.set_trace(params));
            this.notification::<
                    ::lsp_types::notification::Cancel,
                >(|state, params| state.cancel_request(params));
            this.notification::<
                    ::lsp_types::notification::Progress,
                >(|state, params| state.progress(params));
            this
        }
    }
    /// The omnitrait defining all standard LSP requests and notifications supported by
    /// [`lsp_types`] for a Language Client.
    #[allow(missing_docs)]
    pub trait LanguageClient {
        /// Should always be defined to [`ResponseError`] for user implementations.
        type Error: From<ResponseError> + Send + 'static;
        /// Should always be defined to `ControlFlow<Result<()>>` for user implementations.
        type NotifyResult: NotifyResult;
        #[must_use]
        fn workspace_folders(
            &mut self,
            params: <::lsp_types::request::WorkspaceFoldersRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceFoldersRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WorkspaceFoldersRequest, _>()
        }
        #[must_use]
        fn configuration(
            &mut self,
            params: <::lsp_types::request::WorkspaceConfiguration as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceConfiguration, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WorkspaceConfiguration, _>()
        }
        #[must_use]
        fn work_done_progress_create(
            &mut self,
            params: <::lsp_types::request::WorkDoneProgressCreate as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkDoneProgressCreate, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::WorkDoneProgressCreate, _>()
        }
        #[must_use]
        fn semantic_tokens_refresh(
            &mut self,
            params: <::lsp_types::request::SemanticTokensRefresh as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SemanticTokensRefresh, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::SemanticTokensRefresh, _>()
        }
        #[must_use]
        fn show_document(
            &mut self,
            params: <::lsp_types::request::ShowDocument as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ShowDocument, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::ShowDocument, _>()
        }
        #[must_use]
        fn inline_value_refresh(
            &mut self,
            params: <::lsp_types::request::InlineValueRefreshRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::InlineValueRefreshRequest,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::InlineValueRefreshRequest, _>()
        }
        #[must_use]
        fn inlay_hint_refresh(
            &mut self,
            params: <::lsp_types::request::InlayHintRefreshRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintRefreshRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::InlayHintRefreshRequest, _>()
        }
        #[must_use]
        fn workspace_diagnostic_refresh(
            &mut self,
            params: <::lsp_types::request::WorkspaceDiagnosticRefresh as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::WorkspaceDiagnosticRefresh,
            Self::Error,
        > {
            let _ = params;
            method_not_found::<::lsp_types::request::WorkspaceDiagnosticRefresh, _>()
        }
        #[must_use]
        fn register_capability(
            &mut self,
            params: <::lsp_types::request::RegisterCapability as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::RegisterCapability, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::RegisterCapability, _>()
        }
        #[must_use]
        fn unregister_capability(
            &mut self,
            params: <::lsp_types::request::UnregisterCapability as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::UnregisterCapability, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::UnregisterCapability, _>()
        }
        #[must_use]
        fn show_message_request(
            &mut self,
            params: <::lsp_types::request::ShowMessageRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ShowMessageRequest, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::ShowMessageRequest, _>()
        }
        #[must_use]
        fn code_lens_refresh(
            &mut self,
            params: <::lsp_types::request::CodeLensRefresh as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensRefresh, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::CodeLensRefresh, _>()
        }
        #[must_use]
        fn apply_edit(
            &mut self,
            params: <::lsp_types::request::ApplyWorkspaceEdit as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ApplyWorkspaceEdit, Self::Error> {
            let _ = params;
            method_not_found::<::lsp_types::request::ApplyWorkspaceEdit, _>()
        }
        #[must_use]
        fn show_message(
            &mut self,
            params: <::lsp_types::notification::ShowMessage as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::ShowMessage>()
        }
        #[must_use]
        fn log_message(
            &mut self,
            params: <::lsp_types::notification::LogMessage as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::LogMessage>()
        }
        #[must_use]
        fn telemetry_event(
            &mut self,
            params: <::lsp_types::notification::TelemetryEvent as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::TelemetryEvent>()
        }
        #[must_use]
        fn publish_diagnostics(
            &mut self,
            params: <::lsp_types::notification::PublishDiagnostics as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<
                ::lsp_types::notification::PublishDiagnostics,
            >()
        }
        #[must_use]
        fn log_trace(
            &mut self,
            params: <::lsp_types::notification::LogTrace as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::LogTrace>()
        }
        #[must_use]
        fn cancel_request(
            &mut self,
            params: <::lsp_types::notification::Cancel as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::Cancel>()
        }
        #[must_use]
        fn progress(
            &mut self,
            params: <::lsp_types::notification::Progress as Notification>::Params,
        ) -> Self::NotifyResult {
            let _ = params;
            Self::NotifyResult::fallback::<::lsp_types::notification::Progress>()
        }
    }
    impl LanguageClient for ClientSocket {
        type Error = crate::Error;
        type NotifyResult = crate::Result<()>;
        fn workspace_folders(
            &mut self,
            params: <::lsp_types::request::WorkspaceFoldersRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceFoldersRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceFoldersRequest>(params),
            )
        }
        fn configuration(
            &mut self,
            params: <::lsp_types::request::WorkspaceConfiguration as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceConfiguration, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceConfiguration>(params),
            )
        }
        fn work_done_progress_create(
            &mut self,
            params: <::lsp_types::request::WorkDoneProgressCreate as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkDoneProgressCreate, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkDoneProgressCreate>(params),
            )
        }
        fn semantic_tokens_refresh(
            &mut self,
            params: <::lsp_types::request::SemanticTokensRefresh as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SemanticTokensRefresh, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::SemanticTokensRefresh>(params),
            )
        }
        fn show_document(
            &mut self,
            params: <::lsp_types::request::ShowDocument as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ShowDocument, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ShowDocument>(params))
        }
        fn inline_value_refresh(
            &mut self,
            params: <::lsp_types::request::InlineValueRefreshRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::InlineValueRefreshRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::InlineValueRefreshRequest>(params),
            )
        }
        fn inlay_hint_refresh(
            &mut self,
            params: <::lsp_types::request::InlayHintRefreshRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintRefreshRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::InlayHintRefreshRequest>(params),
            )
        }
        fn workspace_diagnostic_refresh(
            &mut self,
            params: <::lsp_types::request::WorkspaceDiagnosticRefresh as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::WorkspaceDiagnosticRefresh,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::WorkspaceDiagnosticRefresh>(params),
            )
        }
        fn register_capability(
            &mut self,
            params: <::lsp_types::request::RegisterCapability as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::RegisterCapability, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::RegisterCapability>(params))
        }
        fn unregister_capability(
            &mut self,
            params: <::lsp_types::request::UnregisterCapability as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::UnregisterCapability, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::UnregisterCapability>(params),
            )
        }
        fn show_message_request(
            &mut self,
            params: <::lsp_types::request::ShowMessageRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ShowMessageRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ShowMessageRequest>(params))
        }
        fn code_lens_refresh(
            &mut self,
            params: <::lsp_types::request::CodeLensRefresh as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensRefresh, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeLensRefresh>(params))
        }
        fn apply_edit(
            &mut self,
            params: <::lsp_types::request::ApplyWorkspaceEdit as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ApplyWorkspaceEdit, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ApplyWorkspaceEdit>(params))
        }
        fn show_message(
            &mut self,
            params: <::lsp_types::notification::ShowMessage as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::ShowMessage>(params)
        }
        fn log_message(
            &mut self,
            params: <::lsp_types::notification::LogMessage as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::LogMessage>(params)
        }
        fn telemetry_event(
            &mut self,
            params: <::lsp_types::notification::TelemetryEvent as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::TelemetryEvent>(params)
        }
        fn publish_diagnostics(
            &mut self,
            params: <::lsp_types::notification::PublishDiagnostics as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::PublishDiagnostics>(params)
        }
        fn log_trace(
            &mut self,
            params: <::lsp_types::notification::LogTrace as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::LogTrace>(params)
        }
        fn cancel_request(
            &mut self,
            params: <::lsp_types::notification::Cancel as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Cancel>(params)
        }
        fn progress(
            &mut self,
            params: <::lsp_types::notification::Progress as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Progress>(params)
        }
    }
    impl LanguageClient for &'_ ClientSocket {
        type Error = crate::Error;
        type NotifyResult = crate::Result<()>;
        fn workspace_folders(
            &mut self,
            params: <::lsp_types::request::WorkspaceFoldersRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceFoldersRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceFoldersRequest>(params),
            )
        }
        fn configuration(
            &mut self,
            params: <::lsp_types::request::WorkspaceConfiguration as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkspaceConfiguration, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkspaceConfiguration>(params),
            )
        }
        fn work_done_progress_create(
            &mut self,
            params: <::lsp_types::request::WorkDoneProgressCreate as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::WorkDoneProgressCreate, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::WorkDoneProgressCreate>(params),
            )
        }
        fn semantic_tokens_refresh(
            &mut self,
            params: <::lsp_types::request::SemanticTokensRefresh as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::SemanticTokensRefresh, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::SemanticTokensRefresh>(params),
            )
        }
        fn show_document(
            &mut self,
            params: <::lsp_types::request::ShowDocument as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ShowDocument, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ShowDocument>(params))
        }
        fn inline_value_refresh(
            &mut self,
            params: <::lsp_types::request::InlineValueRefreshRequest as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::InlineValueRefreshRequest,
            Self::Error,
        > {
            Box::pin(
                self.0.request::<::lsp_types::request::InlineValueRefreshRequest>(params),
            )
        }
        fn inlay_hint_refresh(
            &mut self,
            params: <::lsp_types::request::InlayHintRefreshRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::InlayHintRefreshRequest, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::InlayHintRefreshRequest>(params),
            )
        }
        fn workspace_diagnostic_refresh(
            &mut self,
            params: <::lsp_types::request::WorkspaceDiagnosticRefresh as Request>::Params,
        ) -> ResponseFuture<
            ::lsp_types::request::WorkspaceDiagnosticRefresh,
            Self::Error,
        > {
            Box::pin(
                self
                    .0
                    .request::<::lsp_types::request::WorkspaceDiagnosticRefresh>(params),
            )
        }
        fn register_capability(
            &mut self,
            params: <::lsp_types::request::RegisterCapability as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::RegisterCapability, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::RegisterCapability>(params))
        }
        fn unregister_capability(
            &mut self,
            params: <::lsp_types::request::UnregisterCapability as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::UnregisterCapability, Self::Error> {
            Box::pin(
                self.0.request::<::lsp_types::request::UnregisterCapability>(params),
            )
        }
        fn show_message_request(
            &mut self,
            params: <::lsp_types::request::ShowMessageRequest as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ShowMessageRequest, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ShowMessageRequest>(params))
        }
        fn code_lens_refresh(
            &mut self,
            params: <::lsp_types::request::CodeLensRefresh as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::CodeLensRefresh, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::CodeLensRefresh>(params))
        }
        fn apply_edit(
            &mut self,
            params: <::lsp_types::request::ApplyWorkspaceEdit as Request>::Params,
        ) -> ResponseFuture<::lsp_types::request::ApplyWorkspaceEdit, Self::Error> {
            Box::pin(self.0.request::<::lsp_types::request::ApplyWorkspaceEdit>(params))
        }
        fn show_message(
            &mut self,
            params: <::lsp_types::notification::ShowMessage as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::ShowMessage>(params)
        }
        fn log_message(
            &mut self,
            params: <::lsp_types::notification::LogMessage as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::LogMessage>(params)
        }
        fn telemetry_event(
            &mut self,
            params: <::lsp_types::notification::TelemetryEvent as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::TelemetryEvent>(params)
        }
        fn publish_diagnostics(
            &mut self,
            params: <::lsp_types::notification::PublishDiagnostics as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::PublishDiagnostics>(params)
        }
        fn log_trace(
            &mut self,
            params: <::lsp_types::notification::LogTrace as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::LogTrace>(params)
        }
        fn cancel_request(
            &mut self,
            params: <::lsp_types::notification::Cancel as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Cancel>(params)
        }
        fn progress(
            &mut self,
            params: <::lsp_types::notification::Progress as Notification>::Params,
        ) -> Self::NotifyResult {
            self.notify::<::lsp_types::notification::Progress>(params)
        }
    }
    impl<S> Router<S>
    where
        S: LanguageClient<NotifyResult = ControlFlow<crate::Result<()>>>,
        ResponseError: From<S::Error>,
    {
        /// Create a [`Router`] using its implementation of [`LanguageClient`] as handlers.
        #[must_use]
        pub fn from_language_client(state: S) -> Self {
            let mut this = Self::new(state);
            this.request::<
                    ::lsp_types::request::WorkspaceFoldersRequest,
                    _,
                >(|state, params| {
                let fut = state.workspace_folders(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WorkspaceConfiguration,
                    _,
                >(|state, params| {
                let fut = state.configuration(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WorkDoneProgressCreate,
                    _,
                >(|state, params| {
                let fut = state.work_done_progress_create(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::SemanticTokensRefresh,
                    _,
                >(|state, params| {
                let fut = state.semantic_tokens_refresh(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::ShowDocument,
                    _,
                >(|state, params| {
                let fut = state.show_document(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::InlineValueRefreshRequest,
                    _,
                >(|state, params| {
                let fut = state.inline_value_refresh(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::InlayHintRefreshRequest,
                    _,
                >(|state, params| {
                let fut = state.inlay_hint_refresh(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::WorkspaceDiagnosticRefresh,
                    _,
                >(|state, params| {
                let fut = state.workspace_diagnostic_refresh(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::RegisterCapability,
                    _,
                >(|state, params| {
                let fut = state.register_capability(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::UnregisterCapability,
                    _,
                >(|state, params| {
                let fut = state.unregister_capability(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::ShowMessageRequest,
                    _,
                >(|state, params| {
                let fut = state.show_message_request(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::CodeLensRefresh,
                    _,
                >(|state, params| {
                let fut = state.code_lens_refresh(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.request::<
                    ::lsp_types::request::ApplyWorkspaceEdit,
                    _,
                >(|state, params| {
                let fut = state.apply_edit(params);
                async move { fut.await.map_err(Into::into) }
            });
            this.notification::<
                    ::lsp_types::notification::ShowMessage,
                >(|state, params| state.show_message(params));
            this.notification::<
                    ::lsp_types::notification::LogMessage,
                >(|state, params| state.log_message(params));
            this.notification::<
                    ::lsp_types::notification::TelemetryEvent,
                >(|state, params| state.telemetry_event(params));
            this.notification::<
                    ::lsp_types::notification::PublishDiagnostics,
                >(|state, params| state.publish_diagnostics(params));
            this.notification::<
                    ::lsp_types::notification::LogTrace,
                >(|state, params| state.log_trace(params));
            this.notification::<
                    ::lsp_types::notification::Cancel,
                >(|state, params| state.cancel_request(params));
            this.notification::<
                    ::lsp_types::notification::Progress,
                >(|state, params| state.progress(params));
            this
        }
    }
}
#[cfg(feature = "omni-trait")]
pub use omni_trait::{LanguageClient, LanguageServer};
/// A convenient type alias for `Result` with `E` = [`enum@crate::Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
/// Possible errors.
#[non_exhaustive]
pub enum Error {
    /// The service main loop stopped.
    #[error("service stopped")]
    ServiceStopped,
    /// The peer replies undecodable or invalid responses.
    #[error("deserialization failed: {0}")]
    Deserialize(#[from] serde_json::Error),
    /// The peer replies an error.
    #[error("{0}")]
    Response(#[from] ResponseError),
    /// The peer violates the Language Server Protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Input/output errors from the underlying channels.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The underlying channel reached EOF (end of file).
    #[error("the underlying channel reached EOF")]
    Eof,
    /// No handlers for events or mandatory notifications (not starting with `$/`).
    ///
    /// Will not occur when catch-all handlers ([`router::Router::unhandled_event`] and
    /// [`router::Router::unhandled_notification`]) are installed.
    #[error("{0}")]
    Routing(String),
}
#[automatically_derived]
impl ::core::fmt::Debug for Error {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        match self {
            Error::ServiceStopped => {
                ::core::fmt::Formatter::write_str(f, "ServiceStopped")
            }
            Error::Deserialize(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(
                    f,
                    "Deserialize",
                    &__self_0,
                )
            }
            Error::Response(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(
                    f,
                    "Response",
                    &__self_0,
                )
            }
            Error::Protocol(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(
                    f,
                    "Protocol",
                    &__self_0,
                )
            }
            Error::Io(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(f, "Io", &__self_0)
            }
            Error::Eof => ::core::fmt::Formatter::write_str(f, "Eof"),
            Error::Routing(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(
                    f,
                    "Routing",
                    &__self_0,
                )
            }
        }
    }
}
#[allow(unused_qualifications)]
impl std::error::Error for Error {
    fn source(&self) -> ::core::option::Option<&(dyn std::error::Error + 'static)> {
        use thiserror::__private::AsDynError;
        #[allow(deprecated)]
        match self {
            Error::ServiceStopped { .. } => ::core::option::Option::None,
            Error::Deserialize { 0: source, .. } => {
                ::core::option::Option::Some(source.as_dyn_error())
            }
            Error::Response { 0: source, .. } => {
                ::core::option::Option::Some(source.as_dyn_error())
            }
            Error::Protocol { .. } => ::core::option::Option::None,
            Error::Io { 0: source, .. } => {
                ::core::option::Option::Some(source.as_dyn_error())
            }
            Error::Eof { .. } => ::core::option::Option::None,
            Error::Routing { .. } => ::core::option::Option::None,
        }
    }
}
#[allow(unused_qualifications)]
impl ::core::fmt::Display for Error {
    fn fmt(&self, __formatter: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        use thiserror::__private::AsDisplay as _;
        #[allow(unused_variables, deprecated, clippy::used_underscore_binding)]
        match self {
            Error::ServiceStopped {} => {
                __formatter.write_fmt(format_args!("service stopped"))
            }
            Error::Deserialize(_0) => {
                __formatter
                    .write_fmt(
                        format_args!("deserialization failed: {0}", _0.as_display()),
                    )
            }
            Error::Response(_0) => {
                __formatter.write_fmt(format_args!("{0}", _0.as_display()))
            }
            Error::Protocol(_0) => {
                __formatter
                    .write_fmt(format_args!("protocol error: {0}", _0.as_display()))
            }
            Error::Io(_0) => __formatter.write_fmt(format_args!("{0}", _0.as_display())),
            Error::Eof {} => {
                __formatter.write_fmt(format_args!("the underlying channel reached EOF"))
            }
            Error::Routing(_0) => {
                __formatter.write_fmt(format_args!("{0}", _0.as_display()))
            }
        }
    }
}
#[allow(unused_qualifications)]
impl ::core::convert::From<serde_json::Error> for Error {
    #[allow(deprecated)]
    fn from(source: serde_json::Error) -> Self {
        Error::Deserialize { 0: source }
    }
}
#[allow(unused_qualifications)]
impl ::core::convert::From<ResponseError> for Error {
    #[allow(deprecated)]
    fn from(source: ResponseError) -> Self {
        Error::Response { 0: source }
    }
}
#[allow(unused_qualifications)]
impl ::core::convert::From<io::Error> for Error {
    #[allow(deprecated)]
    fn from(source: io::Error) -> Self {
        Error::Io { 0: source }
    }
}
/// The core service abstraction, representing either a Language Server or Language Client.
pub trait LspService: Service<AnyRequest> {
    /// The handler of [LSP notifications](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#notificationMessage).
    ///
    /// Notifications are delivered in order and synchronously. This is mandatory since they can
    /// change the interpretation of later notifications or requests.
    ///
    /// # Return
    ///
    /// The return value decides the action to either break or continue the main loop.
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>>;
    /// The handler of an arbitrary [`AnyEvent`].
    ///
    /// Events are emitted by users or middlewares via [`ClientSocket::emit`] or
    /// [`ServerSocket::emit`], for user-defined purposes. Events are delivered in order and
    /// synchronously.
    ///
    /// # Return
    ///
    /// The return value decides the action to either break or continue the main loop.
    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>>;
}
/// A JSON-RPC error code.
///
/// Codes defined and/or used by LSP are defined as associated constants, eg.
/// [`ErrorCode::REQUEST_FAILED`].
///
/// See:
/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#errorCodes>
#[error("jsonrpc error {0}")]
pub struct ErrorCode(pub i32);
#[automatically_derived]
impl ::core::fmt::Debug for ErrorCode {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_tuple_field1_finish(f, "ErrorCode", &&self.0)
    }
}
#[automatically_derived]
impl ::core::clone::Clone for ErrorCode {
    #[inline]
    fn clone(&self) -> ErrorCode {
        let _: ::core::clone::AssertParamIsClone<i32>;
        *self
    }
}
#[automatically_derived]
impl ::core::marker::Copy for ErrorCode {}
#[automatically_derived]
impl ::core::marker::StructuralPartialEq for ErrorCode {}
#[automatically_derived]
impl ::core::cmp::PartialEq for ErrorCode {
    #[inline]
    fn eq(&self, other: &ErrorCode) -> bool {
        self.0 == other.0
    }
}
#[automatically_derived]
impl ::core::marker::StructuralEq for ErrorCode {}
#[automatically_derived]
impl ::core::cmp::Eq for ErrorCode {
    #[inline]
    #[doc(hidden)]
    #[coverage(off)]
    fn assert_receiver_is_total_eq(&self) -> () {
        let _: ::core::cmp::AssertParamIsEq<i32>;
    }
}
#[automatically_derived]
impl ::core::hash::Hash for ErrorCode {
    #[inline]
    fn hash<__H: ::core::hash::Hasher>(&self, state: &mut __H) -> () {
        ::core::hash::Hash::hash(&self.0, state)
    }
}
#[automatically_derived]
impl ::core::cmp::PartialOrd for ErrorCode {
    #[inline]
    fn partial_cmp(
        &self,
        other: &ErrorCode,
    ) -> ::core::option::Option<::core::cmp::Ordering> {
        ::core::cmp::PartialOrd::partial_cmp(&self.0, &other.0)
    }
}
#[automatically_derived]
impl ::core::cmp::Ord for ErrorCode {
    #[inline]
    fn cmp(&self, other: &ErrorCode) -> ::core::cmp::Ordering {
        ::core::cmp::Ord::cmp(&self.0, &other.0)
    }
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl _serde::Serialize for ErrorCode {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            _serde::Serializer::serialize_newtype_struct(
                __serializer,
                "ErrorCode",
                &self.0,
            )
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for ErrorCode {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            #[doc(hidden)]
            struct __Visitor<'de> {
                marker: _serde::__private::PhantomData<ErrorCode>,
                lifetime: _serde::__private::PhantomData<&'de ()>,
            }
            impl<'de> _serde::de::Visitor<'de> for __Visitor<'de> {
                type Value = ErrorCode;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "tuple struct ErrorCode",
                    )
                }
                #[inline]
                fn visit_newtype_struct<__E>(
                    self,
                    __e: __E,
                ) -> _serde::__private::Result<Self::Value, __E::Error>
                where
                    __E: _serde::Deserializer<'de>,
                {
                    let __field0: i32 = <i32 as _serde::Deserialize>::deserialize(__e)?;
                    _serde::__private::Ok(ErrorCode(__field0))
                }
                #[inline]
                fn visit_seq<__A>(
                    self,
                    mut __seq: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::SeqAccess<'de>,
                {
                    let __field0 = match _serde::de::SeqAccess::next_element::<
                        i32,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    0usize,
                                    &"tuple struct ErrorCode with 1 element",
                                ),
                            );
                        }
                    };
                    _serde::__private::Ok(ErrorCode(__field0))
                }
            }
            _serde::Deserializer::deserialize_newtype_struct(
                __deserializer,
                "ErrorCode",
                __Visitor {
                    marker: _serde::__private::PhantomData::<ErrorCode>,
                    lifetime: _serde::__private::PhantomData,
                },
            )
        }
    }
};
#[allow(unused_qualifications)]
impl std::error::Error for ErrorCode {}
#[allow(unused_qualifications)]
impl ::core::fmt::Display for ErrorCode {
    #[allow(clippy::used_underscore_binding)]
    fn fmt(&self, __formatter: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        use thiserror::__private::AsDisplay as _;
        #[allow(unused_variables, deprecated)]
        let Self(_0) = self;
        __formatter.write_fmt(format_args!("jsonrpc error {0}", _0.as_display()))
    }
}
impl From<i32> for ErrorCode {
    fn from(i: i32) -> Self {
        Self(i)
    }
}
impl ErrorCode {
    /// Invalid JSON was received by the server. An error occurred on the server while parsing the
    /// JSON text.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const PARSE_ERROR: Self = Self(-32700);
    /// The JSON sent is not a valid Request object.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const INVALID_REQUEST: Self = Self(-32600);
    /// The method does not exist / is not available.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const METHOD_NOT_FOUND: Self = Self(-32601);
    /// Invalid method parameter(s).
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const INVALID_PARAMS: Self = Self(-32602);
    /// Internal JSON-RPC error.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const INTERNAL_ERROR: Self = Self(-32603);
    /// This is the start range of JSON-RPC reserved error codes.
    /// It doesn't denote a real error code. No LSP error codes should
    /// be defined between the start and end range. For backwards
    /// compatibility the `ServerNotInitialized` and the `UnknownErrorCode`
    /// are left in the range.
    ///
    /// @since 3.16.0
    pub const JSONRPC_RESERVED_ERROR_RANGE_START: Self = Self(-32099);
    /// Error code indicating that a server received a notification or
    /// request before the server has received the `initialize` request.
    pub const SERVER_NOT_INITIALIZED: Self = Self(-32002);
    /// (Defined by LSP specification without description)
    pub const UNKNOWN_ERROR_CODE: Self = Self(-32001);
    /// This is the end range of JSON-RPC reserved error codes.
    /// It doesn't denote a real error code.
    ///
    /// @since 3.16.0
    pub const JSONRPC_RESERVED_ERROR_RANGE_END: Self = Self(-32000);
    /// This is the start range of LSP reserved error codes.
    /// It doesn't denote a real error code.
    ///
    /// @since 3.16.0
    pub const LSP_RESERVED_ERROR_RANGE_START: Self = Self(-32899);
    /// A request failed but it was syntactically correct, e.g the
    /// method name was known and the parameters were valid. The error
    /// message should contain human readable information about why
    /// the request failed.
    ///
    /// @since 3.17.0
    pub const REQUEST_FAILED: Self = Self(-32803);
    /// The server cancelled the request. This error code should
    /// only be used for requests that explicitly support being
    /// server cancellable.
    ///
    /// @since 3.17.0
    pub const SERVER_CANCELLED: Self = Self(-32802);
    /// The server detected that the content of a document got
    /// modified outside normal conditions. A server should
    /// NOT send this error code if it detects a content change
    /// in it unprocessed messages. The result even computed
    /// on an older state might still be useful for the client.
    ///
    /// If a client decides that a result is not of any use anymore
    /// the client should cancel the request.
    pub const CONTENT_MODIFIED: Self = Self(-32801);
    /// The client has canceled a request and a server as detected
    /// the cancel.
    pub const REQUEST_CANCELLED: Self = Self(-32800);
    /// This is the end range of LSP reserved error codes.
    /// It doesn't denote a real error code.
    ///
    /// @since 3.16.0
    pub const LSP_RESERVED_ERROR_RANGE_END: Self = Self(-32800);
}
/// The identifier of requests and responses.
///
/// Though `null` is technically a valid id for responses, we reject it since it hardly makes sense
/// for valid communication.
pub type RequestId = NumberOrString;
struct RawMessage<T> {
    jsonrpc: RpcVersion,
    #[serde(flatten)]
    inner: T,
}
#[automatically_derived]
impl<T: ::core::fmt::Debug> ::core::fmt::Debug for RawMessage<T> {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field2_finish(
            f,
            "RawMessage",
            "jsonrpc",
            &self.jsonrpc,
            "inner",
            &&self.inner,
        )
    }
}
#[automatically_derived]
impl<T: ::core::clone::Clone> ::core::clone::Clone for RawMessage<T> {
    #[inline]
    fn clone(&self) -> RawMessage<T> {
        RawMessage {
            jsonrpc: ::core::clone::Clone::clone(&self.jsonrpc),
            inner: ::core::clone::Clone::clone(&self.inner),
        }
    }
}
#[automatically_derived]
impl<T> ::core::marker::StructuralPartialEq for RawMessage<T> {}
#[automatically_derived]
impl<T: ::core::cmp::PartialEq> ::core::cmp::PartialEq for RawMessage<T> {
    #[inline]
    fn eq(&self, other: &RawMessage<T>) -> bool {
        self.jsonrpc == other.jsonrpc && self.inner == other.inner
    }
}
#[automatically_derived]
impl<T> ::core::marker::StructuralEq for RawMessage<T> {}
#[automatically_derived]
impl<T: ::core::cmp::Eq> ::core::cmp::Eq for RawMessage<T> {
    #[inline]
    #[doc(hidden)]
    #[coverage(off)]
    fn assert_receiver_is_total_eq(&self) -> () {
        let _: ::core::cmp::AssertParamIsEq<RpcVersion>;
        let _: ::core::cmp::AssertParamIsEq<T>;
    }
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<T> _serde::Serialize for RawMessage<T>
    where
        T: _serde::Serialize,
    {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            let mut __serde_state = _serde::Serializer::serialize_map(
                __serializer,
                _serde::__private::None,
            )?;
            _serde::ser::SerializeMap::serialize_entry(
                &mut __serde_state,
                "jsonrpc",
                &self.jsonrpc,
            )?;
            _serde::Serialize::serialize(
                &&self.inner,
                _serde::__private::ser::FlatMapSerializer(&mut __serde_state),
            )?;
            _serde::ser::SerializeMap::end(__serde_state)
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de, T> _serde::Deserialize<'de> for RawMessage<T>
    where
        T: _serde::Deserialize<'de>,
    {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            #[allow(non_camel_case_types)]
            #[doc(hidden)]
            enum __Field<'de> {
                __field0,
                __other(_serde::__private::de::Content<'de>),
            }
            #[doc(hidden)]
            struct __FieldVisitor;
            impl<'de> _serde::de::Visitor<'de> for __FieldVisitor {
                type Value = __Field<'de>;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "field identifier",
                    )
                }
                fn visit_bool<__E>(
                    self,
                    __value: bool,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::Bool(__value)),
                    )
                }
                fn visit_i8<__E>(
                    self,
                    __value: i8,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::I8(__value)),
                    )
                }
                fn visit_i16<__E>(
                    self,
                    __value: i16,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::I16(__value)),
                    )
                }
                fn visit_i32<__E>(
                    self,
                    __value: i32,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::I32(__value)),
                    )
                }
                fn visit_i64<__E>(
                    self,
                    __value: i64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::I64(__value)),
                    )
                }
                fn visit_u8<__E>(
                    self,
                    __value: u8,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::U8(__value)),
                    )
                }
                fn visit_u16<__E>(
                    self,
                    __value: u16,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::U16(__value)),
                    )
                }
                fn visit_u32<__E>(
                    self,
                    __value: u32,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::U32(__value)),
                    )
                }
                fn visit_u64<__E>(
                    self,
                    __value: u64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::U64(__value)),
                    )
                }
                fn visit_f32<__E>(
                    self,
                    __value: f32,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::F32(__value)),
                    )
                }
                fn visit_f64<__E>(
                    self,
                    __value: f64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::F64(__value)),
                    )
                }
                fn visit_char<__E>(
                    self,
                    __value: char,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::Char(__value)),
                    )
                }
                fn visit_unit<__E>(self) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    _serde::__private::Ok(
                        __Field::__other(_serde::__private::de::Content::Unit),
                    )
                }
                fn visit_str<__E>(
                    self,
                    __value: &str,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        "jsonrpc" => _serde::__private::Ok(__Field::__field0),
                        _ => {
                            let __value = _serde::__private::de::Content::String(
                                _serde::__private::ToString::to_string(__value),
                            );
                            _serde::__private::Ok(__Field::__other(__value))
                        }
                    }
                }
                fn visit_bytes<__E>(
                    self,
                    __value: &[u8],
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        b"jsonrpc" => _serde::__private::Ok(__Field::__field0),
                        _ => {
                            let __value = _serde::__private::de::Content::ByteBuf(
                                __value.to_vec(),
                            );
                            _serde::__private::Ok(__Field::__other(__value))
                        }
                    }
                }
                fn visit_borrowed_str<__E>(
                    self,
                    __value: &'de str,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        "jsonrpc" => _serde::__private::Ok(__Field::__field0),
                        _ => {
                            let __value = _serde::__private::de::Content::Str(__value);
                            _serde::__private::Ok(__Field::__other(__value))
                        }
                    }
                }
                fn visit_borrowed_bytes<__E>(
                    self,
                    __value: &'de [u8],
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        b"jsonrpc" => _serde::__private::Ok(__Field::__field0),
                        _ => {
                            let __value = _serde::__private::de::Content::Bytes(__value);
                            _serde::__private::Ok(__Field::__other(__value))
                        }
                    }
                }
            }
            impl<'de> _serde::Deserialize<'de> for __Field<'de> {
                #[inline]
                fn deserialize<__D>(
                    __deserializer: __D,
                ) -> _serde::__private::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    _serde::Deserializer::deserialize_identifier(
                        __deserializer,
                        __FieldVisitor,
                    )
                }
            }
            #[doc(hidden)]
            struct __Visitor<'de, T>
            where
                T: _serde::Deserialize<'de>,
            {
                marker: _serde::__private::PhantomData<RawMessage<T>>,
                lifetime: _serde::__private::PhantomData<&'de ()>,
            }
            impl<'de, T> _serde::de::Visitor<'de> for __Visitor<'de, T>
            where
                T: _serde::Deserialize<'de>,
            {
                type Value = RawMessage<T>;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "struct RawMessage",
                    )
                }
                #[inline]
                fn visit_map<__A>(
                    self,
                    mut __map: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::MapAccess<'de>,
                {
                    let mut __field0: _serde::__private::Option<RpcVersion> = _serde::__private::None;
                    let mut __collect = _serde::__private::Vec::<
                        _serde::__private::Option<
                            (
                                _serde::__private::de::Content,
                                _serde::__private::de::Content,
                            ),
                        >,
                    >::new();
                    while let _serde::__private::Some(__key) = _serde::de::MapAccess::next_key::<
                        __Field,
                    >(&mut __map)? {
                        match __key {
                            __Field::__field0 => {
                                if _serde::__private::Option::is_some(&__field0) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field(
                                            "jsonrpc",
                                        ),
                                    );
                                }
                                __field0 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<RpcVersion>(&mut __map)?,
                                );
                            }
                            __Field::__other(__name) => {
                                __collect
                                    .push(
                                        _serde::__private::Some((
                                            __name,
                                            _serde::de::MapAccess::next_value(&mut __map)?,
                                        )),
                                    );
                            }
                        }
                    }
                    let __field0 = match __field0 {
                        _serde::__private::Some(__field0) => __field0,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("jsonrpc")?
                        }
                    };
                    let __field1: T = _serde::de::Deserialize::deserialize(
                        _serde::__private::de::FlatMapDeserializer(
                            &mut __collect,
                            _serde::__private::PhantomData,
                        ),
                    )?;
                    _serde::__private::Ok(RawMessage {
                        jsonrpc: __field0,
                        inner: __field1,
                    })
                }
            }
            _serde::Deserializer::deserialize_map(
                __deserializer,
                __Visitor {
                    marker: _serde::__private::PhantomData::<RawMessage<T>>,
                    lifetime: _serde::__private::PhantomData,
                },
            )
        }
    }
};
impl<T> RawMessage<T> {
    fn new(inner: T) -> Self {
        Self {
            jsonrpc: RpcVersion::V2,
            inner,
        }
    }
}
enum RpcVersion {
    #[serde(rename = "2.0")]
    V2,
}
#[automatically_derived]
impl ::core::fmt::Debug for RpcVersion {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::write_str(f, "V2")
    }
}
#[automatically_derived]
impl ::core::clone::Clone for RpcVersion {
    #[inline]
    fn clone(&self) -> RpcVersion {
        *self
    }
}
#[automatically_derived]
impl ::core::marker::Copy for RpcVersion {}
#[automatically_derived]
impl ::core::marker::StructuralPartialEq for RpcVersion {}
#[automatically_derived]
impl ::core::cmp::PartialEq for RpcVersion {
    #[inline]
    fn eq(&self, other: &RpcVersion) -> bool {
        true
    }
}
#[automatically_derived]
impl ::core::marker::StructuralEq for RpcVersion {}
#[automatically_derived]
impl ::core::cmp::Eq for RpcVersion {
    #[inline]
    #[doc(hidden)]
    #[coverage(off)]
    fn assert_receiver_is_total_eq(&self) -> () {}
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl _serde::Serialize for RpcVersion {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            match *self {
                RpcVersion::V2 => {
                    _serde::Serializer::serialize_unit_variant(
                        __serializer,
                        "RpcVersion",
                        0u32,
                        "2.0",
                    )
                }
            }
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for RpcVersion {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            #[allow(non_camel_case_types)]
            #[doc(hidden)]
            enum __Field {
                __field0,
            }
            #[doc(hidden)]
            struct __FieldVisitor;
            impl<'de> _serde::de::Visitor<'de> for __FieldVisitor {
                type Value = __Field;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "variant identifier",
                    )
                }
                fn visit_u64<__E>(
                    self,
                    __value: u64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        0u64 => _serde::__private::Ok(__Field::__field0),
                        _ => {
                            _serde::__private::Err(
                                _serde::de::Error::invalid_value(
                                    _serde::de::Unexpected::Unsigned(__value),
                                    &"variant index 0 <= i < 1",
                                ),
                            )
                        }
                    }
                }
                fn visit_str<__E>(
                    self,
                    __value: &str,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        "2.0" => _serde::__private::Ok(__Field::__field0),
                        _ => {
                            _serde::__private::Err(
                                _serde::de::Error::unknown_variant(__value, VARIANTS),
                            )
                        }
                    }
                }
                fn visit_bytes<__E>(
                    self,
                    __value: &[u8],
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        b"2.0" => _serde::__private::Ok(__Field::__field0),
                        _ => {
                            let __value = &_serde::__private::from_utf8_lossy(__value);
                            _serde::__private::Err(
                                _serde::de::Error::unknown_variant(__value, VARIANTS),
                            )
                        }
                    }
                }
            }
            impl<'de> _serde::Deserialize<'de> for __Field {
                #[inline]
                fn deserialize<__D>(
                    __deserializer: __D,
                ) -> _serde::__private::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    _serde::Deserializer::deserialize_identifier(
                        __deserializer,
                        __FieldVisitor,
                    )
                }
            }
            #[doc(hidden)]
            struct __Visitor<'de> {
                marker: _serde::__private::PhantomData<RpcVersion>,
                lifetime: _serde::__private::PhantomData<&'de ()>,
            }
            impl<'de> _serde::de::Visitor<'de> for __Visitor<'de> {
                type Value = RpcVersion;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "enum RpcVersion",
                    )
                }
                fn visit_enum<__A>(
                    self,
                    __data: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::EnumAccess<'de>,
                {
                    match _serde::de::EnumAccess::variant(__data)? {
                        (__Field::__field0, __variant) => {
                            _serde::de::VariantAccess::unit_variant(__variant)?;
                            _serde::__private::Ok(RpcVersion::V2)
                        }
                    }
                }
            }
            #[doc(hidden)]
            const VARIANTS: &'static [&'static str] = &["2.0"];
            _serde::Deserializer::deserialize_enum(
                __deserializer,
                "RpcVersion",
                VARIANTS,
                __Visitor {
                    marker: _serde::__private::PhantomData::<RpcVersion>,
                    lifetime: _serde::__private::PhantomData,
                },
            )
        }
    }
};
#[serde(untagged)]
enum Message {
    Request(AnyRequest),
    Response(AnyResponse),
    Notification(AnyNotification),
}
#[automatically_derived]
impl ::core::fmt::Debug for Message {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        match self {
            Message::Request(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(
                    f,
                    "Request",
                    &__self_0,
                )
            }
            Message::Response(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(
                    f,
                    "Response",
                    &__self_0,
                )
            }
            Message::Notification(__self_0) => {
                ::core::fmt::Formatter::debug_tuple_field1_finish(
                    f,
                    "Notification",
                    &__self_0,
                )
            }
        }
    }
}
#[automatically_derived]
impl ::core::clone::Clone for Message {
    #[inline]
    fn clone(&self) -> Message {
        match self {
            Message::Request(__self_0) => {
                Message::Request(::core::clone::Clone::clone(__self_0))
            }
            Message::Response(__self_0) => {
                Message::Response(::core::clone::Clone::clone(__self_0))
            }
            Message::Notification(__self_0) => {
                Message::Notification(::core::clone::Clone::clone(__self_0))
            }
        }
    }
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl _serde::Serialize for Message {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            match *self {
                Message::Request(ref __field0) => {
                    _serde::Serialize::serialize(__field0, __serializer)
                }
                Message::Response(ref __field0) => {
                    _serde::Serialize::serialize(__field0, __serializer)
                }
                Message::Notification(ref __field0) => {
                    _serde::Serialize::serialize(__field0, __serializer)
                }
            }
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for Message {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            let __content = <_serde::__private::de::Content as _serde::Deserialize>::deserialize(
                __deserializer,
            )?;
            let __deserializer = _serde::__private::de::ContentRefDeserializer::<
                __D::Error,
            >::new(&__content);
            if let _serde::__private::Ok(__ok) = _serde::__private::Result::map(
                <AnyRequest as _serde::Deserialize>::deserialize(__deserializer),
                Message::Request,
            ) {
                return _serde::__private::Ok(__ok);
            }
            if let _serde::__private::Ok(__ok) = _serde::__private::Result::map(
                <AnyResponse as _serde::Deserialize>::deserialize(__deserializer),
                Message::Response,
            ) {
                return _serde::__private::Ok(__ok);
            }
            if let _serde::__private::Ok(__ok) = _serde::__private::Result::map(
                <AnyNotification as _serde::Deserialize>::deserialize(__deserializer),
                Message::Notification,
            ) {
                return _serde::__private::Ok(__ok);
            }
            _serde::__private::Err(
                _serde::de::Error::custom(
                    "data did not match any variant of untagged enum Message",
                ),
            )
        }
    }
};
/// A dynamic runtime [LSP request](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#requestMessage).
#[non_exhaustive]
pub struct AnyRequest {
    /// The request id.
    pub id: RequestId,
    /// The method to be invoked.
    pub method: String,
    /// The method's params.
    #[serde(default)]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
}
#[automatically_derived]
impl ::core::fmt::Debug for AnyRequest {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field3_finish(
            f,
            "AnyRequest",
            "id",
            &self.id,
            "method",
            &self.method,
            "params",
            &&self.params,
        )
    }
}
#[automatically_derived]
impl ::core::clone::Clone for AnyRequest {
    #[inline]
    fn clone(&self) -> AnyRequest {
        AnyRequest {
            id: ::core::clone::Clone::clone(&self.id),
            method: ::core::clone::Clone::clone(&self.method),
            params: ::core::clone::Clone::clone(&self.params),
        }
    }
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl _serde::Serialize for AnyRequest {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            let mut __serde_state = _serde::Serializer::serialize_struct(
                __serializer,
                "AnyRequest",
                false as usize + 1 + 1
                    + if serde_json::Value::is_null(&self.params) { 0 } else { 1 },
            )?;
            _serde::ser::SerializeStruct::serialize_field(
                &mut __serde_state,
                "id",
                &self.id,
            )?;
            _serde::ser::SerializeStruct::serialize_field(
                &mut __serde_state,
                "method",
                &self.method,
            )?;
            if !serde_json::Value::is_null(&self.params) {
                _serde::ser::SerializeStruct::serialize_field(
                    &mut __serde_state,
                    "params",
                    &self.params,
                )?;
            } else {
                _serde::ser::SerializeStruct::skip_field(&mut __serde_state, "params")?;
            }
            _serde::ser::SerializeStruct::end(__serde_state)
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for AnyRequest {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            #[allow(non_camel_case_types)]
            #[doc(hidden)]
            enum __Field {
                __field0,
                __field1,
                __field2,
                __ignore,
            }
            #[doc(hidden)]
            struct __FieldVisitor;
            impl<'de> _serde::de::Visitor<'de> for __FieldVisitor {
                type Value = __Field;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "field identifier",
                    )
                }
                fn visit_u64<__E>(
                    self,
                    __value: u64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        0u64 => _serde::__private::Ok(__Field::__field0),
                        1u64 => _serde::__private::Ok(__Field::__field1),
                        2u64 => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_str<__E>(
                    self,
                    __value: &str,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        "id" => _serde::__private::Ok(__Field::__field0),
                        "method" => _serde::__private::Ok(__Field::__field1),
                        "params" => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_bytes<__E>(
                    self,
                    __value: &[u8],
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        b"id" => _serde::__private::Ok(__Field::__field0),
                        b"method" => _serde::__private::Ok(__Field::__field1),
                        b"params" => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
            }
            impl<'de> _serde::Deserialize<'de> for __Field {
                #[inline]
                fn deserialize<__D>(
                    __deserializer: __D,
                ) -> _serde::__private::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    _serde::Deserializer::deserialize_identifier(
                        __deserializer,
                        __FieldVisitor,
                    )
                }
            }
            #[doc(hidden)]
            struct __Visitor<'de> {
                marker: _serde::__private::PhantomData<AnyRequest>,
                lifetime: _serde::__private::PhantomData<&'de ()>,
            }
            impl<'de> _serde::de::Visitor<'de> for __Visitor<'de> {
                type Value = AnyRequest;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "struct AnyRequest",
                    )
                }
                #[inline]
                fn visit_seq<__A>(
                    self,
                    mut __seq: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::SeqAccess<'de>,
                {
                    let __field0 = match _serde::de::SeqAccess::next_element::<
                        RequestId,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    0usize,
                                    &"struct AnyRequest with 3 elements",
                                ),
                            );
                        }
                    };
                    let __field1 = match _serde::de::SeqAccess::next_element::<
                        String,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    1usize,
                                    &"struct AnyRequest with 3 elements",
                                ),
                            );
                        }
                    };
                    let __field2 = match _serde::de::SeqAccess::next_element::<
                        serde_json::Value,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => _serde::__private::Default::default(),
                    };
                    _serde::__private::Ok(AnyRequest {
                        id: __field0,
                        method: __field1,
                        params: __field2,
                    })
                }
                #[inline]
                fn visit_map<__A>(
                    self,
                    mut __map: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::MapAccess<'de>,
                {
                    let mut __field0: _serde::__private::Option<RequestId> = _serde::__private::None;
                    let mut __field1: _serde::__private::Option<String> = _serde::__private::None;
                    let mut __field2: _serde::__private::Option<serde_json::Value> = _serde::__private::None;
                    while let _serde::__private::Some(__key) = _serde::de::MapAccess::next_key::<
                        __Field,
                    >(&mut __map)? {
                        match __key {
                            __Field::__field0 => {
                                if _serde::__private::Option::is_some(&__field0) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("id"),
                                    );
                                }
                                __field0 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<RequestId>(&mut __map)?,
                                );
                            }
                            __Field::__field1 => {
                                if _serde::__private::Option::is_some(&__field1) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("method"),
                                    );
                                }
                                __field1 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<String>(&mut __map)?,
                                );
                            }
                            __Field::__field2 => {
                                if _serde::__private::Option::is_some(&__field2) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("params"),
                                    );
                                }
                                __field2 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<
                                        serde_json::Value,
                                    >(&mut __map)?,
                                );
                            }
                            _ => {
                                let _ = _serde::de::MapAccess::next_value::<
                                    _serde::de::IgnoredAny,
                                >(&mut __map)?;
                            }
                        }
                    }
                    let __field0 = match __field0 {
                        _serde::__private::Some(__field0) => __field0,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("id")?
                        }
                    };
                    let __field1 = match __field1 {
                        _serde::__private::Some(__field1) => __field1,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("method")?
                        }
                    };
                    let __field2 = match __field2 {
                        _serde::__private::Some(__field2) => __field2,
                        _serde::__private::None => _serde::__private::Default::default(),
                    };
                    _serde::__private::Ok(AnyRequest {
                        id: __field0,
                        method: __field1,
                        params: __field2,
                    })
                }
            }
            #[doc(hidden)]
            const FIELDS: &'static [&'static str] = &["id", "method", "params"];
            _serde::Deserializer::deserialize_struct(
                __deserializer,
                "AnyRequest",
                FIELDS,
                __Visitor {
                    marker: _serde::__private::PhantomData::<AnyRequest>,
                    lifetime: _serde::__private::PhantomData,
                },
            )
        }
    }
};
/// A dynamic runtime [LSP notification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#notificationMessage).
#[non_exhaustive]
pub struct AnyNotification {
    /// The method to be invoked.
    pub method: String,
    /// The notification's params.
    #[serde(default)]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: JsonValue,
}
#[automatically_derived]
impl ::core::fmt::Debug for AnyNotification {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field2_finish(
            f,
            "AnyNotification",
            "method",
            &self.method,
            "params",
            &&self.params,
        )
    }
}
#[automatically_derived]
impl ::core::clone::Clone for AnyNotification {
    #[inline]
    fn clone(&self) -> AnyNotification {
        AnyNotification {
            method: ::core::clone::Clone::clone(&self.method),
            params: ::core::clone::Clone::clone(&self.params),
        }
    }
}
#[automatically_derived]
impl ::core::marker::StructuralPartialEq for AnyNotification {}
#[automatically_derived]
impl ::core::cmp::PartialEq for AnyNotification {
    #[inline]
    fn eq(&self, other: &AnyNotification) -> bool {
        self.method == other.method && self.params == other.params
    }
}
#[automatically_derived]
impl ::core::marker::StructuralEq for AnyNotification {}
#[automatically_derived]
impl ::core::cmp::Eq for AnyNotification {
    #[inline]
    #[doc(hidden)]
    #[coverage(off)]
    fn assert_receiver_is_total_eq(&self) -> () {
        let _: ::core::cmp::AssertParamIsEq<String>;
        let _: ::core::cmp::AssertParamIsEq<JsonValue>;
    }
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl _serde::Serialize for AnyNotification {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            let mut __serde_state = _serde::Serializer::serialize_struct(
                __serializer,
                "AnyNotification",
                false as usize + 1
                    + if serde_json::Value::is_null(&self.params) { 0 } else { 1 },
            )?;
            _serde::ser::SerializeStruct::serialize_field(
                &mut __serde_state,
                "method",
                &self.method,
            )?;
            if !serde_json::Value::is_null(&self.params) {
                _serde::ser::SerializeStruct::serialize_field(
                    &mut __serde_state,
                    "params",
                    &self.params,
                )?;
            } else {
                _serde::ser::SerializeStruct::skip_field(&mut __serde_state, "params")?;
            }
            _serde::ser::SerializeStruct::end(__serde_state)
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for AnyNotification {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            #[allow(non_camel_case_types)]
            #[doc(hidden)]
            enum __Field {
                __field0,
                __field1,
                __ignore,
            }
            #[doc(hidden)]
            struct __FieldVisitor;
            impl<'de> _serde::de::Visitor<'de> for __FieldVisitor {
                type Value = __Field;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "field identifier",
                    )
                }
                fn visit_u64<__E>(
                    self,
                    __value: u64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        0u64 => _serde::__private::Ok(__Field::__field0),
                        1u64 => _serde::__private::Ok(__Field::__field1),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_str<__E>(
                    self,
                    __value: &str,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        "method" => _serde::__private::Ok(__Field::__field0),
                        "params" => _serde::__private::Ok(__Field::__field1),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_bytes<__E>(
                    self,
                    __value: &[u8],
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        b"method" => _serde::__private::Ok(__Field::__field0),
                        b"params" => _serde::__private::Ok(__Field::__field1),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
            }
            impl<'de> _serde::Deserialize<'de> for __Field {
                #[inline]
                fn deserialize<__D>(
                    __deserializer: __D,
                ) -> _serde::__private::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    _serde::Deserializer::deserialize_identifier(
                        __deserializer,
                        __FieldVisitor,
                    )
                }
            }
            #[doc(hidden)]
            struct __Visitor<'de> {
                marker: _serde::__private::PhantomData<AnyNotification>,
                lifetime: _serde::__private::PhantomData<&'de ()>,
            }
            impl<'de> _serde::de::Visitor<'de> for __Visitor<'de> {
                type Value = AnyNotification;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "struct AnyNotification",
                    )
                }
                #[inline]
                fn visit_seq<__A>(
                    self,
                    mut __seq: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::SeqAccess<'de>,
                {
                    let __field0 = match _serde::de::SeqAccess::next_element::<
                        String,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    0usize,
                                    &"struct AnyNotification with 2 elements",
                                ),
                            );
                        }
                    };
                    let __field1 = match _serde::de::SeqAccess::next_element::<
                        JsonValue,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => _serde::__private::Default::default(),
                    };
                    _serde::__private::Ok(AnyNotification {
                        method: __field0,
                        params: __field1,
                    })
                }
                #[inline]
                fn visit_map<__A>(
                    self,
                    mut __map: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::MapAccess<'de>,
                {
                    let mut __field0: _serde::__private::Option<String> = _serde::__private::None;
                    let mut __field1: _serde::__private::Option<JsonValue> = _serde::__private::None;
                    while let _serde::__private::Some(__key) = _serde::de::MapAccess::next_key::<
                        __Field,
                    >(&mut __map)? {
                        match __key {
                            __Field::__field0 => {
                                if _serde::__private::Option::is_some(&__field0) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("method"),
                                    );
                                }
                                __field0 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<String>(&mut __map)?,
                                );
                            }
                            __Field::__field1 => {
                                if _serde::__private::Option::is_some(&__field1) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("params"),
                                    );
                                }
                                __field1 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<JsonValue>(&mut __map)?,
                                );
                            }
                            _ => {
                                let _ = _serde::de::MapAccess::next_value::<
                                    _serde::de::IgnoredAny,
                                >(&mut __map)?;
                            }
                        }
                    }
                    let __field0 = match __field0 {
                        _serde::__private::Some(__field0) => __field0,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("method")?
                        }
                    };
                    let __field1 = match __field1 {
                        _serde::__private::Some(__field1) => __field1,
                        _serde::__private::None => _serde::__private::Default::default(),
                    };
                    _serde::__private::Ok(AnyNotification {
                        method: __field0,
                        params: __field1,
                    })
                }
            }
            #[doc(hidden)]
            const FIELDS: &'static [&'static str] = &["method", "params"];
            _serde::Deserializer::deserialize_struct(
                __deserializer,
                "AnyNotification",
                FIELDS,
                __Visitor {
                    marker: _serde::__private::PhantomData::<AnyNotification>,
                    lifetime: _serde::__private::PhantomData,
                },
            )
        }
    }
};
/// A dynamic runtime response.
#[non_exhaustive]
struct AnyResponse {
    id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ResponseError>,
}
#[automatically_derived]
impl ::core::fmt::Debug for AnyResponse {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field3_finish(
            f,
            "AnyResponse",
            "id",
            &self.id,
            "result",
            &self.result,
            "error",
            &&self.error,
        )
    }
}
#[automatically_derived]
impl ::core::clone::Clone for AnyResponse {
    #[inline]
    fn clone(&self) -> AnyResponse {
        AnyResponse {
            id: ::core::clone::Clone::clone(&self.id),
            result: ::core::clone::Clone::clone(&self.result),
            error: ::core::clone::Clone::clone(&self.error),
        }
    }
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl _serde::Serialize for AnyResponse {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            let mut __serde_state = _serde::Serializer::serialize_struct(
                __serializer,
                "AnyResponse",
                false as usize + 1 + if Option::is_none(&self.result) { 0 } else { 1 }
                    + if Option::is_none(&self.error) { 0 } else { 1 },
            )?;
            _serde::ser::SerializeStruct::serialize_field(
                &mut __serde_state,
                "id",
                &self.id,
            )?;
            if !Option::is_none(&self.result) {
                _serde::ser::SerializeStruct::serialize_field(
                    &mut __serde_state,
                    "result",
                    &self.result,
                )?;
            } else {
                _serde::ser::SerializeStruct::skip_field(&mut __serde_state, "result")?;
            }
            if !Option::is_none(&self.error) {
                _serde::ser::SerializeStruct::serialize_field(
                    &mut __serde_state,
                    "error",
                    &self.error,
                )?;
            } else {
                _serde::ser::SerializeStruct::skip_field(&mut __serde_state, "error")?;
            }
            _serde::ser::SerializeStruct::end(__serde_state)
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for AnyResponse {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            #[allow(non_camel_case_types)]
            #[doc(hidden)]
            enum __Field {
                __field0,
                __field1,
                __field2,
                __ignore,
            }
            #[doc(hidden)]
            struct __FieldVisitor;
            impl<'de> _serde::de::Visitor<'de> for __FieldVisitor {
                type Value = __Field;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "field identifier",
                    )
                }
                fn visit_u64<__E>(
                    self,
                    __value: u64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        0u64 => _serde::__private::Ok(__Field::__field0),
                        1u64 => _serde::__private::Ok(__Field::__field1),
                        2u64 => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_str<__E>(
                    self,
                    __value: &str,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        "id" => _serde::__private::Ok(__Field::__field0),
                        "result" => _serde::__private::Ok(__Field::__field1),
                        "error" => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_bytes<__E>(
                    self,
                    __value: &[u8],
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        b"id" => _serde::__private::Ok(__Field::__field0),
                        b"result" => _serde::__private::Ok(__Field::__field1),
                        b"error" => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
            }
            impl<'de> _serde::Deserialize<'de> for __Field {
                #[inline]
                fn deserialize<__D>(
                    __deserializer: __D,
                ) -> _serde::__private::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    _serde::Deserializer::deserialize_identifier(
                        __deserializer,
                        __FieldVisitor,
                    )
                }
            }
            #[doc(hidden)]
            struct __Visitor<'de> {
                marker: _serde::__private::PhantomData<AnyResponse>,
                lifetime: _serde::__private::PhantomData<&'de ()>,
            }
            impl<'de> _serde::de::Visitor<'de> for __Visitor<'de> {
                type Value = AnyResponse;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "struct AnyResponse",
                    )
                }
                #[inline]
                fn visit_seq<__A>(
                    self,
                    mut __seq: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::SeqAccess<'de>,
                {
                    let __field0 = match _serde::de::SeqAccess::next_element::<
                        RequestId,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    0usize,
                                    &"struct AnyResponse with 3 elements",
                                ),
                            );
                        }
                    };
                    let __field1 = match _serde::de::SeqAccess::next_element::<
                        Option<JsonValue>,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    1usize,
                                    &"struct AnyResponse with 3 elements",
                                ),
                            );
                        }
                    };
                    let __field2 = match _serde::de::SeqAccess::next_element::<
                        Option<ResponseError>,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    2usize,
                                    &"struct AnyResponse with 3 elements",
                                ),
                            );
                        }
                    };
                    _serde::__private::Ok(AnyResponse {
                        id: __field0,
                        result: __field1,
                        error: __field2,
                    })
                }
                #[inline]
                fn visit_map<__A>(
                    self,
                    mut __map: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::MapAccess<'de>,
                {
                    let mut __field0: _serde::__private::Option<RequestId> = _serde::__private::None;
                    let mut __field1: _serde::__private::Option<Option<JsonValue>> = _serde::__private::None;
                    let mut __field2: _serde::__private::Option<Option<ResponseError>> = _serde::__private::None;
                    while let _serde::__private::Some(__key) = _serde::de::MapAccess::next_key::<
                        __Field,
                    >(&mut __map)? {
                        match __key {
                            __Field::__field0 => {
                                if _serde::__private::Option::is_some(&__field0) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("id"),
                                    );
                                }
                                __field0 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<RequestId>(&mut __map)?,
                                );
                            }
                            __Field::__field1 => {
                                if _serde::__private::Option::is_some(&__field1) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("result"),
                                    );
                                }
                                __field1 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<
                                        Option<JsonValue>,
                                    >(&mut __map)?,
                                );
                            }
                            __Field::__field2 => {
                                if _serde::__private::Option::is_some(&__field2) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("error"),
                                    );
                                }
                                __field2 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<
                                        Option<ResponseError>,
                                    >(&mut __map)?,
                                );
                            }
                            _ => {
                                let _ = _serde::de::MapAccess::next_value::<
                                    _serde::de::IgnoredAny,
                                >(&mut __map)?;
                            }
                        }
                    }
                    let __field0 = match __field0 {
                        _serde::__private::Some(__field0) => __field0,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("id")?
                        }
                    };
                    let __field1 = match __field1 {
                        _serde::__private::Some(__field1) => __field1,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("result")?
                        }
                    };
                    let __field2 = match __field2 {
                        _serde::__private::Some(__field2) => __field2,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("error")?
                        }
                    };
                    _serde::__private::Ok(AnyResponse {
                        id: __field0,
                        result: __field1,
                        error: __field2,
                    })
                }
            }
            #[doc(hidden)]
            const FIELDS: &'static [&'static str] = &["id", "result", "error"];
            _serde::Deserializer::deserialize_struct(
                __deserializer,
                "AnyResponse",
                FIELDS,
                __Visitor {
                    marker: _serde::__private::PhantomData::<AnyResponse>,
                    lifetime: _serde::__private::PhantomData,
                },
            )
        }
    }
};
/// The error object in case a request fails.
///
/// See:
/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#responseError>
#[non_exhaustive]
#[error("{message} ({code})")]
pub struct ResponseError {
    /// A number indicating the error type that occurred.
    pub code: ErrorCode,
    /// A string providing a short description of the error.
    pub message: String,
    /// A primitive or structured value that contains additional
    /// information about the error. Can be omitted.
    pub data: Option<JsonValue>,
}
#[automatically_derived]
impl ::core::fmt::Debug for ResponseError {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field3_finish(
            f,
            "ResponseError",
            "code",
            &self.code,
            "message",
            &self.message,
            "data",
            &&self.data,
        )
    }
}
#[automatically_derived]
impl ::core::clone::Clone for ResponseError {
    #[inline]
    fn clone(&self) -> ResponseError {
        ResponseError {
            code: ::core::clone::Clone::clone(&self.code),
            message: ::core::clone::Clone::clone(&self.message),
            data: ::core::clone::Clone::clone(&self.data),
        }
    }
}
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl _serde::Serialize for ResponseError {
        fn serialize<__S>(
            &self,
            __serializer: __S,
        ) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            let mut __serde_state = _serde::Serializer::serialize_struct(
                __serializer,
                "ResponseError",
                false as usize + 1 + 1 + 1,
            )?;
            _serde::ser::SerializeStruct::serialize_field(
                &mut __serde_state,
                "code",
                &self.code,
            )?;
            _serde::ser::SerializeStruct::serialize_field(
                &mut __serde_state,
                "message",
                &self.message,
            )?;
            _serde::ser::SerializeStruct::serialize_field(
                &mut __serde_state,
                "data",
                &self.data,
            )?;
            _serde::ser::SerializeStruct::end(__serde_state)
        }
    }
};
#[doc(hidden)]
#[allow(non_upper_case_globals, unused_attributes, unused_qualifications)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de> _serde::Deserialize<'de> for ResponseError {
        fn deserialize<__D>(
            __deserializer: __D,
        ) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            #[allow(non_camel_case_types)]
            #[doc(hidden)]
            enum __Field {
                __field0,
                __field1,
                __field2,
                __ignore,
            }
            #[doc(hidden)]
            struct __FieldVisitor;
            impl<'de> _serde::de::Visitor<'de> for __FieldVisitor {
                type Value = __Field;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "field identifier",
                    )
                }
                fn visit_u64<__E>(
                    self,
                    __value: u64,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        0u64 => _serde::__private::Ok(__Field::__field0),
                        1u64 => _serde::__private::Ok(__Field::__field1),
                        2u64 => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_str<__E>(
                    self,
                    __value: &str,
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        "code" => _serde::__private::Ok(__Field::__field0),
                        "message" => _serde::__private::Ok(__Field::__field1),
                        "data" => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
                fn visit_bytes<__E>(
                    self,
                    __value: &[u8],
                ) -> _serde::__private::Result<Self::Value, __E>
                where
                    __E: _serde::de::Error,
                {
                    match __value {
                        b"code" => _serde::__private::Ok(__Field::__field0),
                        b"message" => _serde::__private::Ok(__Field::__field1),
                        b"data" => _serde::__private::Ok(__Field::__field2),
                        _ => _serde::__private::Ok(__Field::__ignore),
                    }
                }
            }
            impl<'de> _serde::Deserialize<'de> for __Field {
                #[inline]
                fn deserialize<__D>(
                    __deserializer: __D,
                ) -> _serde::__private::Result<Self, __D::Error>
                where
                    __D: _serde::Deserializer<'de>,
                {
                    _serde::Deserializer::deserialize_identifier(
                        __deserializer,
                        __FieldVisitor,
                    )
                }
            }
            #[doc(hidden)]
            struct __Visitor<'de> {
                marker: _serde::__private::PhantomData<ResponseError>,
                lifetime: _serde::__private::PhantomData<&'de ()>,
            }
            impl<'de> _serde::de::Visitor<'de> for __Visitor<'de> {
                type Value = ResponseError;
                fn expecting(
                    &self,
                    __formatter: &mut _serde::__private::Formatter,
                ) -> _serde::__private::fmt::Result {
                    _serde::__private::Formatter::write_str(
                        __formatter,
                        "struct ResponseError",
                    )
                }
                #[inline]
                fn visit_seq<__A>(
                    self,
                    mut __seq: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::SeqAccess<'de>,
                {
                    let __field0 = match _serde::de::SeqAccess::next_element::<
                        ErrorCode,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    0usize,
                                    &"struct ResponseError with 3 elements",
                                ),
                            );
                        }
                    };
                    let __field1 = match _serde::de::SeqAccess::next_element::<
                        String,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    1usize,
                                    &"struct ResponseError with 3 elements",
                                ),
                            );
                        }
                    };
                    let __field2 = match _serde::de::SeqAccess::next_element::<
                        Option<JsonValue>,
                    >(&mut __seq)? {
                        _serde::__private::Some(__value) => __value,
                        _serde::__private::None => {
                            return _serde::__private::Err(
                                _serde::de::Error::invalid_length(
                                    2usize,
                                    &"struct ResponseError with 3 elements",
                                ),
                            );
                        }
                    };
                    _serde::__private::Ok(ResponseError {
                        code: __field0,
                        message: __field1,
                        data: __field2,
                    })
                }
                #[inline]
                fn visit_map<__A>(
                    self,
                    mut __map: __A,
                ) -> _serde::__private::Result<Self::Value, __A::Error>
                where
                    __A: _serde::de::MapAccess<'de>,
                {
                    let mut __field0: _serde::__private::Option<ErrorCode> = _serde::__private::None;
                    let mut __field1: _serde::__private::Option<String> = _serde::__private::None;
                    let mut __field2: _serde::__private::Option<Option<JsonValue>> = _serde::__private::None;
                    while let _serde::__private::Some(__key) = _serde::de::MapAccess::next_key::<
                        __Field,
                    >(&mut __map)? {
                        match __key {
                            __Field::__field0 => {
                                if _serde::__private::Option::is_some(&__field0) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("code"),
                                    );
                                }
                                __field0 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<ErrorCode>(&mut __map)?,
                                );
                            }
                            __Field::__field1 => {
                                if _serde::__private::Option::is_some(&__field1) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field(
                                            "message",
                                        ),
                                    );
                                }
                                __field1 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<String>(&mut __map)?,
                                );
                            }
                            __Field::__field2 => {
                                if _serde::__private::Option::is_some(&__field2) {
                                    return _serde::__private::Err(
                                        <__A::Error as _serde::de::Error>::duplicate_field("data"),
                                    );
                                }
                                __field2 = _serde::__private::Some(
                                    _serde::de::MapAccess::next_value::<
                                        Option<JsonValue>,
                                    >(&mut __map)?,
                                );
                            }
                            _ => {
                                let _ = _serde::de::MapAccess::next_value::<
                                    _serde::de::IgnoredAny,
                                >(&mut __map)?;
                            }
                        }
                    }
                    let __field0 = match __field0 {
                        _serde::__private::Some(__field0) => __field0,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("code")?
                        }
                    };
                    let __field1 = match __field1 {
                        _serde::__private::Some(__field1) => __field1,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("message")?
                        }
                    };
                    let __field2 = match __field2 {
                        _serde::__private::Some(__field2) => __field2,
                        _serde::__private::None => {
                            _serde::__private::de::missing_field("data")?
                        }
                    };
                    _serde::__private::Ok(ResponseError {
                        code: __field0,
                        message: __field1,
                        data: __field2,
                    })
                }
            }
            #[doc(hidden)]
            const FIELDS: &'static [&'static str] = &["code", "message", "data"];
            _serde::Deserializer::deserialize_struct(
                __deserializer,
                "ResponseError",
                FIELDS,
                __Visitor {
                    marker: _serde::__private::PhantomData::<ResponseError>,
                    lifetime: _serde::__private::PhantomData,
                },
            )
        }
    }
};
#[allow(unused_qualifications)]
impl std::error::Error for ResponseError {}
#[allow(unused_qualifications)]
impl ::core::fmt::Display for ResponseError {
    #[allow(clippy::used_underscore_binding)]
    fn fmt(&self, __formatter: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        use thiserror::__private::AsDisplay as _;
        #[allow(unused_variables, deprecated)]
        let Self { code, message, data } = self;
        __formatter
            .write_fmt(
                format_args!("{0} ({1})", message.as_display(), code.as_display()),
            )
    }
}
impl ResponseError {
    /// Create a new error object with a JSON-RPC error code and a message.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
            data: None,
        }
    }
    /// Create a new error object with a JSON-RPC error code, a message, and any additional data.
    #[must_use]
    pub fn new_with_data(
        code: ErrorCode,
        message: impl fmt::Display,
        data: JsonValue,
    ) -> Self {
        Self {
            code,
            message: message.to_string(),
            data: Some(data),
        }
    }
}
impl Message {
    const CONTENT_LENGTH: &'static str = "Content-Length";
    async fn read(mut reader: impl AsyncBufRead + Unpin) -> Result<Self> {
        let mut line = String::new();
        let mut content_len = None;
        loop {
            line.clear();
            reader.read_line(&mut line).await?;
            if line.is_empty() {
                return Err(Error::Eof);
            }
            if line == "\r\n" {
                break;
            }
            let (name, value) = line
                .strip_suffix("\r\n")
                .and_then(|line| line.split_once(": "))
                .ok_or_else(|| Error::Protocol({
                    let res = ::alloc::fmt::format(
                        format_args!("Invalid header: {0:?}", line),
                    );
                    res
                }))?;
            if name.eq_ignore_ascii_case(Self::CONTENT_LENGTH) {
                let value = value
                    .parse::<usize>()
                    .map_err(|_| Error::Protocol({
                        let res = ::alloc::fmt::format(
                            format_args!("Invalid content-length: {0}", value),
                        );
                        res
                    }))?;
                content_len = Some(value);
            }
        }
        let content_len = content_len
            .ok_or_else(|| Error::Protocol("Missing content-length".into()))?;
        let mut buf = ::alloc::vec::from_elem(0u8, content_len);
        reader.read_exact(&mut buf).await?;
        let msg = serde_json::from_slice::<RawMessage<Self>>(&buf)?;
        Ok(msg.inner)
    }
    async fn write(&self, mut writer: impl AsyncWrite + Unpin) -> Result<()> {
        let buf = serde_json::to_string(&RawMessage::new(self))?;
        writer
            .write_all(
                {
                    let res = ::alloc::fmt::format(
                        format_args!("{0}: {1}\r\n\r\n", Self::CONTENT_LENGTH, buf.len()),
                    );
                    res
                }
                    .as_bytes(),
            )
            .await?;
        writer.write_all(buf.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}
/// Service main loop driver for either Language Servers or Language Clients.
pub struct MainLoop<S: LspService> {
    service: S,
    rx: mpsc::UnboundedReceiver<MainLoopEvent>,
    outgoing_id: i32,
    outgoing: HashMap<RequestId, oneshot::Sender<AnyResponse>>,
    tasks: FuturesUnordered<RequestFuture<S::Future>>,
}
enum MainLoopEvent {
    Outgoing(Message),
    OutgoingRequest(AnyRequest, oneshot::Sender<AnyResponse>),
    Any(AnyEvent),
}
impl<S: LspService> MainLoop<S> {
    /// Get a reference to the inner service.
    #[must_use]
    pub fn get_ref(&self) -> &S {
        &self.service
    }
    /// Get a mutable reference to the inner service.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.service
    }
    /// Consume self, returning the inner service.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.service
    }
}
impl<S> MainLoop<S>
where
    S: LspService<Response = JsonValue>,
    ResponseError: From<S::Error>,
{
    /// Create a Language Server main loop.
    #[must_use]
    pub fn new_server(builder: impl FnOnce(ClientSocket) -> S) -> (Self, ClientSocket) {
        let (this, socket) = Self::new(|socket| builder(ClientSocket(socket)));
        (this, ClientSocket(socket))
    }
    /// Create a Language Client main loop.
    #[must_use]
    pub fn new_client(builder: impl FnOnce(ServerSocket) -> S) -> (Self, ServerSocket) {
        let (this, socket) = Self::new(|socket| builder(ServerSocket(socket)));
        (this, ServerSocket(socket))
    }
    fn new(builder: impl FnOnce(PeerSocket) -> S) -> (Self, PeerSocket) {
        let (tx, rx) = mpsc::unbounded();
        let socket = PeerSocket { tx };
        let this = Self {
            service: builder(socket.clone()),
            rx,
            outgoing_id: 0,
            outgoing: HashMap::new(),
            tasks: FuturesUnordered::new(),
        };
        (this, socket)
    }
    /// Drive the service main loop to provide the service.
    ///
    /// Shortcut to [`MainLoop::run`] that accept an `impl AsyncRead` and implicit wrap it in a
    /// [`BufReader`].
    #[allow(clippy::missing_errors_doc)]
    pub async fn run_buffered(
        self,
        input: impl AsyncRead,
        output: impl AsyncWrite,
    ) -> Result<()> {
        self.run(BufReader::new(input), output).await
    }
    /// Drive the service main loop to provide the service.
    ///
    /// # Errors
    ///
    /// - `Error::Io` when the underlying `input` or `output` raises an error.
    /// - `Error::Deserialize` when the peer sends undecodable or invalid message.
    /// - `Error::Protocol` when the peer violates Language Server Protocol.
    /// - Other errors raised from service handlers.
    pub async fn run(
        mut self,
        input: impl AsyncBufRead,
        output: impl AsyncWrite,
    ) -> Result<()> {
        let mut input = input;
        #[allow(unused_mut)]
        let mut input = unsafe {
            ::pin_utils::core_reexport::pin::Pin::new_unchecked(&mut input)
        };
        let mut output = output;
        #[allow(unused_mut)]
        let mut output = unsafe {
            ::pin_utils::core_reexport::pin::Pin::new_unchecked(&mut output)
        };
        let incoming = futures::stream::unfold(
            input,
            |mut input| async move { Some((Message::read(&mut input).await, input)) },
        );
        let outgoing = futures::sink::unfold(
            output,
            |mut output, msg| async move {
                Message::write(&msg, &mut output).await.map(|()| output)
            },
        );
        let mut incoming = incoming;
        #[allow(unused_mut)]
        let mut incoming = unsafe {
            ::pin_utils::core_reexport::pin::Pin::new_unchecked(&mut incoming)
        };
        let mut outgoing = outgoing;
        #[allow(unused_mut)]
        let mut outgoing = unsafe {
            ::pin_utils::core_reexport::pin::Pin::new_unchecked(&mut outgoing)
        };
        let mut flush_fut = futures::future::Fuse::terminated();
        let ret = loop {
            let ctl = {
                use ::futures_util::__private as __futures_crate;
                {
                    enum __PrivResult<_0, _1, _2, _3> {
                        _0(_0),
                        _1(_1),
                        _2(_2),
                        _3(_3),
                    }
                    let __select_result = {
                        __futures_crate::async_await::assert_fused_future(&flush_fut);
                        __futures_crate::async_await::assert_unpin(&flush_fut);
                        let mut _1 = self.tasks.select_next_some();
                        let mut _2 = self.rx.next();
                        let mut _3 = incoming.next();
                        let mut __poll_fn = |
                            __cx: &mut __futures_crate::task::Context<'_>|
                        {
                            let mut __any_polled = false;
                            let mut _0 = |__cx: &mut __futures_crate::task::Context<'_>| {
                                let mut flush_fut = unsafe {
                                    __futures_crate::Pin::new_unchecked(&mut flush_fut)
                                };
                                if __futures_crate::future::FusedFuture::is_terminated(
                                    &flush_fut,
                                ) {
                                    __futures_crate::None
                                } else {
                                    __futures_crate::Some(
                                        __futures_crate::future::FutureExt::poll_unpin(
                                                &mut flush_fut,
                                                __cx,
                                            )
                                            .map(__PrivResult::_0),
                                    )
                                }
                            };
                            let _0: &mut dyn FnMut(
                                &mut __futures_crate::task::Context<'_>,
                            ) -> __futures_crate::Option<
                                    __futures_crate::task::Poll<_>,
                                > = &mut _0;
                            let mut _1 = |__cx: &mut __futures_crate::task::Context<'_>| {
                                let mut _1 = unsafe {
                                    __futures_crate::Pin::new_unchecked(&mut _1)
                                };
                                if __futures_crate::future::FusedFuture::is_terminated(
                                    &_1,
                                ) {
                                    __futures_crate::None
                                } else {
                                    __futures_crate::Some(
                                        __futures_crate::future::FutureExt::poll_unpin(
                                                &mut _1,
                                                __cx,
                                            )
                                            .map(__PrivResult::_1),
                                    )
                                }
                            };
                            let _1: &mut dyn FnMut(
                                &mut __futures_crate::task::Context<'_>,
                            ) -> __futures_crate::Option<
                                    __futures_crate::task::Poll<_>,
                                > = &mut _1;
                            let mut _2 = |__cx: &mut __futures_crate::task::Context<'_>| {
                                let mut _2 = unsafe {
                                    __futures_crate::Pin::new_unchecked(&mut _2)
                                };
                                if __futures_crate::future::FusedFuture::is_terminated(
                                    &_2,
                                ) {
                                    __futures_crate::None
                                } else {
                                    __futures_crate::Some(
                                        __futures_crate::future::FutureExt::poll_unpin(
                                                &mut _2,
                                                __cx,
                                            )
                                            .map(__PrivResult::_2),
                                    )
                                }
                            };
                            let _2: &mut dyn FnMut(
                                &mut __futures_crate::task::Context<'_>,
                            ) -> __futures_crate::Option<
                                    __futures_crate::task::Poll<_>,
                                > = &mut _2;
                            let mut _3 = |__cx: &mut __futures_crate::task::Context<'_>| {
                                let mut _3 = unsafe {
                                    __futures_crate::Pin::new_unchecked(&mut _3)
                                };
                                if __futures_crate::future::FusedFuture::is_terminated(
                                    &_3,
                                ) {
                                    __futures_crate::None
                                } else {
                                    __futures_crate::Some(
                                        __futures_crate::future::FutureExt::poll_unpin(
                                                &mut _3,
                                                __cx,
                                            )
                                            .map(__PrivResult::_3),
                                    )
                                }
                            };
                            let _3: &mut dyn FnMut(
                                &mut __futures_crate::task::Context<'_>,
                            ) -> __futures_crate::Option<
                                    __futures_crate::task::Poll<_>,
                                > = &mut _3;
                            let mut __select_arr = [_0, _1, _2, _3];
                            for poller in &mut __select_arr {
                                let poller: &mut &mut dyn FnMut(
                                    &mut __futures_crate::task::Context<'_>,
                                ) -> __futures_crate::Option<
                                        __futures_crate::task::Poll<_>,
                                    > = poller;
                                match poller(__cx) {
                                    __futures_crate::Some(
                                        x @ __futures_crate::task::Poll::Ready(_),
                                    ) => return x,
                                    __futures_crate::Some(
                                        __futures_crate::task::Poll::Pending,
                                    ) => {
                                        __any_polled = true;
                                    }
                                    __futures_crate::None => {}
                                }
                            }
                            if !__any_polled {
                                {
                                    ::std::rt::begin_panic(
                                        "all futures in select! were completed,\
                    but no `complete =>` handler was provided",
                                    );
                                }
                            } else {
                                __futures_crate::task::Poll::Pending
                            }
                        };
                        __futures_crate::future::poll_fn(__poll_fn).await
                    };
                    match __select_result {
                        __PrivResult::_0(ret) => {
                            ret?;
                            continue;
                        }
                        __PrivResult::_1(resp) => {
                            ControlFlow::Continue(Some(Message::Response(resp)))
                        }
                        __PrivResult::_2(event) => {
                            self.dispatch_event(event.expect("Sender is alive"))
                        }
                        __PrivResult::_3(msg) => {
                            let dispatch_fut = self
                                .dispatch_message(msg.expect("Never ends")?)
                                .fuse();
                            let mut dispatch_fut = dispatch_fut;
                            #[allow(unused_mut)]
                            let mut dispatch_fut = unsafe {
                                ::pin_utils::core_reexport::pin::Pin::new_unchecked(
                                    &mut dispatch_fut,
                                )
                            };
                            loop {
                                {
                                    use ::futures_util::__private as __futures_crate;
                                    {
                                        enum __PrivResult<_0, _1> {
                                            _0(_0),
                                            _1(_1),
                                        }
                                        let __select_result = {
                                            __futures_crate::async_await::assert_fused_future(
                                                &dispatch_fut,
                                            );
                                            __futures_crate::async_await::assert_unpin(&dispatch_fut);
                                            __futures_crate::async_await::assert_fused_future(
                                                &flush_fut,
                                            );
                                            __futures_crate::async_await::assert_unpin(&flush_fut);
                                            let mut __poll_fn = |
                                                __cx: &mut __futures_crate::task::Context<'_>|
                                            {
                                                let mut __any_polled = false;
                                                let mut _0 = |
                                                    __cx: &mut __futures_crate::task::Context<'_>|
                                                {
                                                    let mut dispatch_fut = unsafe {
                                                        __futures_crate::Pin::new_unchecked(&mut dispatch_fut)
                                                    };
                                                    if __futures_crate::future::FusedFuture::is_terminated(
                                                        &dispatch_fut,
                                                    ) {
                                                        __futures_crate::None
                                                    } else {
                                                        __futures_crate::Some(
                                                            __futures_crate::future::FutureExt::poll_unpin(
                                                                    &mut dispatch_fut,
                                                                    __cx,
                                                                )
                                                                .map(__PrivResult::_0),
                                                        )
                                                    }
                                                };
                                                let _0: &mut dyn FnMut(
                                                    &mut __futures_crate::task::Context<'_>,
                                                ) -> __futures_crate::Option<
                                                        __futures_crate::task::Poll<_>,
                                                    > = &mut _0;
                                                let mut _1 = |
                                                    __cx: &mut __futures_crate::task::Context<'_>|
                                                {
                                                    let mut flush_fut = unsafe {
                                                        __futures_crate::Pin::new_unchecked(&mut flush_fut)
                                                    };
                                                    if __futures_crate::future::FusedFuture::is_terminated(
                                                        &flush_fut,
                                                    ) {
                                                        __futures_crate::None
                                                    } else {
                                                        __futures_crate::Some(
                                                            __futures_crate::future::FutureExt::poll_unpin(
                                                                    &mut flush_fut,
                                                                    __cx,
                                                                )
                                                                .map(__PrivResult::_1),
                                                        )
                                                    }
                                                };
                                                let _1: &mut dyn FnMut(
                                                    &mut __futures_crate::task::Context<'_>,
                                                ) -> __futures_crate::Option<
                                                        __futures_crate::task::Poll<_>,
                                                    > = &mut _1;
                                                let mut __select_arr = [_0, _1];
                                                for poller in &mut __select_arr {
                                                    let poller: &mut &mut dyn FnMut(
                                                        &mut __futures_crate::task::Context<'_>,
                                                    ) -> __futures_crate::Option<
                                                            __futures_crate::task::Poll<_>,
                                                        > = poller;
                                                    match poller(__cx) {
                                                        __futures_crate::Some(
                                                            x @ __futures_crate::task::Poll::Ready(_),
                                                        ) => return x,
                                                        __futures_crate::Some(
                                                            __futures_crate::task::Poll::Pending,
                                                        ) => {
                                                            __any_polled = true;
                                                        }
                                                        __futures_crate::None => {}
                                                    }
                                                }
                                                if !__any_polled {
                                                    {
                                                        ::std::rt::begin_panic(
                                                            "all futures in select! were completed,\
                    but no `complete =>` handler was provided",
                                                        );
                                                    }
                                                } else {
                                                    __futures_crate::task::Poll::Pending
                                                }
                                            };
                                            __futures_crate::future::poll_fn(__poll_fn).await
                                        };
                                        match __select_result {
                                            __PrivResult::_0(ctl) => break ctl,
                                            __PrivResult::_1(ret) => {
                                                ret?;
                                                continue;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            };
            let msg = match ctl {
                ControlFlow::Continue(Some(msg)) => msg,
                ControlFlow::Continue(None) => continue,
                ControlFlow::Break(ret) => break ret,
            };
            outgoing.feed(msg).await?;
            flush_fut = outgoing.flush().fuse();
        };
        let flush_ret = outgoing.close().await;
        ret.and(flush_ret)
    }
    async fn dispatch_message(
        &mut self,
        msg: Message,
    ) -> ControlFlow<Result<()>, Option<Message>> {
        match msg {
            Message::Request(req) => {
                if let Err(err) = poll_fn(|cx| self.service.poll_ready(cx)).await {
                    let resp = AnyResponse {
                        id: req.id,
                        result: None,
                        error: Some(err.into()),
                    };
                    return ControlFlow::Continue(Some(Message::Response(resp)));
                }
                let id = req.id.clone();
                let fut = self.service.call(req);
                self.tasks.push(RequestFuture { fut, id: Some(id) });
            }
            Message::Response(resp) => {
                if let Some(resp_tx) = self.outgoing.remove(&resp.id) {
                    let _: Result<_, _> = resp_tx.send(resp);
                }
            }
            Message::Notification(notif) => {
                self.service.notify(notif)?;
            }
        }
        ControlFlow::Continue(None)
    }
    fn dispatch_event(
        &mut self,
        event: MainLoopEvent,
    ) -> ControlFlow<Result<()>, Option<Message>> {
        match event {
            MainLoopEvent::OutgoingRequest(mut req, resp_tx) => {
                req.id = RequestId::Number(self.outgoing_id);
                if !self.outgoing.insert(req.id.clone(), resp_tx).is_none() {
                    ::core::panicking::panic(
                        "assertion failed: self.outgoing.insert(req.id.clone(), resp_tx).is_none()",
                    )
                }
                self.outgoing_id += 1;
                ControlFlow::Continue(Some(Message::Request(req)))
            }
            MainLoopEvent::Outgoing(msg) => ControlFlow::Continue(Some(msg)),
            MainLoopEvent::Any(event) => {
                self.service.emit(event)?;
                ControlFlow::Continue(None)
            }
        }
    }
}
struct RequestFuture<Fut> {
    fut: Fut,
    id: Option<RequestId>,
}
#[allow(explicit_outlives_requirements)]
#[allow(single_use_lifetimes)]
#[allow(clippy::unknown_clippy_lints)]
#[allow(clippy::redundant_pub_crate)]
#[allow(clippy::used_underscore_binding)]
const _: () = {
    #[doc(hidden)]
    #[allow(dead_code)]
    #[allow(single_use_lifetimes)]
    #[allow(clippy::unknown_clippy_lints)]
    #[allow(clippy::mut_mut)]
    #[allow(clippy::redundant_pub_crate)]
    #[allow(clippy::ref_option_ref)]
    #[allow(clippy::type_repetition_in_bounds)]
    struct Projection<'__pin, Fut>
    where
        RequestFuture<Fut>: '__pin,
    {
        fut: ::pin_project_lite::__private::Pin<&'__pin mut (Fut)>,
        id: &'__pin mut (Option<RequestId>),
    }
    #[doc(hidden)]
    #[allow(dead_code)]
    #[allow(single_use_lifetimes)]
    #[allow(clippy::unknown_clippy_lints)]
    #[allow(clippy::mut_mut)]
    #[allow(clippy::redundant_pub_crate)]
    #[allow(clippy::ref_option_ref)]
    #[allow(clippy::type_repetition_in_bounds)]
    struct ProjectionRef<'__pin, Fut>
    where
        RequestFuture<Fut>: '__pin,
    {
        fut: ::pin_project_lite::__private::Pin<&'__pin (Fut)>,
        id: &'__pin (Option<RequestId>),
    }
    impl<Fut> RequestFuture<Fut> {
        #[doc(hidden)]
        #[inline]
        fn project<'__pin>(
            self: ::pin_project_lite::__private::Pin<&'__pin mut Self>,
        ) -> Projection<'__pin, Fut> {
            unsafe {
                let Self { fut, id } = self.get_unchecked_mut();
                Projection {
                    fut: ::pin_project_lite::__private::Pin::new_unchecked(fut),
                    id: id,
                }
            }
        }
        #[doc(hidden)]
        #[inline]
        fn project_ref<'__pin>(
            self: ::pin_project_lite::__private::Pin<&'__pin Self>,
        ) -> ProjectionRef<'__pin, Fut> {
            unsafe {
                let Self { fut, id } = self.get_ref();
                ProjectionRef {
                    fut: ::pin_project_lite::__private::Pin::new_unchecked(fut),
                    id: id,
                }
            }
        }
    }
    #[allow(non_snake_case)]
    struct __Origin<'__pin, Fut> {
        __dummy_lifetime: ::pin_project_lite::__private::PhantomData<&'__pin ()>,
        fut: Fut,
        id: ::pin_project_lite::__private::AlwaysUnpin<Option<RequestId>>,
    }
    impl<'__pin, Fut> ::pin_project_lite::__private::Unpin for RequestFuture<Fut>
    where
        __Origin<'__pin, Fut>: ::pin_project_lite::__private::Unpin,
    {}
    trait MustNotImplDrop {}
    #[allow(clippy::drop_bounds, drop_bounds)]
    impl<T: ::pin_project_lite::__private::Drop> MustNotImplDrop for T {}
    impl<Fut> MustNotImplDrop for RequestFuture<Fut> {}
    #[forbid(unaligned_references, safe_packed_borrows)]
    fn __assert_not_repr_packed<Fut>(this: &RequestFuture<Fut>) {
        let _ = &this.fut;
        let _ = &this.id;
    }
};
impl<Fut, Error> Future for RequestFuture<Fut>
where
    Fut: Future<Output = Result<JsonValue, Error>>,
    ResponseError: From<Error>,
{
    type Output = AnyResponse;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let (mut result, mut error) = (None, None);
        match match this.fut.poll(cx) {
            ::core::task::Poll::Ready(t) => t,
            ::core::task::Poll::Pending => {
                return ::core::task::Poll::Pending;
            }
        } {
            Ok(v) => result = Some(v),
            Err(err) => error = Some(err.into()),
        }
        Poll::Ready(AnyResponse {
            id: this.id.take().expect("Future is consumed"),
            result,
            error,
        })
    }
}
/// The socket for Language Server to communicate with the Language Client peer.
pub struct ClientSocket(PeerSocket);
#[automatically_derived]
impl ::core::fmt::Debug for ClientSocket {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_tuple_field1_finish(f, "ClientSocket", &&self.0)
    }
}
#[automatically_derived]
impl ::core::clone::Clone for ClientSocket {
    #[inline]
    fn clone(&self) -> ClientSocket {
        ClientSocket(::core::clone::Clone::clone(&self.0))
    }
}
impl ClientSocket {
    /// Create a closed socket outside a main loop. Any interaction will immediately return
    /// an error of [`Error::ServiceStopped`].
    ///
    /// This works as a placeholder where a socket is required but actually unused.
    ///
    /// # Note
    ///
    /// To prevent accidental misusages, this method is NOT implemented as
    /// [`Default::default`] intentionally.
    #[must_use]
    pub fn new_closed() -> Self {
        Self(PeerSocket::new_closed())
    }
    /// Send a request to the peer and wait for its response.
    ///
    /// # Errors
    /// - [`Error::ServiceStopped`] when the service main loop stopped.
    /// - [`Error::Response`] when the peer replies an error.
    pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result> {
        self.0.request::<R>(params).await
    }
    /// Send a notification to the peer and wait for its response.
    ///
    /// This is done asynchronously. An `Ok` result indicates the message is successfully
    /// queued, but may not be sent to the peer yet.
    ///
    /// # Errors
    /// - [`Error::ServiceStopped`] when the service main loop stopped.
    pub fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        self.0.notify::<N>(params)
    }
    /// Emit an arbitrary loopback event object to the service handler.
    ///
    /// This is done asynchronously. An `Ok` result indicates the message is successfully
    /// queued, but may not be processed yet.
    ///
    /// # Errors
    /// - [`Error::ServiceStopped`] when the service main loop stopped.
    pub fn emit<E: Send + 'static>(&self, event: E) -> Result<()> {
        self.0.emit::<E>(event)
    }
}
/// The socket for Language Client to communicate with the Language Server peer.
pub struct ServerSocket(PeerSocket);
#[automatically_derived]
impl ::core::fmt::Debug for ServerSocket {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_tuple_field1_finish(f, "ServerSocket", &&self.0)
    }
}
#[automatically_derived]
impl ::core::clone::Clone for ServerSocket {
    #[inline]
    fn clone(&self) -> ServerSocket {
        ServerSocket(::core::clone::Clone::clone(&self.0))
    }
}
impl ServerSocket {
    /// Create a closed socket outside a main loop. Any interaction will immediately return
    /// an error of [`Error::ServiceStopped`].
    ///
    /// This works as a placeholder where a socket is required but actually unused.
    ///
    /// # Note
    ///
    /// To prevent accidental misusages, this method is NOT implemented as
    /// [`Default::default`] intentionally.
    #[must_use]
    pub fn new_closed() -> Self {
        Self(PeerSocket::new_closed())
    }
    /// Send a request to the peer and wait for its response.
    ///
    /// # Errors
    /// - [`Error::ServiceStopped`] when the service main loop stopped.
    /// - [`Error::Response`] when the peer replies an error.
    pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result> {
        self.0.request::<R>(params).await
    }
    /// Send a notification to the peer and wait for its response.
    ///
    /// This is done asynchronously. An `Ok` result indicates the message is successfully
    /// queued, but may not be sent to the peer yet.
    ///
    /// # Errors
    /// - [`Error::ServiceStopped`] when the service main loop stopped.
    pub fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        self.0.notify::<N>(params)
    }
    /// Emit an arbitrary loopback event object to the service handler.
    ///
    /// This is done asynchronously. An `Ok` result indicates the message is successfully
    /// queued, but may not be processed yet.
    ///
    /// # Errors
    /// - [`Error::ServiceStopped`] when the service main loop stopped.
    pub fn emit<E: Send + 'static>(&self, event: E) -> Result<()> {
        self.0.emit::<E>(event)
    }
}
struct PeerSocket {
    tx: mpsc::UnboundedSender<MainLoopEvent>,
}
#[automatically_derived]
impl ::core::fmt::Debug for PeerSocket {
    #[inline]
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        ::core::fmt::Formatter::debug_struct_field1_finish(
            f,
            "PeerSocket",
            "tx",
            &&self.tx,
        )
    }
}
#[automatically_derived]
impl ::core::clone::Clone for PeerSocket {
    #[inline]
    fn clone(&self) -> PeerSocket {
        PeerSocket {
            tx: ::core::clone::Clone::clone(&self.tx),
        }
    }
}
impl PeerSocket {
    fn new_closed() -> Self {
        let (tx, _rx) = mpsc::unbounded();
        Self { tx }
    }
    fn send(&self, v: MainLoopEvent) -> Result<()> {
        self.tx.unbounded_send(v).map_err(|_| Error::ServiceStopped)
    }
    fn request<R: Request>(
        &self,
        params: R::Params,
    ) -> PeerSocketRequestFuture<R::Result> {
        let req = AnyRequest {
            id: RequestId::Number(0),
            method: R::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        let (tx, rx) = oneshot::channel();
        let _: Result<_, _> = self.send(MainLoopEvent::OutgoingRequest(req, tx));
        PeerSocketRequestFuture {
            rx,
            _marker: PhantomData,
        }
    }
    fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        let notif = AnyNotification {
            method: N::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        self.send(MainLoopEvent::Outgoing(Message::Notification(notif)))
    }
    pub fn emit<E: Send + 'static>(&self, event: E) -> Result<()> {
        self.send(MainLoopEvent::Any(AnyEvent::new(event)))
    }
}
struct PeerSocketRequestFuture<T> {
    rx: oneshot::Receiver<AnyResponse>,
    _marker: PhantomData<fn() -> T>,
}
impl<T: DeserializeOwned> Future for PeerSocketRequestFuture<T> {
    type Output = Result<T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let resp = match Pin::new(&mut self.rx)
            .poll(cx)
            .map_err(|_| Error::ServiceStopped)
        {
            ::core::task::Poll::Ready(t) => t,
            ::core::task::Poll::Pending => {
                return ::core::task::Poll::Pending;
            }
        }?;
        Poll::Ready(
            match resp.error {
                None => Ok(serde_json::from_value(resp.result.unwrap_or_default())?),
                Some(err) => Err(Error::Response(err)),
            },
        )
    }
}
/// A dynamic runtime event.
///
/// This is a wrapper of `Box<dyn Any + Send>`, but saves the underlying type name for better
/// `Debug` impl.
pub struct AnyEvent {
    inner: Box<dyn Any + Send>,
    type_name: &'static str,
}
impl fmt::Debug for AnyEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyEvent")
            .field("type_name", &self.type_name)
            .finish_non_exhaustive()
    }
}
impl AnyEvent {
    #[must_use]
    fn new<T: Send + 'static>(v: T) -> Self {
        AnyEvent {
            inner: Box::new(v),
            type_name: type_name::<T>(),
        }
    }
    #[must_use]
    fn inner_type_id(&self) -> TypeId {
        Any::type_id(&*self.inner)
    }
    /// Get the underlying type name for debugging purpose.
    ///
    /// The result string is only meant for debugging. It is not stable and cannot be trusted.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }
    /// Returns `true` if the inner type is the same as `T`.
    #[must_use]
    pub fn is<T: Send + 'static>(&self) -> bool {
        self.inner.is::<T>()
    }
    /// Returns some reference to the inner value if it is of type `T`, or `None` if it isn't.
    #[must_use]
    pub fn downcast_ref<T: Send + 'static>(&self) -> Option<&T> {
        self.inner.downcast_ref::<T>()
    }
    /// Returns some mutable reference to the inner value if it is of type `T`, or `None` if it
    /// isn't.
    #[must_use]
    pub fn downcast_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.inner.downcast_mut::<T>()
    }
    /// Attempt to downcast it to a concrete type.
    ///
    /// # Errors
    ///
    /// Returns `self` if the type mismatches.
    pub fn downcast<T: Send + 'static>(self) -> Result<T, Self> {
        match self.inner.downcast::<T>() {
            Ok(v) => Ok(*v),
            Err(inner) => {
                Err(Self {
                    inner,
                    type_name: self.type_name,
                })
            }
        }
    }
}
