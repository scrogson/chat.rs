extern crate byteorder;
extern crate http_muncher;
extern crate mio;
extern crate rustc_serialize;
extern crate sha1;

mod frame;

use frame::{OpCode, WebSocketFrame};
use http_muncher::{Parser, ParserHandler};
use mio::*;
use mio::tcp::*;
use rustc_serialize::base64::{ToBase64, STANDARD};

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::rc::Rc;


const SERVER_TOKEN: Token = Token(0);

fn gen_key(key: &String) -> String {
    let mut sha = sha1::Sha1::new();
    let mut buf = [0u8; 20];

    sha.update(key.as_bytes());
    sha.update("258EAFA5-E914-47DA-95CA-C5AB0DC85B11".as_bytes());
    sha.output(&mut buf);

    buf.to_base64(STANDARD)
}

struct HttpParser {
    current_key: Option<String>,
    headers: Rc<RefCell<HashMap<String, String>>>
}

impl ParserHandler for HttpParser {
    fn on_header_field(&mut self, s: &[u8]) -> bool {
        self.current_key = Some(std::str::from_utf8(s).unwrap().to_string());
        true
    }

    fn on_header_value(&mut self, s: &[u8]) -> bool {
        self.headers.borrow_mut()
            .insert(self.current_key.clone().unwrap(),
                    std::str::from_utf8(s).unwrap().to_string());
        true
    }

    fn on_headers_complete(&mut self) -> bool {
        false
    }
}

enum ClientState {
    AwaitingHandshake(RefCell<Parser<HttpParser>>),
    HandshakeResponse,
    Connected
}

struct WebSocketClient {
    socket: TcpStream,
    headers: Rc<RefCell<HashMap<String, String>>>,
    interest: EventSet,
    state: ClientState,
    outgoing: Vec<WebSocketFrame>
}

impl WebSocketClient {
    fn new(socket: TcpStream) -> WebSocketClient {
        let headers = Rc::new(RefCell::new(HashMap::new()));

        WebSocketClient {
            socket: socket,
            headers: headers.clone(),
            interest: EventSet::readable(),
            outgoing: Vec::new(),
            state: ClientState::AwaitingHandshake(RefCell::new(Parser::request(HttpParser {
                current_key: None,
                headers: headers.clone()
            })))

        }
    }

    fn read(&mut self) {
        match self.state {
            ClientState::AwaitingHandshake(_) => self.read_handshake(),
            ClientState::Connected => self.read_frame(),
            _ => {}
        }
    }

    fn read_frame(&mut self) {
        let frame = WebSocketFrame::read(&mut self.socket);
        match frame {
            Ok(frame) => {
                match frame.get_opcode() {
                    OpCode::TextFrame => {
                        println!("{:?}", frame);
                        let reply_frame = WebSocketFrame::from("hi there!");
                        self.outgoing.push(reply_frame);
                    },
                    OpCode::Ping => {
                        println!("ping/pong");
                        self.outgoing.push(WebSocketFrame::pong(&frame));
                    },
                    OpCode::ConnectionClose => {
                        self.outgoing.push(WebSocketFrame::close_from(&frame));
                    },
                    _ => {}
                }
                self.interest.remove(EventSet::readable());
                self.interest.insert(EventSet::writable());
            },
            Err(e) => println!("error while reading frame: {}", e)
        }
    }

    fn read_handshake(&mut self) {
        loop {
            let mut buf = [0; 2048];
            match self.socket.try_read(&mut buf) {
                Ok(None) => break, // Socket buffer has got no more bytes.
                Ok(Some(_len)) => {
                    let is_upgrade = if let ClientState::AwaitingHandshake(ref parser_state) = self.state {
                        let mut parser = parser_state.borrow_mut();
                        parser.parse(&buf);
                        parser.is_upgrade()
                    } else { false };

                    if is_upgrade {
                        self.state = ClientState::HandshakeResponse;
                        self.interest.remove(EventSet::readable());
                        self.interest.insert(EventSet::writable());
                        break;
                    }
                },
                Err(e) => {
                    println!("Error while reading socket: {:?}", e);
                    return
                }
            }
        }
    }

    fn write(&mut self) {
        match self.state {
            ClientState::HandshakeResponse => self.write_handshake(),
            ClientState::Connected => {
                println!("sending {} frames", self.outgoing.len());

                let mut close_connection = false;

                for frame in self.outgoing.iter() {
                    if let Err(e) = frame.write(&mut self.socket) {
                        println!("error on write: {}", e);
                    }

                    if frame.is_close() {
                        close_connection = true;
                    }
                }

                self.outgoing.clear();
                self.interest.remove(EventSet::writable());

                if close_connection {
                    self.interest.insert(EventSet::hup());
                } else {
                    self.interest.insert(EventSet::readable());
                }
            },
            _ => {}
        }
    }

    fn write_handshake(&mut self) {
        let headers = self.headers.borrow();
        let response_key = gen_key(&headers.get("Sec-WebSocket-Key").unwrap());
        let response = fmt::format(format_args!("HTTP/1.1 101 Switching Protocols\r\n\
                                                 Connection: Upgrade\r\n\
                                                 Sec-WebSocket-Accept: {}\r\n\
                                                 Upgrade: websocket\r\n\r\n", response_key));

        self.socket.try_write(response.as_bytes()).unwrap();
        self.state = ClientState::Connected;
        self.interest.remove(EventSet::writable());
        self.interest.insert(EventSet::readable());
    }
}

struct WebSocketServer {
    socket: TcpListener,
    clients: HashMap<Token, WebSocketClient>,
    token_counter: usize
}

impl Handler for WebSocketServer {
    type Timeout = usize;
    type Message = ();

    fn ready(&mut self, event_loop: &mut EventLoop<WebSocketServer>, token: Token, events: EventSet) {
        if events.is_readable() {
            match token {
                SERVER_TOKEN => {
                    let client_socket = match self.socket.accept() {
                        Ok(Some((sock, _addr))) => sock,
                        Ok(None) => unreachable!("Accept has returned 'None'"),
                        Err(e) => {
                            println!("Accept error: {}", e);
                            return;
                        }
                    };
                    let new_token = Token(self.token_counter);
                    self.clients.insert(new_token, WebSocketClient::new(client_socket));
                    self.token_counter += 1;

                    event_loop.register(&self.clients[&new_token].socket, new_token, EventSet::readable(),
                                        PollOpt::edge() | PollOpt::oneshot()).unwrap();
                },
                token => {
                    let mut client = self.clients.get_mut(&token).unwrap();
                    client.read();
                    event_loop.reregister(&client.socket, token, client.interest,
                                          PollOpt::edge() | PollOpt::oneshot()).unwrap();
                }
            }
        }

        if events.is_writable() {
            let mut client = self.clients.get_mut(&token).unwrap();
            client.write();
            event_loop.reregister(&client.socket, token, client.interest,
                                  PollOpt::edge() | PollOpt::oneshot()).unwrap();
        }

        if events.is_hup() {
            let client = self.clients.remove(&token).unwrap();
            client.socket.shutdown(Shutdown::Both);
            event_loop.deregister(&client.socket);
        }
    }
}

fn main() {
    let address = "127.0.0.1:10000".parse::<SocketAddr>().unwrap();
    let server_socket = TcpListener::bind(&address).unwrap();

    let mut event_loop = EventLoop::new().unwrap();
    let mut server = WebSocketServer {
        token_counter: 1,
        clients: HashMap::new(),
        socket: server_socket
    };

    event_loop.register(&server.socket,
                        SERVER_TOKEN,
                        EventSet::readable(),
                        PollOpt::edge()).unwrap();
    event_loop.run(&mut server).unwrap();
}
