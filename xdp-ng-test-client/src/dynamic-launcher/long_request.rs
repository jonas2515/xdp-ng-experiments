use std::os::fd::OwnedFd;

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use serde::{Deserialize, Serialize};
use zlink::{ReplyError, proxy};

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LongRequestReply {
    Cancel {},
}

#[derive(Debug, Clone, PartialEq, ReplyError, zlink::introspect::ReplyError)]
#[zlink(interface = "org.freedesktop.portal2.LongRequest")]
pub enum LongRequestError {
    Other,
}

#[proxy("org.freedesktop.portal2.LongRequest")]
pub trait LongRequest {
    async fn cancel(&mut self) -> zlink::Result<Result<LongRequestReply, LongRequestError>>;
}

pub async fn connect_to_sidechannel_socket(
    fd: OwnedFd,
    rx: oneshot::Receiver<()>,
) -> Result<(), anyhow::Error> {
    let unix_stream: std::os::unix::net::UnixStream = fd.into();
    unix_stream.set_nonblocking(true)?;
    let tokio_stream = tokio::net::UnixStream::from_std(unix_stream)?;
    let zlink_stream: zlink::unix::Stream = tokio_stream.into();
    let mut connection = zlink::Connection::new(zlink_stream);

    let _ = rx.await?;

    println!("Calling Ćancel() on LongRequest side-channel");

    let result = connection
        .cancel()
        .await
        .map_err(|e| anyhow!("Failed to call Cancel(): {:?}", e))?;
    let reply = result.map_err(|e| anyhow!("Cancel() returned error: {:?}", e))?;

    println!(
        "Called Cancel() on LongRequest side-channel, response: {:?}",
        reply
    );

    Ok(())
}

pub async fn connect_to_sidechannel_socket_on_thread(
    fd: OwnedFd,
) -> Result<oneshot::Sender<()>, anyhow::Error> {
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        connect_to_sidechannel_socket(fd, rx)
            .await
            .inspect_err(|e| {
                if e.downcast_ref::<oneshot::Canceled>().is_some() {
                    // Sender got dropped without a message
                } else {
                    eprintln!("Error on LongRequest thread: {:?}", e)
                }
            })
    });

    Ok(tx)
}
