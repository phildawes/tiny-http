// Copyright 2015 The tiny-http Contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

/*!
# Simple usage

## Creating the server

The easiest way to create a server is to call `Server::new()`.

The `new()` function returns an `IoResult<Server>` which will return an error
in the case where the server creation fails (for example if the listening port is already
occupied).

```no_run
let server = tiny_http::ServerBuilder::new().build().unwrap();
```

A newly-created `Server` will immediatly start listening for incoming connections and HTTP
requests.

## Receiving requests

Calling `server.recv()` will block until the next request is available.
This function returns an `IoResult<Request>`, so you need to handle the possible errors.

```no_run
# let server = tiny_http::ServerBuilder::new().build().unwrap();

loop {
    // blocks until the next request is received
    let request = match server.recv() {
        Ok(rq) => rq,
        Err(e) => { println!("error: {}", e); break }
    };

    // do something with the request
    // ...
}
```

In a real-case scenario, you will probably want to spawn multiple worker tasks and call
`server.recv()` on all of them. Like this:

```no_run
# use std::sync::Arc;
# use std::thread;
# let server = tiny_http::ServerBuilder::new().build().unwrap();
let server = Arc::new(server);
let mut guards = Vec::with_capacity(4);

for _ in (0 .. 4) {
    let server = server.clone();

    let guard = thread::spawn(move || {
        loop {
            let rq = server.recv().unwrap();

            // ...
        }
    });

    guards.push(guard);
}
```

If you don't want to block, you can call `server.try_recv()` instead.

## Handling requests

The `Request` object returned by `server.recv()` contains informations about the client's request.
The most useful methods are probably `request.method()` and `request.url()` which return
the requested method (`GET`, `POST`, etc.) and url.

To handle a request, you need to create a `Response` object. See the docs of this object for
more infos. Here is an example of creating a `Response` from a file:

```no_run
# use std::fs::File;
# use std::path::Path;
let response = tiny_http::Response::from_file(File::open(&Path::new("image.png")).unwrap());
```

All that remains to do is call `request.respond()`:

```no_run
# use std::fs::File;
# use std::path::Path;
# let server = tiny_http::ServerBuilder::new().build().unwrap();
# let request = server.recv().unwrap();
# let response = tiny_http::Response::from_file(File::open(&Path::new("image.png")).unwrap());
request.respond(response)
```
*/
#![crate_name = "tiny_http"]
#![crate_type = "lib"]
#![forbid(unsafe_code)]

extern crate ascii;
extern crate chunked_transfer;
extern crate encoding;
extern crate url;
extern crate chrono;

use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::net;

use client::ClientConnection;
use util::MessagesQueue;

pub use common::{Header, HeaderField, HTTPVersion, Method, StatusCode};
pub use request::Request;
pub use response::{ResponseBox, Response};

mod client;
mod common;
mod request;
mod response;

#[allow(dead_code)]     // TODO: remove when everything is implemented
mod util;

/// The main class of this library.
///
/// Destroying this object will immediatly close the listening socket annd the reading
///  part of all the client's connections. Requests that have already been returned by
///  the `recv()` function will not close and the responses will be transferred to the client.
pub struct Server {
    // should be false as long as the server exists
    // when set to true, all the subtasks will close within a few hundreds ms
    close: Arc<AtomicBool>,

    // queue for messages received by child threads
    messages: Arc<MessagesQueue<Message>>,

    // result of TcpListener::local_addr()
    listening_addr: net::SocketAddr,
}

enum Message {
    Error(IoError),
    NewRequest(Request),
}

impl From<IoError> for Message {
    fn from(e: IoError) -> Message {
        Message::Error(e)
    }
}

impl From<Request> for Message {
    fn from(rq: Request) -> Message {
        Message::NewRequest(rq)
    }
}

// this trait is to make sure that Server implements Share and Send
#[doc(hidden)]
trait MustBeShareDummy : Sync + Send {}
#[doc(hidden)]
impl MustBeShareDummy for Server {}


pub struct IncomingRequests<'a> {
    server: &'a Server
}

/// Object which allows you to build a server.
pub struct ServerBuilder {
    // the address to listen to
    address: net::SocketAddrV4,

    // number of milliseconds before client timeout
    client_timeout_ms: u32,

    // maximum number of clients before 503
    // TODO:
    //max_clients: usize,
}

impl ServerBuilder {
    /// Creates a new builder.
    pub fn new() -> ServerBuilder {
        ServerBuilder {
            address: net::SocketAddrV4::new(net::Ipv4Addr::new(0, 0, 0, 0), 80),
            client_timeout_ms: 60 * 1000,
            //max_clients: { use std::num::Bounded; Bounded::max_value() },
        }
    }

    /// The server will use a precise port.
    pub fn with_port(mut self, port: u16) -> ServerBuilder {
        let addr = self.address.ip().clone();
        self.address = net::SocketAddrV4::new(addr, port);
        self
    }

    /// The server will bind to the nic:port specified by address
    pub fn with_address(mut self, address: net::SocketAddrV4) -> ServerBuilder {
        self.address = address;
        self
    }

    /// The server will use a random port.
    ///
    /// Call `server.server_addr()` to retreive it once the server is created.
    pub fn with_random_port(mut self) -> ServerBuilder {
        let addr = self.address.ip().clone();
        self.address = net::SocketAddrV4::new(addr, 0);
        self
    }

    /// The server will use a precise port.
    pub fn with_client_connections_timeout(mut self, milliseconds: u32) -> ServerBuilder {
        self.client_timeout_ms = milliseconds;
        self
    }

    /// Builds the server with the given configuration.
    pub fn build(self) -> IoResult<Server> {
        Server::new(self)
    }
}

impl Server {
    /// Builds a new server that listens on the specified address.
    fn new(config: ServerBuilder) -> IoResult<Server> {
        // building the "close" variable
        let close_trigger = Arc::new(AtomicBool::new(false));

        // building the TcpListener
        let (server, local_addr) = {
            let listener = try!(net::TcpListener::bind(net::SocketAddr::V4(config.address)));
            let local_addr = try!(listener.local_addr());
            (listener, local_addr)
        };

        // creating a task where server.accept() is continuously called
        // and ClientConnection objects are pushed in the messages queue
        let messages = MessagesQueue::with_capacity(8);

        let inside_close_trigger = close_trigger.clone();
        let inside_messages = messages.clone();
        thread::spawn(move || {
            // a tasks pool is used to dispatch the connections into threads
            let tasks_pool = util::TaskPool::new();

            loop {
                let new_client = server.accept().map(|(sock, _)| {
                    use util::ClosableTcpStream;

                    let read_closable = ClosableTcpStream::new(sock.try_clone().unwrap(), true, false);
                    let write_closable = ClosableTcpStream::new(sock, false, true);

                    ClientConnection::new(write_closable, read_closable)
                });

                match new_client {
                    Ok(client) => {
                        let messages = inside_messages.clone();
                        let mut client = Some(client);
                        tasks_pool.spawn(Box::new(move || {
                            if let Some(client) = client.take() {
                                for rq in client {
                                    messages.push(rq.into());
                                }
                            }
                        }));
                    },

                    Err(e) => {
                        inside_messages.push(e.into());
                        break;
                    }
                }
            }
        });

        // result
        Ok(Server {
            messages: messages,
            close: close_trigger,
            listening_addr: local_addr,
        })
    }

    /// Returns an iterator for all the incoming requests.
    ///
    /// The iterator will return `None` if the server socket is shutdown.
    #[inline]
    pub fn incoming_requests(&self) -> IncomingRequests {
        IncomingRequests { server: self }
    }

    /// Returns the address the server is listening to.
    #[inline]
    pub fn server_addr(&self) -> net::SocketAddr {
        self.listening_addr.clone()
    }

    /// Returns the number of clients currently connected to the server.
    pub fn num_connections(&self) -> usize {
        unimplemented!()
        //self.requests_receiver.lock().len()
    }

    /// Blocks until an HTTP request has been submitted and returns it.
    pub fn recv(&self) -> IoResult<Request> {
        loop {
            match self.messages.pop() {
                Message::Error(err) => return Err(err),
                Message::NewRequest(rq) => return Ok(rq),
            }
        }
    }

    /// Same as `recv()` but doesn't block.
    pub fn try_recv(&self) -> IoResult<Option<Request>> {
        loop {
            match self.messages.try_pop() {
                Some(Message::Error(err)) => return Err(err),
                Some(Message::NewRequest(rq)) => return Ok(Some(rq)),
                None => return Ok(None)
            }
        }
    }
}

impl<'a> Iterator for IncomingRequests<'a> {
    type Item = Request;
    fn next(&mut self) -> Option<Request> {
        self.server.recv().ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.close.store(true, Relaxed);
    }
}
