use futures_util::StreamExt;
use rmcp::service::{RxJsonRpcMessage, ServiceRole, TxJsonRpcMessage};
use rmcp::transport::{Transport, async_rw::JsonRpcMessageCodec};
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::codec::FramedRead;
use tokio_util::sync::CancellationToken;

pub(crate) struct RpcTransport<R, W, Role>
where
    Role: ServiceRole,
{
    reader: FramedRead<R, JsonRpcMessageCodec<RxJsonRpcMessage<Role>>>,
    writer: Arc<Mutex<Option<W>>>,
    output_budget: Arc<Semaphore>,
    failure_token: Option<CancellationToken>,
    max_message_bytes: usize,
    write_stall: Duration,
    role: PhantomData<fn() -> Role>,
}

impl<R, W, Role> RpcTransport<R, W, Role>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
    Role: ServiceRole + 'static,
{
    pub(crate) fn new(
        reader: R,
        writer: W,
        output_budget: Arc<Semaphore>,
        failure_token: Option<CancellationToken>,
        max_message_bytes: usize,
        write_stall: Duration,
    ) -> Self {
        Self {
            reader: FramedRead::new(
                reader,
                JsonRpcMessageCodec::new_with_max_length(max_message_bytes),
            ),
            writer: Arc::new(Mutex::new(Some(writer))),
            output_budget,
            failure_token,
            max_message_bytes,
            write_stall,
            role: PhantomData,
        }
    }
}

impl<R, W, Role> Transport<Role> for RpcTransport<R, W, Role>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
    Role: ServiceRole + 'static,
{
    type Error = io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<Role>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = Arc::clone(&self.writer);
        let output_budget = Arc::clone(&self.output_budget);
        let failure_token = self.failure_token.clone();
        let max_message_bytes = self.max_message_bytes;
        let write_stall = self.write_stall;
        async move {
            let reserved_bytes = max_message_bytes.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "MCP message limit is too large",
                )
            })?;
            let permits = u32::try_from(reserved_bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "MCP message limit is too large",
                )
            })?;
            let mut permit = output_budget
                .acquire_many_owned(permits)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "gateway output closed"))?;

            let mut bytes = match serde_json::to_vec(&message) {
                Ok(bytes) => bytes,
                Err(error) => {
                    if let Some(token) = &failure_token {
                        token.cancel();
                    }
                    return Err(io::Error::other(error));
                }
            };
            if bytes.len() > max_message_bytes {
                if let Some(token) = &failure_token {
                    token.cancel();
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MCP message exceeds the configured byte limit",
                ));
            }
            let actual_bytes = bytes.len() + 1;
            if let Some(unused) = permit.split(reserved_bytes - actual_bytes) {
                drop(unused);
            }
            bytes.push(b'\n');

            let mut writer = writer.lock().await;
            let writer = writer.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "gateway output closed")
            })?;
            match tokio::time::timeout(write_stall, async {
                writer.write_all(&bytes).await?;
                writer.flush().await
            })
            .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => {
                    if let Some(token) = &failure_token {
                        token.cancel();
                    }
                    Err(error)
                }
                Err(_) => {
                    if let Some(token) = &failure_token {
                        token.cancel();
                    }
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "MCP output stalled",
                    ))
                }
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<Role>> {
        match self.reader.next().await {
            Some(Ok(message)) => Some(message),
            Some(Err(_)) | None => None,
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.writer.lock().await.take();
        Ok(())
    }
}
