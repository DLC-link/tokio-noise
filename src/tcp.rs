use bytes::BufMut;
use log::{debug, error, info, trace, warn};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

use crate::errors::NoiseError;
use crate::handshakes::{Handshake, NNpsk0};

const NOISE_TAG_SIZE: usize = 16;
const NOISE_NONCE_SIZE: usize = 8;
const CIPHERTEXT_PACKET_SIZE: usize = 2048;
const PLAINTEXT_PACKET_SIZE: usize = CIPHERTEXT_PACKET_SIZE - NOISE_TAG_SIZE - NOISE_NONCE_SIZE;
const MIN_CIPHERTEXT_PACKET_SIZE: usize = NOISE_NONCE_SIZE + NOISE_TAG_SIZE;

/// Represents a [`tokio::io::TcpStream`] wrapped with a layer of [Noise](https://noiseprotocol.org/)
/// encryption applied on top.
pub struct NoiseTcpStream {
    name: String,
    tcp: TcpStream,
    noise: snow::TransportState,
    read_overflow_buf: VecDeque<u8>,
}

impl NoiseTcpStream {
    /// Instantiate a new encrypted stream using the given noise transport state machine.
    /// The name can be any arbitrary identifier for the stream - it is only used for logging.
    pub fn new(name: String, socket: TcpStream, noise: snow::TransportState) -> NoiseTcpStream {
        NoiseTcpStream {
            name,
            tcp: socket,
            noise,
            read_overflow_buf: VecDeque::with_capacity(CIPHERTEXT_PACKET_SIZE),
        }
    }

    /// Conduct a Noise handshake over the given TCP socket as the initiator,
    /// using a custom [`Handshake`] protocol.
    pub async fn handshake_initiator(
        mut socket: TcpStream,
        mut handshake: impl Handshake,
    ) -> Result<NoiseTcpStream, NoiseError> {
        let mut recv_cipher_buf = [0u8; CIPHERTEXT_PACKET_SIZE];
        let mut recv_clear_buf = [0u8; PLAINTEXT_PACKET_SIZE];
        let mut send_buf = [0u8; CIPHERTEXT_PACKET_SIZE];

        let mut initiator = handshake.new_builder().build_initiator()?;

        // -> 1
        let wrote_n = handshake.initiator_first_message(&mut initiator, &mut send_buf)?;
        socket.write_all(&send_buf[..wrote_n]).await?;
        debug!(
            "[initiator] sent initial {}-byte message to responder",
            wrote_n
        );

        let mut read_overflow_buf = VecDeque::with_capacity(CIPHERTEXT_PACKET_SIZE);

        // <- 2
        if !initiator.is_handshake_finished() {
            let read_cipher_n = socket.read(&mut recv_cipher_buf).await?;
            debug!(
                "[initiator] received initial {}-byte reply from responder",
                read_cipher_n
            );

            let read_clear_n =
                initiator.read_message(&recv_cipher_buf[..read_cipher_n], &mut recv_clear_buf)?;
            debug!(
                "[initiator] decrypted initial {}-byte reply from responder",
                read_cipher_n
            );

            // -> 3
            if !initiator.is_handshake_finished() {
                let wrote_n = handshake.initiator_second_message(
                    &mut initiator,
                    &recv_clear_buf[..read_clear_n],
                    &mut send_buf,
                )?;
                socket.write_all(&send_buf[..wrote_n]).await?;
                debug!(
                    "[initiator] sent second {}-byte message to responder",
                    wrote_n
                );

                // <- 4
                if !initiator.is_handshake_finished() {
                    let read_cipher_n = socket.read(&mut recv_cipher_buf).await?;
                    debug!(
                        "[initiator] received second {}-byte reply from responder",
                        read_cipher_n
                    );

                    let read_clear_n = initiator
                        .read_message(&recv_cipher_buf[..read_cipher_n], &mut recv_clear_buf)?;
                    debug!(
                        "[initiator] decrypted second {}-byte reply from responder",
                        read_clear_n
                    );

                    // Dump any additional bytes read into the buffer so the caller will read
                    // them first.
                    read_overflow_buf.extend(&recv_clear_buf[..read_clear_n]);

                    assert!(
                        initiator.is_handshake_finished(),
                        "handshake should always finish after 4 messages"
                    );
                }
            } else {
                read_overflow_buf.extend(&recv_clear_buf[..read_clear_n]);
            }
        }

        let chan = NoiseTcpStream {
            name: "initiator".to_string(),
            tcp: socket,
            noise: initiator.into_transport_mode()?,
            read_overflow_buf,
        };

        info!("[initiator] completed noise handshake");
        Ok(chan)
    }

    /// Conduct a Noise handshake over the given TCP socket as the responder,
    /// using a custom [`Handshake`] protocol.
    pub async fn handshake_responder(
        mut socket: TcpStream,
        mut handshake: impl Handshake,
    ) -> Result<NoiseTcpStream, NoiseError> {
        let mut recv_cipher_buf = [0u8; CIPHERTEXT_PACKET_SIZE];
        let mut recv_clear_buf = [0u8; PLAINTEXT_PACKET_SIZE];
        let mut send_buf = [0u8; CIPHERTEXT_PACKET_SIZE];

        let mut responder = handshake.new_builder().build_responder()?;

        // -> 1
        let read_cipher_n = socket.read(&mut recv_cipher_buf).await?;
        debug!(
            "[responder] received initial {}-byte message from initiator",
            read_cipher_n
        );

        let read_clear_n =
            responder.read_message(&recv_cipher_buf[..read_cipher_n], &mut recv_clear_buf)?;
        debug!(
            "[responder] decrypted initial {}-byte message from initiator",
            read_cipher_n
        );

        let mut read_overflow_buf = VecDeque::with_capacity(CIPHERTEXT_PACKET_SIZE);

        // <- 2
        if !responder.is_handshake_finished() {
            let wrote_n = handshake.responder_first_message(
                &mut responder,
                &recv_clear_buf[..read_clear_n],
                &mut send_buf,
            )?;
            socket.write_all(&send_buf[..wrote_n]).await?;
            debug!(
                "[responder] sent initial {}-byte reply to initiator",
                wrote_n
            );

            // -> 3
            if !responder.is_handshake_finished() {
                let read_cipher_n = socket.read(&mut recv_cipher_buf).await?;
                debug!(
                    "[responder] received second {}-byte reply from initiator",
                    read_cipher_n
                );

                let read_clear_n = responder
                    .read_message(&recv_cipher_buf[..read_cipher_n], &mut recv_clear_buf)?;
                debug!(
                    "[responder] decrypted second {}-byte reply from initiator",
                    read_clear_n
                );

                // <- 4
                if !responder.is_handshake_finished() {
                    let wrote_n = handshake.responder_second_message(
                        &mut responder,
                        &recv_clear_buf[..read_clear_n],
                        &mut send_buf,
                    )?;
                    socket.write_all(&send_buf[..wrote_n]).await?;
                    debug!(
                        "[responder] sent second {}-byte message to initiator",
                        wrote_n
                    );
                } else {
                    read_overflow_buf.extend(&recv_clear_buf[..read_clear_n]);
                }
            }
        } else {
            read_overflow_buf.extend(&recv_clear_buf[..read_clear_n]);
        }

        let chan = NoiseTcpStream {
            name: "responder".to_string(),
            tcp: socket,
            noise: responder.into_transport_mode()?,
            read_overflow_buf,
        };

        info!("[responder] completed noise handshake");
        Ok(chan)
    }

    /// Conduct an `NNpsk0` handshake as the Noise initiator.
    ///
    /// This presumes the initiator and responder both have access to the same pre-shared key (PSK),
    /// which is used for authentication and encryption of the proceeding handshake, which establishes
    /// a session key with perfect-forward secrecy.
    pub async fn handshake_initiator_psk0(
        socket: TcpStream,
        psk: &[u8],
    ) -> Result<NoiseTcpStream, NoiseError> {
        NoiseTcpStream::handshake_initiator(socket, NNpsk0::new(psk)).await
    }

    /// Conduct an `NNpsk0` handshake as the Noise responder.
    ///
    /// This presumes the initiator and responder both have access to the same pre-shared key (PSK),
    /// which is used for authentication and encryption of the proceeding handshake, which establishes
    /// a session key with perfect-forward secrecy.
    pub async fn handshake_responder_psk0(
        socket: TcpStream,
        psk: &[u8],
    ) -> Result<NoiseTcpStream, NoiseError> {
        NoiseTcpStream::handshake_responder(socket, NNpsk0::new(psk)).await
    }

    /// Send some arbitrary data over the noise-encrypted channel.
    pub async fn send(&mut self, cleartext: &[u8]) -> Result<(), NoiseError> {
        AsyncWriteExt::write_all(self, cleartext).await?;
        Ok(())
    }

    /// Receive some arbitrary data over the noise-encrypted channel.
    pub async fn recv(&mut self, output: &mut [u8]) -> Result<usize, NoiseError> {
        Ok(AsyncReadExt::read(self, output).await?)
    }

    /// Wraps [`TcpStream::nodelay`].
    pub fn nodelay(&self) -> Result<bool, io::Error> {
        self.tcp.nodelay()
    }
    /// Wraps [`TcpStream::set_nodelay`].
    pub fn set_nodelay(&self, nodelay: bool) -> Result<(), io::Error> {
        self.tcp.set_nodelay(nodelay)
    }
    /// Wraps [`TcpStream::linger`].
    pub fn linger(&self) -> Result<Option<Duration>, io::Error> {
        self.tcp.linger()
    }
    /// Wraps [`TcpStream::set_linger`].
    pub fn set_linger(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.tcp.set_linger(dur)
    }
    /// Wraps [`TcpStream::ttl`].
    pub fn ttl(&self) -> Result<u32, io::Error> {
        self.tcp.ttl()
    }
    /// Wraps [`TcpStream::set_ttl`].
    pub fn set_ttl(&self, ttl: u32) -> Result<(), io::Error> {
        self.tcp.set_ttl(ttl)
    }
    /// Wraps [`TcpStream::local_addr`].
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.tcp.local_addr()
    }
    /// Wraps [`TcpStream::peer_addr`].
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.tcp.peer_addr()
    }
    /// Wraps [`TcpStream::take_error`].
    pub fn take_error(&self) -> Result<Option<io::Error>, io::Error> {
        self.tcp.take_error()
    }
    /// Wraps [`TcpStream::ready`].
    pub async fn ready(&self, interest: io::Interest) -> Result<io::Ready, io::Error> {
        self.tcp.ready(interest).await
    }
    /// Wraps [`TcpStream::readable`].
    pub async fn readable(&self) -> Result<(), io::Error> {
        self.tcp.readable().await
    }
    /// Wraps [`TcpStream::writable`].
    pub async fn writable(&self) -> Result<(), io::Error> {
        self.tcp.writable().await
    }
    /// Wraps [`TcpStream::poll_read_ready`].
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.tcp.poll_read_ready(cx)
    }
    /// Wraps [`TcpStream::poll_write_ready`].
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.tcp.poll_write_ready(cx)
    }
}

impl AsyncWrite for NoiseTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.tcp.poll_write_ready(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        };

        let nonce = self.noise.sending_nonce();

        if buf.len() > PLAINTEXT_PACKET_SIZE {
            buf = &buf[..PLAINTEXT_PACKET_SIZE];
        }

        let mut ciphertext = [0u8; CIPHERTEXT_PACKET_SIZE];
        write_u64(&mut ciphertext[..NOISE_NONCE_SIZE], nonce);
        let wrote_n = match self
            .noise
            .write_message(buf, &mut ciphertext[NOISE_NONCE_SIZE..])
        {
            Ok(n) => n + NOISE_NONCE_SIZE,
            Err(e) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    e.to_string(),
                )));
            }
        };

        trace!(
            "[{}] invoking poll_write; plaintext={} ciphertext={} nonce={}",
            self.name,
            buf.len(),
            wrote_n,
            nonce
        );

        match AsyncWrite::poll_write(Pin::new(&mut self.tcp), cx, &ciphertext[..wrote_n]) {
            Poll::Ready(Ok(sent_n)) => {
                trace!("[{}] poll_write sent {} bytes", self.name, sent_n);

                // TODO what happens if we didn't write the full message?
                assert_eq!(
                    sent_n, wrote_n,
                    "underlying writer didn't write the full noise message"
                );
                Poll::Ready(Ok(buf.len()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => {
                warn!(
                    "[{}] hit pending state after noise state update; skipping nonce={}",
                    self.name, nonce
                );
                Poll::Pending
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.tcp), cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.tcp), cx)
    }
}

impl AsyncRead for NoiseTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output_buf: &mut io::ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let mut total_read = 0;
        loop {
            while let Some(byte) = self.read_overflow_buf.pop_front() {
                output_buf.put_u8(byte);
                if output_buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }
            }

            let mut ciphertext = [0u8; CIPHERTEXT_PACKET_SIZE];
            let mut ciphertext_buf = io::ReadBuf::new(&mut ciphertext);

            match AsyncRead::poll_read(Pin::new(&mut self.tcp), cx, &mut ciphertext_buf) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    // No data left to read from socket
                    if total_read > 0 {
                        return Poll::Ready(Ok(()));
                    } else {
                        return Poll::Pending;
                    }
                }
            };

            let filled = ciphertext_buf.filled();

            // No data left in socket.
            if filled.len() == 0 {
                return Poll::Ready(Ok(()));
            }

            // Invalid noise message.
            if filled.len() < MIN_CIPHERTEXT_PACKET_SIZE {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "received message is too short to hold noise message",
                )));
            }

            let mut cleartext = [0u8; PLAINTEXT_PACKET_SIZE];
            let mut our_nonce = self.noise.receiving_nonce();
            let their_nonce = read_u64(&filled[..NOISE_NONCE_SIZE]);

            // Sometimes the remote side will encounter a problem sending, and for safety
            // they cannot reuse nonces. So they specify which nonce they used in each
            // message. As long as the nonce claimed by the remote side is no lower than
            // the nonce in our local state, it is safe to update our receiving nonce to match.
            if their_nonce > our_nonce {
                our_nonce = their_nonce;
                self.noise.set_receiving_nonce(their_nonce);
            }

            match self
                .noise
                .read_message(&filled[NOISE_NONCE_SIZE..], &mut cleartext)
            {
                Ok(read_n) => {
                    trace!(
                        "[{}] poll_read OK; ciphertext={} plaintext={} nonce={}",
                        self.name,
                        filled.len(),
                        read_n,
                        our_nonce
                    );

                    // No room left in output buffer. Fill it and return.
                    if output_buf.remaining() <= read_n {
                        output_buf.put_slice(&cleartext[..output_buf.remaining()]);
                        self.read_overflow_buf
                            .extend(&cleartext[output_buf.remaining()..read_n]);
                        return Poll::Ready(Ok(()));
                    }

                    output_buf.put_slice(&cleartext[..read_n]);
                    total_read += read_n;
                }

                Err(e) => {
                    error!(
                        "[{}] poll_read ERROR; ciphertext={} nonce={}; error message: {}",
                        self.name,
                        filled.len(),
                        our_nonce,
                        e
                    );
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        e.to_string(),
                    )));
                }
            };
        }
    }
}

fn write_u64(buf: &mut [u8], n: u64) {
    buf.copy_from_slice(&n.to_be_bytes());
}

fn read_u64(buf: &[u8]) -> u64 {
    let mut array = [0u8; 8];
    array.copy_from_slice(buf);
    u64::from_be_bytes(array)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty, Full};
    use hyper::body::Bytes;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::future::Future;
    use tokio::{net::TcpListener, task::spawn};

    async fn run_client_server_test<T1, T2, F1, F2>(server_run: T1, client_run: T2)
    where
        T1: Send + 'static + FnOnce(NoiseTcpStream) -> F1,
        T2: Send + 'static + FnOnce(NoiseTcpStream) -> F2,
        F1: Send + Future<Output = ()>,
        F2: Send + Future<Output = ()>,
    {
        let psk = [10u8; 32];

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let srv = spawn(async move {
            let (tcp_stream, _) = listener.accept().await.unwrap();
            let noise_stream = NoiseTcpStream::handshake_responder_psk0(tcp_stream, &psk)
                .await
                .expect("noise handshake failed on server side");

            server_run(noise_stream).await;
        });

        let tcp_stream = TcpStream::connect(&addr).await.unwrap();
        let noise_stream = NoiseTcpStream::handshake_initiator_psk0(tcp_stream, &psk)
            .await
            .expect("noise handshake failed on client side");

        client_run(noise_stream).await;

        srv.await.unwrap();
    }

    #[tokio::test]
    async fn send_and_recv_small() {
        let server_run = |mut noise_stream: NoiseTcpStream| async move {
            let mut ok_buf = [0u8; 2];
            let n = noise_stream
                .recv(&mut ok_buf)
                .await
                .expect("server failed to receive OK");

            assert_eq!(n, ok_buf.len());
            assert_eq!(&ok_buf, b"OK");

            noise_stream
                .send(&ok_buf)
                .await
                .expect("server failed to reply OK");
        };

        let client_run = |mut noise_stream: NoiseTcpStream| async move {
            noise_stream
                .send(b"OK")
                .await
                .expect("client failed to send OK");

            let mut ok_buf = [0u8; 2];
            let n = noise_stream
                .recv(&mut ok_buf)
                .await
                .expect("client failed to receive OK");

            assert_eq!(n, ok_buf.len());
            assert_eq!(&ok_buf, b"OK");
        };

        run_client_server_test(server_run, client_run).await;
    }

    #[tokio::test]
    async fn send_and_recv_large() {
        const BIG_SIZE: usize = 200_000;

        let server_run = |mut noise_stream: NoiseTcpStream| async move {
            let mut big_buf = [0u8; BIG_SIZE];
            let n = noise_stream
                .recv(&mut big_buf)
                .await
                .expect("server failed to receive big chunk of data");

            assert_eq!(n, BIG_SIZE);
            assert_eq!(big_buf, [0xFF; BIG_SIZE]);

            noise_stream
                .send(&big_buf)
                .await
                .expect("server failed to reply with big chunk of data");
        };

        let client_run = |mut noise_stream: NoiseTcpStream| async move {
            let mut big_buf = [0xFF; BIG_SIZE];
            noise_stream
                .send(&big_buf)
                .await
                .expect("client failed to send big chunk of data");

            let n = noise_stream
                .recv(&mut big_buf)
                .await
                .expect("client failed to receive big chunk of data");

            assert_eq!(n, BIG_SIZE);
            assert_eq!(big_buf, [0xFF; BIG_SIZE]);
        };

        run_client_server_test(server_run, client_run).await;
    }

    #[tokio::test]
    async fn http1_get() {
        let server_run = |noise_stream: NoiseTcpStream| async move {
            async fn service_fn(
                _req: Request<hyper::body::Incoming>,
            ) -> Result<Response<Full<Bytes>>, hyper::Error> {
                let resp = Response::new(Full::new(Bytes::from_static(b"Hello world!")));
                Ok(resp)
            }

            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(noise_stream),
                    hyper::service::service_fn(service_fn),
                )
                .await
                .expect("error serving HTTP1 GET request");
        };

        let client_run = |noise_stream: NoiseTcpStream| async move {
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(TokioIo::new(noise_stream))
                    .await
                    .expect("client failed to run HTTP1 handshake");

            // Spawn a task to poll the connection, driving the HTTP state
            let driver = spawn(async move {
                conn.await.expect("client connection driver failed");
            });

            // Create an HTTP request with an empty body
            let req = Request::builder().body(Empty::<Bytes>::new()).unwrap();

            let res = sender
                .send_request(req)
                .await
                .expect("client failed to send HTTP1 GET request");

            assert_eq!(res.status(), 200);

            let response_bytes = res
                .collect()
                .await
                .expect("client error reading response body")
                .to_bytes();

            // Close the connection
            drop(sender);
            driver.await.unwrap();

            assert_eq!(response_bytes, b"Hello world!".as_ref());
        };

        run_client_server_test(server_run, client_run).await;
    }

    #[tokio::test]
    async fn http1_post() {
        let server_run = |noise_stream: NoiseTcpStream| async move {
            async fn service_fn(
                req: Request<hyper::body::Incoming>,
            ) -> Result<Response<Full<Bytes>>, hyper::Error> {
                let request_bytes = req
                    .collect()
                    .await
                    .expect("server error reading request body")
                    .to_bytes();

                assert_eq!(request_bytes, b"Client says hi".as_ref());

                let resp = Response::new(Full::new(Bytes::from_static(b"Hello client!")));
                Ok(resp)
            }

            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(noise_stream),
                    hyper::service::service_fn(service_fn),
                )
                .await
                .expect("error serving HTTP1 POST request");
        };

        let client_run = |noise_stream: NoiseTcpStream| async move {
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(TokioIo::new(noise_stream))
                    .await
                    .expect("client failed to run HTTP1 handshake");

            // Spawn a task to poll the connection, driving the HTTP state
            let driver = spawn(async move {
                conn.await.expect("client connection driver failed");
            });

            // Create an HTTP POST request with body
            let req = Request::builder()
                .method("POST")
                .body(Full::new(Bytes::from_static(b"Client says hi")))
                .unwrap();

            let res = sender
                .send_request(req)
                .await
                .expect("client failed to send HTTP1 POST request");

            assert_eq!(res.status(), 200);

            let response_bytes = res
                .collect()
                .await
                .expect("client error reading response body")
                .to_bytes();

            // Close the connection
            drop(sender);
            driver.await.unwrap();

            assert_eq!(response_bytes, b"Hello client!".as_ref());
        };

        run_client_server_test(server_run, client_run).await;
    }
}
