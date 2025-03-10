use core::task::{Context, Poll};
use std::collections::HashMap;
use std::io::Cursor;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::fmt::Debug;

use async_channel::bounded;
use async_channel::Receiver;
use async_channel::Sender;
use async_lock::Mutex;
use bytes::BytesMut;
use event_listener::Event;
use futures_util::stream::{Stream, StreamExt};
use pin_project::{pin_project, pinned_drop};
use tokio::select;
use tracing::{debug, error, trace, instrument};

use fluvio_future::timer::sleep;
use fluvio_protocol::api::Request;
use fluvio_protocol::api::RequestHeader;
use fluvio_protocol::api::RequestMessage;
use fluvio_protocol::Decoder;

use crate::SocketError;
use crate::ExclusiveFlvSink;
use crate::FluvioSocket;
use crate::FluvioStream;

pub type SharedMultiplexerSocket = Arc<MultiplexerSocket>;

type SharedMsg = (Arc<Mutex<Option<BytesMut>>>, Arc<Event>);

/// Handle different way to multiplex
enum SharedSender {
    /// Serial socket
    Serial(SharedMsg),
    /// Batch Socket
    Queue(Sender<Option<BytesMut>>),
}

type Senders = Arc<Mutex<HashMap<i32, SharedSender>>>;

async fn correlation_id(counter: Arc<Mutex<i32>>) -> i32 {
    let mut guard = counter.lock().await;
    let current_value = *guard;
    // update to new
    *guard = current_value + 1;
    current_value
}

/// Socket that can multiplex connections
pub struct MultiplexerSocket {
    correlation_id_counter: Arc<Mutex<i32>>,
    senders: Senders,
    sink: ExclusiveFlvSink,
    terminate: Arc<Event>,
}

impl Debug for MultiplexerSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MultiplexerSocket {}", self.sink.id())
    }
}

impl Drop for MultiplexerSocket {
    fn drop(&mut self) {
        // notify dispatcher
        self.terminate.notify(usize::MAX);
    }
}

impl MultiplexerSocket {
    pub fn shared(socket: FluvioSocket) -> Arc<Self> {
        Arc::new(Self::new(socket))
    }

    /// create new multiplexer socket, this always starts with correlation id of 1
    /// correlation id of 0 means shared
    pub fn new(socket: FluvioSocket) -> Self {
        let (sink, stream) = socket.split();

        let multiplexer = Self {
            correlation_id_counter: Arc::new(Mutex::new(1)),
            senders: Arc::new(Mutex::new(HashMap::new())),
            sink: ExclusiveFlvSink::new(sink),
            terminate: Arc::new(Event::new()),
        };

        MultiPlexingResponseDispatcher::run(
            stream,
            multiplexer.senders.clone(),
            multiplexer.terminate.clone(),
        );

        multiplexer
    }

    /// get next available correlation to use
    //  use lock to ensure update happens in orderly manner
    async fn next_correlation_id(&self) -> i32 {
        correlation_id(self.correlation_id_counter.clone()).await
    }

    /// create socket to perform request and response
    #[instrument(skip(self, req_msg))]
    pub async fn send_and_receive<R>(
        &self,
        mut req_msg: RequestMessage<R>,
    ) -> Result<R::Response, SocketError>
    where
        R: Request,
    {
        use once_cell::sync::Lazy;

        static MAX_WAIT_TIME: Lazy<u64> = Lazy::new(|| {
            use std::env;

            let var_value = env::var("FLV_SOCKET_WAIT").unwrap_or_default();
            let wait_time: u64 = var_value.parse().unwrap_or(10);
            wait_time
        });

        let correlation_id = self.next_correlation_id().await;
        let bytes_lock: SharedMsg = (Arc::new(Mutex::new(None)), Arc::new(Event::new()));

        req_msg.header.set_correlation_id(correlation_id);

        trace!("senders trying lock");
        let mut senders = self.senders.lock().await;
        senders.insert(correlation_id, SharedSender::Serial(bytes_lock.clone()));
        drop(senders);

        debug!(
            "serial multiplexing: sending request: {} id: {}",
            R::API_KEY,
            correlation_id
        );
        self.sink.send_request(&req_msg).await?;

        trace!("inserts shared sender");
        let (msg, msg_event) = bytes_lock;

        select! {
            _ = sleep(Duration::from_secs(*MAX_WAIT_TIME)) => {
                let mut senders = self.senders.lock().await;
                senders.remove(&correlation_id);
                drop(senders);
                debug!("serial socket for: {}  timeout happen, id: {}", R::API_KEY, correlation_id);

                Err(IoError::new(
                    ErrorKind::TimedOut,
                    format!("time out in serial: {} request: {}", R::API_KEY, correlation_id),
                ).into())
            },

            _ = msg_event.listen() => {

                let mut senders = self.senders.lock().await;
                senders.remove(&correlation_id);
                drop(senders);

                match msg.try_lock() {
                    Some(guard) => {

                        if let Some(response_bytes) =  &*guard {

                            debug!("receive serial socket id: {}, bytes: {}", correlation_id, response_bytes.len());
                            let response = R::Response::decode_from(
                                &mut Cursor::new(&response_bytes),
                                req_msg.header.api_version(),
                            )?;
                            trace!("receive serial socket id: {}, response: {:#?}", correlation_id, response);
                            Ok(response)
                        } else {
                            debug!("serial socket: {}, id: {}, value is empty, something bad happened",R::API_KEY,correlation_id);
                            Err(IoError::new(
                                ErrorKind::UnexpectedEof,
                                "connection is closed".to_string(),
                            ).into())
                        }

                    },
                    None => Err(IoError::new(
                        ErrorKind::BrokenPipe,
                        format!("locked failed: {}, serial socket is in bad state",correlation_id)
                    ).into())
                }
            },
        }
    }

    /// create stream response
    #[instrument(skip(self,req_msg), fields(api = R::API_KEY))]
    pub async fn create_stream<R>(
        &self,
        mut req_msg: RequestMessage<R>,
        queue_len: usize,
    ) -> Result<AsyncResponse<R>, SocketError>
    where
        R: Request,
    {
        let correlation_id = self.next_correlation_id().await;

        req_msg.header.set_correlation_id(correlation_id);

        trace!(correlation_id,request = ?req_msg, "new correlation id");

        // set up new channel
        let (sender, receiver) = bounded(queue_len);
        let mut senders = self.senders.lock().await;

        // remove any closed channel, this is not optimal but should do trick for now

        senders.retain(|_, shared_sender| match shared_sender {
            SharedSender::Serial(_) => true,
            SharedSender::Queue(sender) => !sender.is_closed(),
        });

        senders.insert(correlation_id, SharedSender::Queue(sender));
        drop(senders);

        trace!(correlation_id, "created new channel");

        self.sink.send_request(&req_msg).await?;

        trace!(correlation_id, "request send");

        // it is possible that msg have received by dispatcher before channel is inserted into senders
        // but it is easier to clean up

        Ok(AsyncResponse {
            receiver,
            header: req_msg.header,
            correlation_id,
            data: PhantomData,
        })
    }
}

/// Implement async socket where response are send back async manner
/// they are queued using channel
#[pin_project(PinnedDrop)]
pub struct AsyncResponse<R> {
    #[pin]
    receiver: Receiver<Option<BytesMut>>,
    header: RequestHeader,
    correlation_id: i32,
    data: PhantomData<R>,
}

#[pinned_drop]
impl<R> PinnedDrop for AsyncResponse<R> {
    fn drop(self: Pin<&mut Self>) {
        self.receiver.close();
        debug!("multiplexer stream: {} closed", self.correlation_id);
    }
}

impl<R: Request> Stream for AsyncResponse<R> {
    type Item = Result<R::Response, SocketError>;

    #[instrument(
        skip(self, cx),
        fields(
            api_key = R::API_KEY,
            correlation_id = self.correlation_id,
        )
    )]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let next: Option<Option<_>> = match this.receiver.poll_next(cx) {
            Poll::Pending => {
                trace!("Waiting for async response");
                return Poll::Pending;
            }
            Poll::Ready(next) => next,
        };

        let bytes = if let Some(bytes) = next {
            bytes
        } else {
            return Poll::Ready(None);
        };

        let bytes = if let Some(bytes) = bytes {
            bytes
        } else {
            return Poll::Ready(Some(Err(SocketError::SocketClosed)));
        };

        let mut cursor = Cursor::new(&bytes);
        let response = R::Response::decode_from(&mut cursor, this.header.api_version());

        let value = match response {
            Ok(value) => {
                trace!("Received response bytes: {},  {:#?}", bytes.len(), &value,);
                Some(Ok(value))
            }
            Err(e) => Some(Err(e.into())),
        };
        Poll::Ready(value)
    }
}

/// This decodes fluvio protocol based streams and multiplex into different slots
struct MultiPlexingResponseDispatcher {
    senders: Senders,
    terminate: Arc<Event>,
}

impl MultiPlexingResponseDispatcher {
    pub fn run(stream: FluvioStream, senders: Senders, terminate: Arc<Event>) {
        use fluvio_future::task::spawn;

        let dispatcher = Self { senders, terminate };

        debug!("dispatcher: spawning dispatcher loop");
        spawn(dispatcher.dispatcher_loop(stream));
    }

    #[instrument(name = "loop", skip(self, stream))]
    async fn dispatcher_loop(mut self, mut stream: FluvioStream) {
        let frame_stream = stream.get_mut_tcp_stream();

        loop {
            trace!("waiting");

            select! {
                frame = frame_stream.next() => {
                    if let Some(request) = frame {
                        if let Ok(mut msg) = request {
                            let mut correlation_id: i32 = 0;
                            match correlation_id.decode(&mut msg, 0) {
                                Ok(_) => {
                                    trace!(correlation_id,len = msg.len(), "received frame");

                                    if let Err(err) = self.send(correlation_id, msg).await {
                                        error!("error sending to socket, {}", err)
                                    }
                                }
                                Err(err) => error!("error decoding response, {}", err),
                            }
                        } else {
                            debug!("problem getting frame from stream. terminating");
                            break;
                        }
                    } else {
                        debug!("inner stream has terminated ");

                        let guard = self.senders.lock().await;
                        for sender in guard.values() {
                            match sender {
                                SharedSender::Serial(_) => {},
                                SharedSender::Queue(stream_sender) => {
                                    let _ = stream_sender.send(None).await;
                                }
                            }
                        }
                        break;
                    }
                },

                _ = self.terminate.listen() => {
                    // terminate all channels

                    let guard = self.senders.lock().await;
                    for sender in guard.values() {
                        match sender {
                            SharedSender::Serial(_) => {},
                            SharedSender::Queue(stream_sender) => {
                                stream_sender.close();
                            }
                        }
                    }

                    debug!("multiplexer terminated");
                    break;

                }
            }
        }
    }

    /// send message to correct receiver
    #[instrument(skip(self, msg),fields( msg = msg.len()))]
    pub async fn send(&mut self, correlation_id: i32, msg: BytesMut) -> Result<(), SocketError> {
        let mut senders = self.senders.lock().await;
        if let Some(sender) = senders.get_mut(&correlation_id) {
            match sender {
                SharedSender::Serial(serial_sender) => {
                    trace!("found serial");
                    // this should always succeed since nobody should lock
                    match serial_sender.0.try_lock() {
                        Some(mut guard) => {
                            *guard = Some(msg);
                            drop(guard); // unlock
                            serial_sender.1.notify(1);

                            Ok(())
                        }
                        None => Err(IoError::new(
                            ErrorKind::BrokenPipe,
                            format!(
                                "failed locking, abandoning sending to socket: {}",
                                correlation_id
                            ),
                        )
                        .into()),
                    }
                }
                SharedSender::Queue(queue_sender) => {
                    trace!("found queue");
                    queue_sender.send(Some(msg)).await.map_err(|err| {
                        IoError::new(
                            ErrorKind::BrokenPipe,
                            format!(
                                "problem sending to queue socket: {}, err: {}",
                                correlation_id, err
                            ),
                        )
                        .into()
                    })
                }
            }
        } else {
            Err(IoError::new(
                ErrorKind::BrokenPipe,
                format!(
                    "no socket receiver founded for id: {}, abandoning sending",
                    correlation_id
                ),
            )
            .into())
        }
    }
}

#[cfg(test)]
mod tests {

    use std::time::Duration;

    use async_trait::async_trait;
    use futures_util::future::{join, join3};
    use futures_util::io::{AsyncRead, AsyncWrite};
    use futures_util::StreamExt;
    use tracing::debug;

    use fluvio_future::net::TcpListener;
    use fluvio_future::net::TcpStream;
    use fluvio_future::task::spawn;
    use fluvio_future::timer::sleep;
    use fluvio_protocol::api::RequestMessage;

    use super::MultiplexerSocket;
    use crate::test_request::*;
    use crate::ExclusiveFlvSink;
    use crate::FluvioSocket;

    #[allow(unused)]
    const CA_PATH: &str = "certs/certs/ca.crt";
    #[allow(unused)]
    const X509_SERVER: &str = "certs/certs/server.crt";
    #[allow(unused)]
    const X509_SERVER_KEY: &str = "certs/certs/server.key";
    #[allow(unused)]
    const X509_CLIENT: &str = "certs/certs/client.crt";
    #[allow(unused)]
    const X509_CLIENT_KEY: &str = "certs/certs/client.key";

    #[async_trait]
    trait AcceptorHandler {
        type Stream: AsyncRead + AsyncWrite + Unpin + Send;
        async fn accept(&mut self, stream: TcpStream) -> FluvioSocket;
    }

    #[derive(Clone)]
    struct TcpStreamHandler {}

    #[async_trait]
    impl AcceptorHandler for TcpStreamHandler {
        type Stream = TcpStream;

        async fn accept(&mut self, stream: TcpStream) -> FluvioSocket {
            stream.into()
        }
    }

    async fn test_server<A: AcceptorHandler + 'static>(addr: &str, mut handler: A) {
        let listener = TcpListener::bind(addr).await.expect("binding");
        debug!("server is running");
        let mut incoming = listener.incoming();
        let incoming_stream = incoming.next().await;
        debug!("server: got connection");
        let incoming_stream = incoming_stream.expect("next").expect("unwrap again");
        let socket: FluvioSocket = handler.accept(incoming_stream).await;

        let (sink, mut stream) = socket.split();

        let shared_sink = ExclusiveFlvSink::new(sink);

        let mut api_stream = stream.api_stream::<TestApiRequest, TestKafkaApiEnum>();

        for i in 0..3u16 {
            debug!("server: waiting for next msg: {}", i);
            let msg = api_stream.next().await.expect("msg").expect("unwrap");
            debug!("server: msg received: {:#?}", msg);

            match msg {
                TestApiRequest::EchoRequest(echo_request) => {
                    let mut reply_sink = shared_sink.clone();
                    // depends on different request we delay
                    if echo_request.request().msg == "slow" {
                        debug!("server: received slow msg");
                        spawn(async move {
                            sleep(Duration::from_millis(500)).await;
                            let resp =
                                echo_request.new_response(EchoResponse::new("slow".to_owned()));
                            debug!("server send slow response");
                            reply_sink
                                .send_response(&resp, 0)
                                .await
                                .expect("send succeed");
                        });
                    } else {
                        debug!("server: received fast msg");
                        spawn(async move {
                            let resp =
                                echo_request.new_response(EchoResponse::new("hello".to_owned()));
                            debug!("server: send fast response");
                            reply_sink
                                .send_response(&resp, 0)
                                .await
                                .expect("send succeed");
                        });
                    }
                }
                TestApiRequest::AsyncStatusRequest(status_request) => {
                    debug!("server: received async status msg");
                    let mut reply_sink = shared_sink.clone();
                    spawn(async move {
                        sleep(Duration::from_millis(30)).await;
                        let resp = status_request.new_response(AsyncStatusResponse {
                            status: status_request.request.count * 2,
                        });
                        reply_sink
                            .send_response(&resp, 0)
                            .await
                            .expect("send succeed");
                        debug!("server: send back status first");
                        sleep(Duration::from_millis(100)).await;
                        let resp = status_request.new_response(AsyncStatusResponse {
                            status: status_request.request.count * 4,
                        });
                        reply_sink
                            .send_response(&resp, 0)
                            .await
                            .expect("send succeed");
                        debug!("server: send back status second");
                    });
                }
                _ => panic!("no echo request"),
            }
        }

        debug!("server: finish sending out"); // finish ok
    }

    #[async_trait]
    trait ConnectorHandler {
        type Stream: AsyncRead + AsyncWrite + Unpin + Send + Sync;
        async fn connect(&mut self, stream: TcpStream) -> FluvioSocket;
    }

    #[async_trait]
    impl ConnectorHandler for TcpStreamHandler {
        type Stream = TcpStream;

        async fn connect(&mut self, stream: TcpStream) -> FluvioSocket {
            stream.into()
        }
    }

    async fn test_client<C: ConnectorHandler + 'static>(addr: &str, mut handler: C) {
        use std::time::SystemTime;

        sleep(Duration::from_millis(20)).await;
        debug!("client: trying to connect");
        let tcp_stream = TcpStream::connect(&addr).await.expect("connection fail");
        let socket = handler.connect(tcp_stream).await;
        debug!("client: connected to test server and waiting...");
        sleep(Duration::from_millis(20)).await;
        let multiplexer = MultiplexerSocket::shared(socket);

        // create async status
        let async_status_request = RequestMessage::new_request(AsyncStatusRequest { count: 2 });
        let mut status_response = multiplexer
            .create_stream(async_status_request, 10)
            .await
            .expect("response");

        let multiplexor2 = multiplexer.clone();

        let (slow, fast, _) = join3(
            async move {
                debug!("trying to send slow");
                // this message was send first but since there is delay of 500ms, it will return slower than fast
                let request = RequestMessage::new_request(EchoRequest::new("slow".to_owned()));
                let response = multiplexer
                    .send_and_receive(request)
                    .await
                    .expect("send success");
                debug!("received slow response");
                assert_eq!(response.msg, "slow");
                SystemTime::now()
            },
            async move {
                // this message will be send later than slow but since there is no delay, it should get earlier than first
                sleep(Duration::from_millis(20)).await;
                debug!("trying to send fast");
                let request = RequestMessage::new_request(EchoRequest::new("fast".to_owned()));
                let response = multiplexor2
                    .send_and_receive(request)
                    .await
                    .expect("send success");
                debug!("received fast response");
                assert_eq!(response.msg, "hello");
                SystemTime::now()
            },
            async move {
                sleep(Duration::from_millis(100)).await;
                let response = status_response
                    .next()
                    .await
                    .expect("stream yields value")
                    .expect("async response");
                debug!("received async response");
                assert_eq!(response.status, 4); // multiply by 2
                let response = status_response
                    .next()
                    .await
                    .expect("stream yields value")
                    .expect("async response");
                debug!("received async response");
                assert_eq!(response.status, 8);
                SystemTime::now()
            },
        )
        .await;

        assert!(slow > fast);
    }

    #[fluvio_future::test(ignore)]
    async fn test_multiplexing() {
        debug!("start testing");
        let addr = "127.0.0.1:6000";

        let _r = join(
            test_client(addr, TcpStreamHandler {}),
            test_server(addr, TcpStreamHandler {}),
        )
        .await;
    }

    #[cfg(unix)]
    mod tls_test {
        use std::os::unix::io::AsRawFd;

        use fluvio_future::{
            native_tls::{
                AcceptorBuilder, CertBuilder, ConnectorBuilder, DefaultClientTlsStream,
                DefaultServerTlsStream, IdentityBuilder, PrivateKeyBuilder, TlsAcceptor,
                TlsConnector, X509PemBuilder,
            },
            net::SplitConnection,
        };

        use super::*;

        struct TlsAcceptorHandler(TlsAcceptor);

        impl TlsAcceptorHandler {
            fn new() -> Self {
                let acceptor = AcceptorBuilder::identity(
                    IdentityBuilder::from_x509(
                        X509PemBuilder::from_path(X509_SERVER).expect("read"),
                        PrivateKeyBuilder::from_path(X509_SERVER_KEY).expect("file"),
                    )
                    .expect("identity"),
                )
                .expect("identity:")
                .build()
                .expect("acceptor");
                Self(acceptor)
            }
        }

        #[async_trait]
        impl AcceptorHandler for TlsAcceptorHandler {
            type Stream = DefaultServerTlsStream;

            async fn accept(&mut self, stream: TcpStream) -> FluvioSocket {
                let fd = stream.as_raw_fd();
                let handshake = self.0.accept(stream);
                let tls_stream = handshake.await.expect("hand shake failed");
                let (write, read) = tls_stream.split_connection();
                FluvioSocket::from_stream(write, read, fd)
            }
        }

        struct TlsConnectorHandler(TlsConnector);

        impl TlsConnectorHandler {
            fn new() -> Self {
                let connector = ConnectorBuilder::identity(
                    IdentityBuilder::from_x509(
                        X509PemBuilder::from_path(X509_CLIENT).expect("read"),
                        PrivateKeyBuilder::from_path(X509_CLIENT_KEY).expect("read"),
                    )
                    .expect("509"),
                )
                .expect("connector")
                .danger_accept_invalid_hostnames()
                .no_cert_verification()
                .build();
                Self(connector)
            }
        }

        #[async_trait]
        impl ConnectorHandler for TlsConnectorHandler {
            type Stream = DefaultClientTlsStream;

            async fn connect(&mut self, stream: TcpStream) -> FluvioSocket {
                let fd = stream.as_raw_fd();
                let (write, read) = self
                    .0
                    .connect("localhost", stream)
                    .await
                    .expect("hand shakefailed")
                    .split_connection();

                FluvioSocket::from_stream(write, read, fd)
            }
        }

        #[fluvio_future::test(ignore)]
        async fn test_multiplexing_native_tls() {
            debug!("start testing");
            let addr = "127.0.0.1:6001";

            let _r = join(
                test_client(addr, TlsConnectorHandler::new()),
                test_server(addr, TlsAcceptorHandler::new()),
            )
            .await;
        }
    }
}
