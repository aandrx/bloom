//! Local Unix-socket IPC foundation. Request framing and dispatch belong to B3b.

mod dispatch;
mod listener;

pub use dispatch::{serve, RequestHandler, REQUEST_IDLE_TIMEOUT};

pub use listener::{
    AuthenticatedStream, Listener, ListenerError, MAX_ACCEPTED, MAX_HANDSHAKE_IDLE,
};
