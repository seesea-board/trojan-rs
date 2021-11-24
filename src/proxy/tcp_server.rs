use std::{
    collections::HashMap,
    io::ErrorKind,
    net::{Shutdown, SocketAddr},
    time::Instant,
};

use bytes::BytesMut;
use mio::{
    event::Event,
    net::{TcpListener, TcpStream},
    Interest, Poll, Token,
};

use crate::{
    config::OPTIONS,
    proto::{TrojanRequest, CONNECT, MAX_PACKET_SIZE},
    proxy::{idle_pool::IdlePool, next_index, CHANNEL_CLIENT, CHANNEL_CNT, CHANNEL_TCP, MIN_INDEX},
    resolver::DnsResolver,
    status::{ConnStatus, StatusProvider},
    sys, tcp_util,
    tls_conn::TlsConn,
    types::{Result, TrojanError},
};

pub struct TcpServer {
    tcp_listener: TcpListener,
    conns: HashMap<usize, Connection>,
    next_id: usize,
}

struct Connection {
    index: usize,
    dst_addr: SocketAddr,
    client: TcpStream,
    recv_buffer: Vec<u8>,
    send_buffer: BytesMut,
    status: ConnStatus,
    client_time: Instant,
    server_conn: TlsConn,
    last_active_time: Instant,
}

impl TcpServer {
    pub fn new(tcp_listener: TcpListener) -> TcpServer {
        TcpServer {
            tcp_listener,
            conns: HashMap::new(),
            next_id: MIN_INDEX,
        }
    }

    pub fn accept(&mut self, poll: &Poll, pool: &mut IdlePool, resolver: &DnsResolver) {
        loop {
            if let Err(err) = self.accept_once(poll, pool, resolver) {
                if let TrojanError::StdIoError(err) = &err {
                    if err.kind() == ErrorKind::WouldBlock {
                        break;
                    }
                }
                log::error!("tcp server accept failed:{}", err);
            }
        }
    }

    fn accept_once(
        &mut self,
        poll: &Poll,
        pool: &mut IdlePool,
        resolver: &DnsResolver,
    ) -> Result<()> {
        let (client, src_addr) = self.tcp_listener.accept()?;
        sys::set_mark(&client, OPTIONS.marker)?;
        client.set_nodelay(true)?;
        let dst_addr = sys::get_oridst_addr(&client)?;
        log::info!("got new connection from:{} to:{}", src_addr, dst_addr);
        if let Some(mut conn) = pool.get(poll, resolver) {
            let index = next_index(&mut self.next_id);
            if !conn.reset_index(index, Token(index * CHANNEL_CNT + CHANNEL_TCP), poll) {
                conn.check_status(poll);
            } else {
                let mut conn = Connection::new(index, conn, dst_addr, client);
                if conn.setup(poll) {
                    self.conns.insert(conn.index(), conn);
                } else {
                    conn.destroy(poll);
                }
            }
        } else {
            log::error!("alloc new connection failed")
        }
        Ok(())
    }

    pub fn ready(&mut self, event: &Event, poll: &Poll) {
        let index = Connection::token2index(event.token());
        if let Some(conn) = self.conns.get_mut(&index) {
            conn.ready(event, poll);
            if conn.destroyed() {
                log::debug!("connection:{} removed from list", index);
                self.conns.remove(&index);
            }
        } else {
            log::error!("tcp connection:{} not found, check deregister", index)
        }
    }

    pub fn check_timeout(&mut self, poll: &Poll, now: Instant) {
        for conn in self.conns.values_mut() {
            if conn.timeout(now) {
                conn.destroy(poll);
            }
        }
    }
}

impl Connection {
    fn new(
        index: usize,
        server_conn: TlsConn,
        dst_addr: SocketAddr,
        client: TcpStream,
    ) -> Connection {
        Connection {
            index,
            dst_addr,
            client,
            server_conn,
            status: ConnStatus::Established,
            send_buffer: BytesMut::new(),
            recv_buffer: vec![0u8; MAX_PACKET_SIZE],
            client_time: Instant::now(),
            last_active_time: Instant::now(),
        }
    }

    fn timeout(&self, now: Instant) -> bool {
        now - self.last_active_time > OPTIONS.tcp_idle_duration
    }

    fn destroyed(&self) -> bool {
        self.deregistered() && self.server_conn.deregistered()
    }

    fn destroy(&mut self, poll: &Poll) {
        self.shutdown();
        self.server_conn.shutdown();
        self.check_status(poll);
        self.server_conn.check_status(poll);
    }

    fn setup(&mut self, poll: &Poll) -> bool {
        let mut request = BytesMut::new();
        TrojanRequest::generate(&mut request, CONNECT, &self.dst_addr);
        let token = self.client_token();
        if !self.server_conn.write_session(request.as_ref()) {
            false
        } else if let Err(err) = poll.registry().register(
            &mut self.client,
            token,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            log::warn!("connection:{} register client failed:{}", self.index(), err);
            false
        } else {
            true
        }
    }

    fn index(&self) -> usize {
        self.index
    }

    fn token2index(token: Token) -> usize {
        token.0 / CHANNEL_CNT
    }

    fn ready(&mut self, event: &Event, poll: &Poll) {
        self.last_active_time = Instant::now();
        match event.token().0 % CHANNEL_CNT {
            CHANNEL_CLIENT => {
                if event.is_readable() {
                    self.try_read_client();
                }
                if event.is_writable() {
                    self.try_send_client(&[]);
                }
            }
            CHANNEL_TCP => {
                if event.is_readable() {
                    self.try_read_server();
                }
                if event.is_writable() {
                    self.try_send_server();
                }
            }
            _ => {
                log::error!("invalid token found in tcp listener");
                self.shutdown();
            }
        }
        if self.is_shutdown() {
            self.server_conn.peer_closed();
        }
        if self.server_conn.is_shutdown() {
            self.peer_closed();
        }
        self.check_status(poll);
        self.server_conn.check_status(poll);
    }

    fn client_token(&self) -> Token {
        Token(self.index * CHANNEL_CNT + CHANNEL_CLIENT)
    }

    fn try_read_client(&mut self) {
        if !tcp_util::tcp_read(
            self.index,
            &self.client,
            &mut self.recv_buffer,
            &mut self.server_conn,
        ) {
            self.shutdown();
        }

        self.try_send_server();
    }

    fn try_send_client(&mut self, buffer: &[u8]) {
        if self.send_buffer.is_empty() {
            self.do_send_client(buffer);
        } else {
            self.send_buffer.extend_from_slice(buffer);
            let buffer = self.send_buffer.split();
            self.do_send_client(buffer.as_ref());
        }
    }

    fn do_send_client(&mut self, data: &[u8]) {
        if !tcp_util::tcp_send(self.index, &self.client, &mut self.send_buffer, data) {
            self.shutdown();
            return;
        }
        if let ConnStatus::Shutdown = self.status {
            if self.send_buffer.is_empty() {
                self.shutdown();
                log::debug!("connection:{} is closing for no data to send", self.index());
            }
        }
    }

    fn try_read_server(&mut self) {
        if let Some(buffer) = self.server_conn.do_read() {
            self.try_send_client(buffer.as_slice());
        }
    }

    fn try_send_server(&mut self) {
        self.server_conn.do_send();
    }
}

impl StatusProvider for Connection {
    fn set_status(&mut self, status: ConnStatus) {
        self.status = status;
    }
    fn get_status(&self) -> ConnStatus {
        self.status
    }

    fn close_conn(&self) {
        let _ = self.client.shutdown(Shutdown::Both);
    }

    fn deregister(&mut self, poll: &Poll) {
        let _ = poll.registry().deregister(&mut self.client);
    }

    fn finish_send(&mut self) -> bool {
        self.send_buffer.is_empty()
    }
}
