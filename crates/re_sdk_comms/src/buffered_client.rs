use std::net::SocketAddr;

use crossbeam::channel::{select, Receiver, Sender};

use re_log_types::LogMsg;

#[derive(Debug, PartialEq, Eq)]
struct FlushedMsg;

/// Sent to prematurely quit (before flushing).
#[derive(Debug, PartialEq, Eq)]
struct QuitMsg;

enum MsgMsg {
    LogMsg(LogMsg),
    SetAddr(SocketAddr),
    Flush,
}

enum PacketMsg {
    Packet(Vec<u8>),
    SetAddr(SocketAddr),
    Flush,
}

/// Send [`LogMsg`]es to a server.
///
/// The messages are encoded and sent on separate threads
/// so that calling [`Client::send`] is non-blocking.
pub struct Client {
    msg_tx: Sender<MsgMsg>,
    flushed_rx: Receiver<FlushedMsg>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new(crate::default_server_addr())
    }
}

impl Client {
    pub fn new(addr: SocketAddr) -> Self {
        // TODO(emilk): keep track of how much memory is in each pipe
        // and apply back-pressure to not use too much RAM.
        let (msg_tx, msg_rx) = crossbeam::channel::unbounded();
        let (packet_tx, packet_rx) = crossbeam::channel::unbounded();
        let (flushed_tx, flushed_rx) = crossbeam::channel::unbounded();
        let (encode_quit_tx, encode_quit_rx) = crossbeam::channel::unbounded();
        let (send_quit_tx, send_quit_rx) = crossbeam::channel::unbounded();

        std::thread::Builder::new()
            .name("msg_encoder".into())
            .spawn(move || {
                msg_encode(&msg_rx, &encode_quit_rx, &packet_tx);
                tracing::debug!("Shutting down msg encoder thread");
            })
            .expect("Failed to spawn thread");

        std::thread::Builder::new()
            .name("tcp_sender".into())
            .spawn(move || {
                tcp_sender(addr, &packet_rx, &send_quit_rx, &flushed_tx);
                tracing::debug!("Shutting down TCP sender thread");
            })
            .expect("Failed to spawn thread");

        ctrlc::set_handler(move || {
            tracing::debug!("Ctrl-C detected - Aborting before everything has been sent");
            encode_quit_tx.send(QuitMsg).ok();
            send_quit_tx.send(QuitMsg).ok();
        })
        .expect("Error setting Ctrl-C handler");

        Self { msg_tx, flushed_rx }
    }

    pub fn set_addr(&mut self, addr: SocketAddr) {
        self.msg_tx
            .send(MsgMsg::SetAddr(addr))
            .expect("msg_encoder should not shut down until we tell it to");
    }

    pub fn send(&mut self, log_msg: LogMsg) {
        tracing::trace!("Sending message…");
        self.msg_tx
            .send(MsgMsg::LogMsg(log_msg))
            .expect("msg_encoder should not shut down until we tell it to");
    }

    /// Stall untill all messages so far has been sent.
    pub fn flush(&mut self) {
        tracing::debug!("Flushing message queue…");

        self.msg_tx
            .send(MsgMsg::Flush)
            .expect("msg_encoder should not shut down until we tell it to");

        match self.flushed_rx.recv() {
            Ok(FlushedMsg) => {
                tracing::debug!("Flush complete.");
            }
            Err(_) => {
                // This can happen on Ctrl-C
                tracing::warn!("Failed to flush pipeline - not all messages were sent (Ctrl-C).");
            }
        }
    }
}

impl Drop for Client {
    /// Wait until everything has been sent.
    fn drop(&mut self) {
        self.flush();
        tracing::debug!("Sender has shut down.");
    }
}

fn msg_encode(
    msg_rx: &Receiver<MsgMsg>,
    quit_rx: &Receiver<QuitMsg>,
    packet_tx: &Sender<PacketMsg>,
) {
    loop {
        select! {
            recv(msg_rx) -> msg_msg => {
                if let Ok(msg_msg) = msg_msg {
                    let packet_msg = match msg_msg {
                        MsgMsg::LogMsg(log_msg) => {
                            let packet = crate::encode_log_msg(&log_msg);
                            tracing::trace!("Encoded message of size {}", packet.len());
                            PacketMsg::Packet(packet)
                        }
                        MsgMsg::SetAddr(new_addr) => PacketMsg::SetAddr(new_addr),
                        MsgMsg::Flush => PacketMsg::Flush,
                    };

                    packet_tx
                        .send(packet_msg)
                        .expect("tcp_sender thread should live longer");
                } else {
                    return; // channel has closed
                }
            }
            recv(quit_rx) -> _quit_msg => {
                return;
            }
        }
    }
}

fn tcp_sender(
    addr: SocketAddr,
    packet_rx: &Receiver<PacketMsg>,
    quit_rx: &Receiver<QuitMsg>,
    flushed_tx: &Sender<FlushedMsg>,
) {
    let mut tcp_client = crate::tcp_client::TcpClient::new(addr);

    loop {
        select! {
            recv(packet_rx) -> packet_msg => {
                if let Ok(packet_msg) = packet_msg {
                    match packet_msg {
                        PacketMsg::Packet(packet) => {
                            if send_until_success(&mut tcp_client, &packet, quit_rx) == Some(QuitMsg) {
                                return;
                            }
                        }
                        PacketMsg::SetAddr(new_addr) => {
                            tcp_client.set_addr(new_addr);
                        }
                        PacketMsg::Flush => {
                            tcp_client.flush();
                            flushed_tx
                                .send(FlushedMsg)
                                .expect("Main thread should still be alive");
                        }
                    }
                } else {
                    return; // channel has closed
                }
            }
            recv(quit_rx) -> _quit_msg => {
                return;
            }
        }
    }
}

fn send_until_success(
    tcp_client: &mut crate::tcp_client::TcpClient,
    packet: &[u8],
    quit_rx: &Receiver<QuitMsg>,
) -> Option<QuitMsg> {
    if let Err(err) = tcp_client.send(packet) {
        tracing::warn!("Failed to send message: {err}");

        let mut sleep_ms = 100;

        loop {
            select! {
                recv(quit_rx) -> _quit_msg => {
                    return Some(QuitMsg);
                }
                default(std::time::Duration::from_millis(sleep_ms)) => {
                    if let Err(new_err) = tcp_client.send(packet) {
                        if new_err.to_string() != err.to_string() {
                            tracing::warn!("Failed to send message: {err}");
                        }
                        sleep_ms = (sleep_ms * 2).min(3000);
                    } else {
                        return None;
                    }
                }
            }
        }
    } else {
        None
    }
}
