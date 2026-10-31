// src/osc/server.rs — OSC сервер для удаленного управления шоу

use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use rosc::{OscMessage, OscPacket, OscType};
use std::net::UdpSocket;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct OscEvent {
    pub addr: String,
    pub args: Vec<OscType>,
}

#[allow(dead_code)]
pub struct OscServer {
    pub event_rx: Receiver<OscEvent>,
    pub send_tx: Sender<OscEvent>,
    pub listen_port: u16,
    pub send_addr: String,
}

impl OscServer {
    pub fn new(listen_port: u16, send_addr: String) -> Result<Self> {
        let (event_tx, event_rx) = bounded::<OscEvent>(256);
        let (send_tx, send_rx) = bounded::<OscEvent>(256);

        let listen_addr = format!("0.0.0.0:{listen_port}");
        let socket = UdpSocket::bind(&listen_addr)?;
        socket.set_nonblocking(true)?;
        info!("OSC listening on {listen_addr}");

        let send_addr_clone = send_addr.clone();

        {
            let socket_recv = socket.try_clone()?;
            let tx = event_tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match socket_recv.recv_from(&mut buf) {
                        Ok((size, _addr)) => {
                            if let Ok(packet) = rosc::decoder::decode_udp(&buf[..size]) {
                                flatten_packet(packet.1, &tx);
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(e) => {
                            error!("OSC recv error: {e}");
                        }
                    }
                }
            });
        }

        {
            let send_socket = UdpSocket::bind("0.0.0.0:0")?;
            std::thread::spawn(move || {
                while let Ok(event) = send_rx.recv() {
                    let msg = OscPacket::Message(OscMessage {
                        addr: event.addr,
                        args: event.args,
                    });
                    if let Ok(bytes) = rosc::encoder::encode(&msg) {
                        if let Err(e) = send_socket.send_to(&bytes, &send_addr_clone) {
                            warn!("OSC send error: {e}");
                        }
                    }
                }
            });
        }

        Ok(Self {
            event_rx,
            send_tx,
            listen_port,
            send_addr,
        })
    }

    pub fn drain(&self) -> Vec<OscEvent> {
        let mut out = Vec::new();
        while let Ok(e) = self.event_rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[allow(dead_code)]
    pub fn send(&self, addr: &str, args: Vec<OscType>) {
        let _ = self.send_tx.try_send(OscEvent {
            addr: addr.to_string(),
            args,
        });
    }
}

fn flatten_packet(packet: OscPacket, tx: &Sender<OscEvent>) {
    match packet {
        OscPacket::Message(msg) => {
            let _ = tx.try_send(OscEvent {
                addr: msg.addr,
                args: msg.args,
            });
        }
        OscPacket::Bundle(bundle) => {
            for p in bundle.content {
                flatten_packet(p, tx);
            }
        }
    }
}