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
        
        //////////

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

        //////////

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
        
        /////////

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

        //////

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

            //////

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