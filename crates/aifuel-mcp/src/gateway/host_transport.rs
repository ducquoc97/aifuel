use rmcp::RoleServer;
use rmcp::model::{
    ClientJsonRpcMessage, ClientRequest, ErrorCode, ErrorData, ServerJsonRpcMessage,
};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::{Transport, async_rw::JsonRpcMessageCodec};
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::Decoder;
use tokio_util::sync::CancellationToken;

struct HostOutput {
    bytes: Vec<u8>,
    finished: oneshot::Sender<bool>,
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct HostTransport {
    reader: mpsc::Receiver<io::Result<RxJsonRpcMessage<RoleServer>>>,
    initial_message: Option<RxJsonRpcMessage<RoleServer>>,
    writer: Arc<Mutex<Option<mpsc::Sender<HostOutput>>>>,
    output_budget: Arc<Semaphore>,
    failure_token: CancellationToken,
    max_message_bytes: usize,
    write_stall: Duration,
}

impl HostTransport {
    pub(crate) fn new(
        max_message_bytes: usize,
        output_budget: Arc<Semaphore>,
        write_stall: Duration,
        failure_token: CancellationToken,
    ) -> io::Result<Self> {
        let (reader_tx, reader) = mpsc::channel(64);
        thread::Builder::new()
            .name("aifuel-mcp-stdin".to_owned())
            .spawn(move || read_host_input(reader_tx, max_message_bytes))?;

        let (writer, mut output) = mpsc::channel::<HostOutput>(64);
        thread::Builder::new()
            .name("aifuel-mcp-stdout".to_owned())
            .spawn(move || {
                let stdout = io::stdout();
                let mut stdout = stdout.lock();
                while let Some(message) = output.blocking_recv() {
                    let success = stdout
                        .write_all(&message.bytes)
                        .and_then(|()| stdout.flush())
                        .is_ok();
                    let _ = message.finished.send(success);
                    if !success {
                        break;
                    }
                }
            })?;

        Ok(Self {
            reader,
            initial_message: None,
            writer: Arc::new(Mutex::new(Some(writer))),
            output_budget,
            failure_token,
            max_message_bytes,
            write_stall,
        })
    }

    /// Answer the optional modern discovery probe used by newer MCP hosts,
    /// then leave the following legacy initialize request for rmcp.
    pub(crate) async fn prepare_legacy_host(&mut self) -> io::Result<()> {
        loop {
            let message =
                self.reader.recv().await.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "MCP host closed")
                })??;
            let discovery_id = match &message {
                ClientJsonRpcMessage::Request(request)
                    if matches!(
                        &request.request,
                        ClientRequest::CustomRequest(custom)
                            if custom.method == "server/discover"
                    ) =>
                {
                    Some(request.id.clone())
                }
                _ => None,
            };
            let Some(discovery_id) = discovery_id else {
                self.initial_message = Some(message);
                return Ok(());
            };

            self.send(discovery_error(discovery_id)).await?;
        }
    }
}

impl Transport<RoleServer> for HostTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleServer>,
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
                    failure_token.cancel();
                    return Err(io::Error::other(error));
                }
            };
            if bytes.len() > max_message_bytes {
                failure_token.cancel();
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
            let (finished, completion) = oneshot::channel();
            let output = HostOutput {
                bytes,
                finished,
                _permit: permit,
            };

            let writer = writer.lock().await;
            let Some(writer) = writer.as_ref() else {
                failure_token.cancel();
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "gateway output closed",
                ));
            };
            match tokio::time::timeout(write_stall, async {
                writer.send(output).await.map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "gateway output closed")
                })?;
                match completion.await {
                    Ok(true) => Ok(()),
                    Ok(false) | Err(_) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "gateway output failed",
                    )),
                }
            })
            .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => {
                    failure_token.cancel();
                    Err(error)
                }
                Err(_) => {
                    failure_token.cancel();
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "MCP output stalled",
                    ))
                }
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        if self.initial_message.is_some() {
            return self.initial_message.take();
        }
        self.reader.recv().await?.ok()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.reader.close();
        self.writer.lock().await.take();
        Ok(())
    }
}

fn discovery_error(id: rmcp::model::RequestId) -> TxJsonRpcMessage<RoleServer> {
    ServerJsonRpcMessage::error(
        ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Method not found", None),
        id,
    )
}

fn read_host_input(
    sender: mpsc::Sender<io::Result<RxJsonRpcMessage<RoleServer>>>,
    max_message_bytes: usize,
) {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut codec =
        JsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(max_message_bytes);
    loop {
        let line = match read_line_bounded(&mut reader, max_message_bytes) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                let _ = sender.blocking_send(Err(error));
                break;
            }
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut bytes = BytesMut::from(line.as_slice());
        match codec.decode(&mut bytes) {
            Ok(Some(message)) => {
                if sender.blocking_send(Ok(message)).is_err() {
                    break;
                }
            }
            Ok(None) => continue,
            Err(error) => {
                let _ = sender.blocking_send(Err(error.into()));
                break;
            }
        }
    }
}

fn read_line_bounded(reader: &mut impl BufRead, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        let payload_bytes = line.len() + count - usize::from(newline.is_some());
        if payload_bytes > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP message exceeds the configured byte limit",
            ));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}
