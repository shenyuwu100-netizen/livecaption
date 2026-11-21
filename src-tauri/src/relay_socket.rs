#[cfg(test)]
use futures_util::SinkExt;
use futures_util::StreamExt;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};
use tokio_util::sync::CancellationToken;

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub enum Packet {
    Message(Message),
    Idle,
    HttpFallback,
}

pub struct RelaySocket {
    socket: Option<Socket>,
    retry_at: Instant,
    last_received: Instant,
    url: &'static str,
    session: Option<String>,
    pending: VecDeque<Message>,
    #[cfg(test)]
    report_id: u64,
}

impl RelaySocket {
    pub fn new(url: &'static str, session: Option<String>) -> Self {
        Self {
            socket: None,
            retry_at: Instant::now(),
            last_received: Instant::now(),
            url,
            session,
            pending: VecDeque::new(),
            #[cfg(test)]
            report_id: 0,
        }
    }

    pub fn connected(&self) -> bool {
        self.socket.is_some()
    }

    pub fn disconnect(&mut self) {
        self.socket = None;
        self.pending.clear();
    }

    // Confirm storage on the server before marking telemetry as synchronized.
    // State pushes received before the ACK remain available to the command loop.
    #[cfg(test)]
    pub async fn report(
        &mut self,
        body: &serde_json::Value,
        cancel: &CancellationToken,
    ) -> Option<u16> {
        let socket = self.socket.as_mut()?;
        self.report_id += 1;
        let id = self.report_id;
        let message = serde_json::json!({"type":"desktopReport","id":id,"body":body});
        let exchange = async {
            socket
                .send(Message::Text(message.to_string().into()))
                .await
                .ok()?;
            loop {
                let message = socket.next().await?.ok()?;
                if message.is_close() {
                    return None;
                }
                self.last_received = Instant::now();
                if let Message::Text(text) = &message {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                        if value["type"] == "desktopAck" && value["id"].as_u64() == Some(id) {
                            return value["status"].as_u64().and_then(|s| u16::try_from(s).ok());
                        }
                        if value["type"] == "revoked" {
                            return None;
                        }
                    }
                }
                if self.pending.len() >= 32 {
                    self.pending.pop_front();
                }
                self.pending.push_back(message);
            }
        };
        let result = tokio::select! {
            _=cancel.cancelled()=>None,
            result=tokio::time::timeout(Duration::from_secs(2),exchange)=>result.ok().flatten(),
        };
        if result.is_none() {
            self.disconnect();
        }
        result
    }

    pub async fn read(&mut self, token: &str, cancel: &CancellationToken) -> Packet {
        if let Some(message) = self.pending.pop_front() {
            return Packet::Message(message);
        }
        if self.socket.is_none() && Instant::now() >= self.retry_at {
            self.retry_at = Instant::now() + Duration::from_secs(30);
            if let Ok(mut request) = self.url.into_client_request() {
                if let Ok(auth) = format!("Bearer {token}").parse() {
                    request.headers_mut().insert("Authorization", auth);
                    if let Some(session) = &self.session {
                        if let Ok(value) = session.parse() {
                            request.headers_mut().insert("X-LiveCaption-Session", value);
                        }
                    }
                    let attempt = tokio::time::timeout(
                        Duration::from_secs(5),
                        tokio_tungstenite::connect_async(request),
                    );
                    let result = tokio::select! { _=cancel.cancelled()=>return Packet::Idle, result=attempt=>result };
                    if let Ok(Ok((socket, _))) = result {
                        self.socket = Some(socket);
                        self.last_received = Instant::now();
                    }
                }
            }
        }
        let Some(socket) = &mut self.socket else {
            return Packet::HttpFallback;
        };
        let result = tokio::select! {
            _=cancel.cancelled()=>return Packet::Idle,
            result=tokio::time::timeout(Duration::from_millis(100), socket.next())=>result,
        };
        match result {
            Ok(Some(Ok(message))) if !message.is_close() => {
                self.last_received = Instant::now();
                Packet::Message(message)
            }
            Err(_) if self.last_received.elapsed() < Duration::from_secs(14) => Packet::Idle,
            _ => {
                self.socket = None;
                Packet::HttpFallback
            }
        }
    }
}

pub fn audio_frame(bytes: &[u8]) -> Option<(u64, Vec<u8>)> {
    let pcm_size = bytes.len().checked_sub(8)?;
    if !(3200..=32000).contains(&pcm_size) || pcm_size % 2 != 0 {
        return None;
    }
    let sequence = u64::from_be_bytes(bytes[..8].try_into().ok()?);
    if sequence == 0 || sequence > 9_007_199_254_740_991 {
        return None;
    }
    Some((sequence, bytes[8..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reports_wait_for_ack_preserve_commands_and_fall_back_on_disconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for status in [200, 409] {
                let message = socket.next().await.unwrap().unwrap();
                let report: serde_json::Value =
                    serde_json::from_str(message.to_text().unwrap()).unwrap();
                assert_eq!(report["type"], "desktopReport");
                assert_eq!(report["body"]["kind"], "audioStatus");
                if status == 200 {
                    socket
                        .send(Message::Text(
                            r#"{"type":"state","state":{"commandId":7}}"#.into(),
                        ))
                        .await
                        .unwrap();
                }
                let ack =
                    serde_json::json!({"type":"desktopAck","id":report["id"],"status":status});
                socket
                    .send(Message::Text(ack.to_string().into()))
                    .await
                    .unwrap();
            }
            socket.next().await.unwrap().unwrap();
            socket.close(None).await.unwrap();
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let mut feed = RelaySocket::new("ws://localhost/unused", None);
        feed.socket = Some(socket);
        let cancel = CancellationToken::new();
        let body = serde_json::json!({"kind":"audioStatus"});
        assert_eq!(feed.report(&body, &cancel).await, Some(200));
        let Packet::Message(message) = feed.read("unused", &cancel).await else {
            panic!("state push lost before ACK");
        };
        assert!(message.to_text().unwrap().contains("commandId"));
        assert_eq!(feed.report(&body, &cancel).await, Some(409));
        assert!(
            feed.connected(),
            "rejected stale reports keep the command channel open"
        );
        assert_eq!(feed.report(&body, &cancel).await, None);
        assert!(
            !feed.connected(),
            "unconfirmed delivery enables HTTP fallback"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn report_timeout_drops_uncertain_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.next().await.unwrap().unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let mut feed = RelaySocket::new("ws://localhost/unused", None);
        feed.socket = Some(socket);
        assert_eq!(
            feed.report(
                &serde_json::json!({"kind":"audioStatus"}),
                &CancellationToken::new()
            )
            .await,
            None
        );
        assert!(!feed.connected());
        server.abort();
    }

    #[test]
    fn frames_preserve_sequence_and_pcm_and_reject_malformed_input() {
        let mut bytes = 42u64.to_be_bytes().to_vec();
        bytes.extend(vec![7u8; 16000]);
        let (sequence, pcm) = audio_frame(&bytes).unwrap();
        assert_eq!(sequence, 42);
        assert_eq!(pcm, vec![7u8; 16000]);
        assert!(audio_frame(&bytes[..9]).is_none());
        assert!(audio_frame(&bytes[..16007]).is_none());
        bytes[..8].fill(0);
        assert!(audio_frame(&bytes).is_none());
    }
}
