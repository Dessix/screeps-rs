use std::{fmt, thread};

use std::time::Duration;

use std::sync::mpsc as std_mpsc;
use std::sync::mpsc::Sender as StdSender;
use std::sync::mpsc::Receiver as StdReceiver;

use futures::sync::mpsc as futures_mpsc;
use futures::sync::mpsc::UnboundedSender as FuturesSender;
use futures::sync::mpsc::UnboundedReceiver as FuturesReceiver;
use futures::sync::mpsc::Sender as BoundedFuturesSender;

use futures::{future, Future, Stream};
use tokio_core::reactor::{Handle, Remote, Core, Timeout};
use hyper::status::StatusCode;

use screeps_api::{self, TokenStorage, ArcTokenStorage};

use {glutin, hyper, hyper_tls, tokio_core};

use super::{LoginDetails, Request, NetworkEvent, ScreepsConnection, NotLoggedIn};

pub struct Handler {
    /// Receiver and sender interacting with the current threaded handler.
    ///
    /// Use std sync channel for (tokio -> main thread), and a futures channel for (main thread -> tokio):
    /// - neither have any specific requirements for where the sender is called, but both require that the
    ///   polling receiver be in the 'right context'. This way, it just works.
    handles: Option<(Remote, FuturesSender<Request>, StdReceiver<NetworkEvent>)>,
    /// Tokens saved.
    tokens: ArcTokenStorage,
    /// Username and password in case we need to re-login.
    login_info: Option<LoginDetails>,
    /// Window proxy in case we need to restart handler thread.
    window: glutin::WindowProxy,
}

impl Handler {
    /// Creates a new requests state, and starts an initial handler with a pending login request.
    pub fn new(window: glutin::WindowProxy) -> Self {
        Handler {
            handles: None,
            login_info: None,
            tokens: ArcTokenStorage::default(),
            window: window,
        }
    }

    fn start_handler(&mut self) -> Result<(), NotLoggedIn> {
        let login_details = match self.login_info {
            Some(ref tuple) => tuple.clone(),
            None => return Err(NotLoggedIn),
        };

        let mut queued: Option<Vec<NetworkEvent>> = None;
        if let Some((_, _send, recv)) = self.handles.take() {
            let mut queued_vec = Vec::new();
            while let Ok(v) = recv.try_recv() {
                queued_vec.push(v);
            }
            queued = Some(queued_vec);
        }

        let (send_to_handler, handler_recv) = futures_mpsc::unbounded();
        let (handler_send, recv_from_handler) = std_mpsc::channel();

        if let Some(values) = queued {
            for v in values {
                // fake these coming from the new handler.
                handler_send.send(v).expect("expected handles to still be in current scope");
            }
        }

        let handler = ThreadedHandler::new(handler_recv,
                                           handler_send,
                                           self.window.clone(),
                                           self.tokens.clone(),
                                           login_details.clone());

        let remote = handler.start_async_and_get_remote();

        self.handles = Some((remote, send_to_handler, recv_from_handler));

        Ok(())
    }
}

impl ScreepsConnection for Handler {
    fn send(&mut self, request: Request) -> Result<(), NotLoggedIn> {
        // TODO: find out how to get panic info from the threaded thread, and report that we had to reconnect!
        let request_retry = match self.handles {
            Some((_, ref mut send, _)) => {
                match send.send(request) {
                    Ok(()) => None,
                    Err(send_err) => Some(send_err.into_inner()),
                }
            }
            None => Some(request),
        };

        if let Some(request) = request_retry {
            match request {
                Request::Login { details } => {
                    self.login_info = Some(details);
                    self.start_handler()?;
                }
                request => {
                    self.start_handler()?;
                    let send = &self.handles.as_ref().expect("expected handles to exist after freshly restarting").1;
                    send.send(request).expect("expected freshly started handler to still be running");
                }
            }
        }

        Ok(())
    }

    fn poll(&mut self) -> Option<NetworkEvent> {
        let (evt, reset) = match self.handles {
            Some((_, _, ref mut recv)) => {
                match recv.try_recv() {
                    Ok(v) => (Some(v), false),
                    Err(std_mpsc::TryRecvError::Empty) => (None, false),
                    Err(std_mpsc::TryRecvError::Disconnected) => (None, true),
                }
            }
            None => (None, false),
        };
        if reset {
            self.handles = None;
        }
        evt
    }
}
impl fmt::Debug for Handler {
    fn fmt(&self, fmt: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
        fmt.debug_struct("Handler")
            .field("handles", &self.handles)
            .field("login_info", &self.login_info)
            .field("tokens", &self.tokens)
            .field("window", &"<non-debug>")
            .finish()
    }
}


struct ThreadedHandler {
    recv: FuturesReceiver<Request>,
    send: StdSender<NetworkEvent>,
    window: glutin::WindowProxy,
    login: LoginDetails,
    tokens: ArcTokenStorage,
}

struct TokioExecutor<C, H, T> {
    handle: Handle,
    send_results: StdSender<NetworkEvent>,
    notify: glutin::WindowProxy,
    executor_return: BoundedFuturesSender<TokioExecutor<C, H, T>>,
    login: LoginDetails,
    client: screeps_api::Api<C, H, T>,
}

impl<C, H, T> TokioExecutor<C, H, T>
    where C: hyper::client::Connect,
          H: screeps_api::HyperClient<C> + 'static + Clone,
          T: TokenStorage
{
    fn execute_request(self, request: Request) -> impl Future<Item = (), Error = ()> + 'static {
        use futures::Sink;

        request.exec_with(&self.login, &self.client)
            .then(move |event_result| -> Box<Future<Item = (), Error = ()> + 'static> {
                // this should never return an error - in any case though, we should handle the Err case so we
                // do return the executor.
                if let Ok(event) = event_result {
                    if let Some(err) = event.error() {
                        if let screeps_api::ErrorKind::StatusCode(ref status) = *err.kind() {
                            if *status == StatusCode::TooManyRequests {
                                debug!("starting 5-second timeout from TooManyRequests error.");
                                match Timeout::new(Duration::from_secs(5), &self.handle) {
                                    Ok(timeout) => {
                                        return Box::new(timeout.then(|result| {
                                            if let Err(e) = result {
                                                warn!("IO error in 5-second timeout! {}", e);
                                            }

                                            debug!("5-second timeout finished.");

                                            self.execute_request(request)
                                        })) as
                                               Box<Future<Item = (), Error = ()>>
                                    }
                                    Err(e) => {
                                        warn!("IO error in attempt to start timeout: {}", e);
                                        warn!("instead of timing out, just letting 429 error fall through instead.");
                                    }
                                }
                            }
                        }
                    }

                    match self.send_results.send(event) {
                        Ok(_) => {
                            trace!("successfully finished a request.");
                            self.notify.wakeup_event_loop();
                        }
                        Err(_) => {
                            warn!("failed to send the result of a request.");
                        }
                    }
                } else {
                    warn!("unexpected () error from calling Request::exec_with!");
                }

                Box::new(self.executor_return.clone().send(self).then(|result| {
                    if let Err(_) = result {
                        warn!("couldn't return connection token after finishing a request.")
                    };
                    future::ok(())
                })) as Box<Future<Item = (), Error = ()>>
            })
    }
}
impl ThreadedHandler {
    fn new(recv: FuturesReceiver<Request>,
           send: StdSender<NetworkEvent>,
           awaken: glutin::WindowProxy,
           tokens: ArcTokenStorage,
           login: LoginDetails)
           -> Self {
        ThreadedHandler {
            recv: recv,
            send: send,
            window: awaken,
            login: login,
            tokens: tokens,
        }
    }

    fn start_async_and_get_remote(self) -> tokio_core::reactor::Remote {
        let (temp_sender, temp_receiver) = std_mpsc::channel();
        thread::spawn(|| self.run(temp_sender));
        temp_receiver.recv().expect("expected newly created channel to not be dropped, perhaps tokio core panicked?")
    }

    fn run(self, send_remote_to: StdSender<tokio_core::reactor::Remote>) {
        use futures::Sink;

        let ThreadedHandler { recv, send, window, login, tokens } = self;

        let mut core = Core::new().expect("expected tokio core to succeed startup.");

        {
            // move into scope to drop.
            let sender = send_remote_to;
            sender.send(core.remote()).expect("expected sending remote to spawning thread to succeed.");
        }

        let handle = core.handle();

        let hyper = hyper::Client::configure()
            .connector(hyper_tls::HttpsConnector::new(4, &handle))
            .build(&handle);

        let client = screeps_api::Api::with_tokens(hyper, tokens);

        // token pool so we only have at max 5 connections open at a time.
        let (mut token_pool_send, token_pool_recv) = futures_mpsc::channel(5);

        // fill with 5 tokens.
        for _ in 0..5 {
            let cloned_send = token_pool_send.clone();
            assert!(token_pool_send.start_send(TokioExecutor {
                    handle: handle.clone(),
                    send_results: send.clone(),
                    notify: window.clone(),
                    executor_return: cloned_send,
                    login: login.clone(),
                    client: client.clone(),
                })
                .expect("expected newly created channel to still be in scope")
                .is_ready());
        }

        // zip ensures that we have one token for each request! this way we'll
        // never have more than 5 concurrent requests.
        let result = core.run(recv.zip(token_pool_recv).and_then(|(request, executor)| {
            // execute request returns the executor to the token poll at the end.
            handle.spawn(executor.execute_request(request));

            future::ok(())
        }).fold((), |(), _| future::ok(())));

        if let Err(()) = result {
            warn!("Unexpected error when running network core.");
        }

        info!("single threaded event loop exiting.");
        // let the client know that we have closed.
        window.wakeup_event_loop();
    }
}
