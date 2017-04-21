use std::io;
use std::net::SocketAddr;
use std::convert::Into;
use std::collections::VecDeque;
use cpython::*;
use futures::unsync::mpsc;
use futures::{Async, AsyncSink, Stream, Future, Poll, Sink};
use tokio_io::AsyncRead;
use tokio_io::codec::Framed;
use tokio_core::net::TcpStream;

use http::codec::{HttpTransportCodec, EncoderMessage};
use http::pytransport::{PyHttpTransport, PyHttpTransportMessage};
use pyunsafe::{Handle, Sender};


pub fn http_transport_factory(handle: Handle, factory: &PyObject,
                              socket: TcpStream, _peer: SocketAddr) -> Result<(), io::Error> {
    let gil = Python::acquire_gil();
    let py = gil.python();

    let (tx, rx) = mpsc::unbounded();
    let tr = PyHttpTransport::new(py, handle.clone(), Sender::new(tx), factory)?;
    let tr2 = tr.clone_ref(py);
    let tr3 = tr.clone_ref(py);

    // create internal wire transport
    let transport = HttpTransport::new(socket, rx, tr);

    // start connection processing
    handle.spawn(
        transport.map(move |_| {
            tr2.connection_lost()
        }).map_err(move |err| {
            tr3.connection_error(err)
        })
    );
    Ok(())
}


struct HttpTransport {
    framed: Framed<TcpStream, HttpTransportCodec>,
    intake: mpsc::UnboundedReceiver<PyHttpTransportMessage>,
    transport: PyHttpTransport,

    buf: Option<EncoderMessage>,
    streams: VecDeque<mpsc::UnboundedReceiver<EncoderMessage>>,
    incoming_eof: bool,
    flushed: bool,
    closing: bool,
}

impl HttpTransport {

    fn new(socket: TcpStream,
           intake: mpsc::UnboundedReceiver<PyHttpTransportMessage>,
           transport: PyHttpTransport) -> HttpTransport {

        HttpTransport {
            framed: socket.framed(HttpTransportCodec::new()),
            intake: intake,
            transport: transport,

            buf: None,
            streams: VecDeque::new(),
            incoming_eof: false,
            flushed: false,
            closing: false,
        }
    }
}


impl Future for HttpTransport
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // poll for incoming data
        if !self.incoming_eof {
            loop {
                match self.framed.poll() {
                    Ok(Async::Ready(Some(msg))) => {
                        if let Some(recv) = self.transport.data_received(msg)? {
                            self.streams.push_back(recv);
                        }
                        continue
                    },
                    Ok(Async::Ready(None)) => {
                        // TODO send eof_received to pytransport
                        self.incoming_eof = true;
                    },
                    Ok(Async::NotReady) => (),
                    Err(err) => return Err(err.into()),
                }
                break
            }
        }

        // process outgoing data
        'sink: loop {
            if let Some(msg) = self.buf.take() {
                self.flushed = false;

                let enc_msg = match self.framed.start_send(msg) {
                    Ok(AsyncSink::NotReady(bytes)) => {
                        Some(bytes)
                    },
                    Ok(AsyncSink::Ready) => None,
                    Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "Closed")),
                };
                // unprocessed data
                if let Some(msg) = enc_msg {
                    self.buf = Some(msg);
                    break
                }
            }

            // poll streams
            'streams: loop {
                match self.streams.front_mut() {
                    Some(ref mut stream) => {
                        match stream.poll() {
                            Ok(Async::Ready(Some(msg))) => {     // data available, try to send
                                self.buf = Some(msg);
                                continue 'sink
                            },
                            Ok(Async::Ready(None)) => (),        // stream is empty
                            Ok(Async::NotReady) => break 'sink,  // no data available
                            Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "Closed")),
                        }
                    }
                    None => break 'sink,
                }
                // this can happen only if stream is empty
                let _ = self.streams.pop_front();
            }
        }

        // commands from transport
        match self.intake.poll() {
            Ok(Async::Ready(Some(msg))) => {
                match msg {
                    PyHttpTransportMessage::Close(_) => {
                        trace!("Start transport closing procesdure");
                        self.closing = true;
                    }
                }
            },
            Ok(_) => (),
            Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "Closed")),
        }

        // close
        if self.closing {
            return self.framed.close()
        }

        // flush sink
        if !self.flushed {
            self.flushed = self.framed.poll_complete()?.is_ready();
        }

        Ok(Async::NotReady)
    }
}