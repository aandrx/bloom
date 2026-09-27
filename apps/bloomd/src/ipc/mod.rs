//! Local Unix-socket IPC foundation. Request framing and dispatch belong to B3b.

mod dispatch;
mod listener;
mod subscription;

pub use dispatch::{
    serve, serve_with_subscriptions, HandlerFuture, RequestHandler, REQUEST_IDLE_TIMEOUT,
};

pub use subscription::{serve_subscriber, SubscriptionHub, SUBSCRIPTION_WRITE_TIMEOUT};

pub use listener::{
    AuthenticatedStream, Listener, ListenerError, MAX_ACCEPTED, MAX_HANDSHAKE_IDLE,
};
