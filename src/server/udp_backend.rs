use std::{net::SocketAddr, time::Duration};

use bytes::BytesMut;
use mio::{event::Event, net::UdpSocket, Poll};

use crate::{
    config::OPTIONS,
    proto::{UdpAssociate, UdpParseResult, MAX_PACKET_SIZE},
    server::tls_server::Backend,
    status::{ConnStatus, StatusProvider},
    tls_conn::TlsConn,
};

pub struct UdpBackend {
    socket: UdpSocket,
    send_buffer: BytesMut,
    recv_body: Vec<u8>,
    recv_head: BytesMut,
    index: usize,
    status: ConnStatus,
    timeout: Duration,
    bytes_read: usize,
    bytes_sent: usize,
    remote_addr: SocketAddr,
}

impl UdpBackend {
    pub fn new(socket: UdpSocket, index: usize) -> UdpBackend {
        let remote_addr = socket.local_addr().unwrap();
        UdpBackend {
            socket,
            index,
            remote_addr,
            send_buffer: Default::default(),
            recv_body: vec![0u8; MAX_PACKET_SIZE],
            recv_head: Default::default(),
            status: ConnStatus::Established,
            timeout: OPTIONS.udp_idle_duration,
            bytes_read: 0,
            bytes_sent: 0,
        }
    }

    fn do_send(&mut self, mut buffer: &[u8]) {
        loop {
            match UdpAssociate::parse(buffer) {
                UdpParseResult::Packet(packet) => {
                    match self
                        .socket
                        .send_to(&packet.payload[..packet.length], packet.address)
                    {
                        Ok(size) => {
                            self.bytes_sent += size;
                            if size != packet.length {
                                log::error!(
                                    "connection:{} udp packet is truncated, {}：{}",
                                    self.index,
                                    packet.length,
                                    size
                                );
                                self.shutdown();
                                return;
                            }
                            log::debug!(
                                "connection:{} write {} bytes to udp target:{}",
                                self.index,
                                size,
                                packet.address
                            );
                            buffer = &packet.payload[packet.length..];
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            log::debug!("connection:{} write to udp target blocked", self.index);
                            self.send_buffer.extend_from_slice(buffer);
                            break;
                        }
                        Err(err) => {
                            log::warn!(
                                "connection:{} send_to {} failed:{}",
                                self.index,
                                packet.address,
                                err
                            );
                            self.shutdown();
                            return;
                        }
                    }
                }
                UdpParseResult::InvalidProtocol => {
                    log::error!("connection:{} got invalid udp protocol", self.index);
                    self.shutdown();
                    return;
                }
                UdpParseResult::Continued => {
                    log::trace!("connection:{} got partial request", self.index);
                    self.send_buffer.extend_from_slice(buffer);
                    break;
                }
            }
        }
    }

    fn do_read(&mut self, conn: &mut TlsConn) {
        loop {
            match self.socket.recv_from(self.recv_body.as_mut_slice()) {
                Ok((size, addr)) => {
                    self.remote_addr = addr;
                    self.bytes_read += size;
                    log::debug!(
                        "connection:{} got {} bytes udp data from:{}",
                        self.index,
                        size,
                        addr
                    );
                    self.recv_head.clear();
                    UdpAssociate::generate(&mut self.recv_head, &addr, size as u16);
                    if !conn.write_session(self.recv_head.as_ref()) {
                        break;
                    }
                    if !conn.write_session(&self.recv_body.as_slice()[..size]) {
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    log::debug!("connection:{} write to session blocked", self.index);
                    break;
                }
                Err(err) => {
                    log::warn!("connection:{} got udp read err:{}", self.index, err);
                    self.shutdown();
                    break;
                }
            }
        }
        conn.do_send();
    }
}

impl Backend for UdpBackend {
    fn ready(&mut self, event: &Event, conn: &mut TlsConn) {
        if event.is_readable() {
            self.do_read(conn);
        }
        if event.is_writable() {
            self.dispatch(&[]);
        }
    }

    fn dispatch(&mut self, buffer: &[u8]) {
        if self.send_buffer.is_empty() {
            self.do_send(buffer);
        } else {
            self.send_buffer.extend_from_slice(buffer);
            let buffer = self.send_buffer.split();
            self.do_send(buffer.as_ref());
        }
    }

    fn get_timeout(&self) -> Duration {
        self.timeout
    }
}

impl StatusProvider for UdpBackend {
    fn set_status(&mut self, status: ConnStatus) {
        self.status = status;
    }

    fn get_status(&self) -> ConnStatus {
        self.status
    }

    fn close_conn(&self) {}

    fn deregister(&mut self, poll: &Poll) {
        let _ = poll.registry().deregister(&mut self.socket);
    }

    fn finish_send(&mut self) -> bool {
        self.send_buffer.is_empty()
    }
}
