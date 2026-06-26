use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use futures::channel::oneshot;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};
use listen_fds::ListenFds;
use memfd::{Memfd, MemfdOptions};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use zlink::service::MethodReply;
use zlink::unix::Stream;
use zlink::{Call, Connection, Reply, ReplyError, Service};

struct LongRequest {}

impl LongRequest {
    fn new() -> Self {
        Self {}
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method", content = "parameters")]
enum LongRequestMethod {
    #[serde(rename = "org.freedesktop.portal2.LongRequest.Cancel")]
    Cancel,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum LongRequestReply {
    Cancel {},
}

#[derive(Debug, Clone, PartialEq, ReplyError, zlink::introspect::ReplyError)]
#[zlink(interface = "org.freedesktop.portal2.LongRequest")]
pub enum LongRequestError {
    Other,
}

impl Service<zlink::unix::Stream> for LongRequest {
    type MethodCall<'de> = LongRequestMethod;
    type ReplyParams<'ser> = LongRequestReply;
    type ReplyStreamParams = ();
    type ReplyStreamError = ();
    type ReplyStream = futures::stream::Empty<(Result<zlink::Reply<()>, ()>, Vec<OwnedFd>)>;
    type ReplyError<'ser> = LongRequestError;

    async fn handle<'service>(
        &'service mut self,
        call: &'service Call<Self::MethodCall<'_>>,
        conn: &mut Connection<zlink::unix::Stream>,
        fds: Vec<OwnedFd>,
    ) -> (
        MethodReply<Self::ReplyParams<'service>, Self::ReplyStream, Self::ReplyError<'service>>,
        Vec<OwnedFd>,
    ) {
        match call.method() {
            LongRequestMethod::Cancel => {
                println!("LongRequest: Received call to Cancel()");
                (
                    MethodReply::Single(Some(LongRequestReply::Cancel {})),
                    Vec::new(),
                )
            }
        }
    }
}

async fn spawn_long_request_server(
    tokio_stream: tokio::net::UnixStream,
    mut cancel_tx: futures::channel::oneshot::Sender<()>,
) -> Result<(), anyhow::Error> {
    let zlink_stream: zlink::unix::Stream = tokio_stream.into();

    let mut connection: Connection<Stream> = Connection::new(zlink_stream);

    let mut service = LongRequest::new();

    println!("LongRequest: Listening on side-channel now");

    // No loop, this server is supposed to handle just one method call, and shutdown after
    let (call, fds) = connection.receive_call().await?;
    let (r, reply_fds) = service.handle(&call, &mut connection, fds).await;
    match r {
        MethodReply::Single(r) => {
            connection.send_reply(&Reply::new(r), reply_fds).await?;

            // Meh, we assume that any reply we're sending is a reply to Cancel(), and use it to
            // inform our sender that Cancel() was called. Would be nicer to actually do this in the call
            // handler.
            let _ = cancel_tx.send(());
        }
        MethodReply::Error(e) => connection.send_error(&e, reply_fds).await?,
        MethodReply::Multi(_) => unreachable!(),
    };

    println!("LongRequest: Shutting down server");

    Ok(())
}

pub fn long_request_server_on_thread() -> Result<(OwnedFd, oneshot::Receiver<()>), anyhow::Error> {
    let (sock1, sock2) = std::os::unix::net::UnixStream::pair()?;
    sock1.set_nonblocking(true)?;
    let tokio_stream: tokio::net::UnixStream = tokio::net::UnixStream::from_std(sock1)?;
    let paired_fd: OwnedFd = sock2.into();

    let (mut cancel_tx, cancel_rx) = oneshot::channel::<()>();

    tokio::spawn(async {
        spawn_long_request_server(tokio_stream, cancel_tx)
            .await
            .inspect_err(|e| {
                if matches!(
                    e.downcast_ref::<zlink::Error>(),
                    Some(zlink::Error::UnexpectedEof)
                ) {
                    // Client disconnected, probably the long running request succeeded
                } else {
                    println!("LongRequest: Thread errored out: {:?}", e);
                }
            });
    });

    Ok((paired_fd, cancel_rx))
}
