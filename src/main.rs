use anyhow::bail;
use bytes::{Buf, BufMut, BytesMut};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use futures::future::BoxFuture;
use futures::stream::SplitSink;
use futures::task::waker_ref;
use futures::{AsyncWrite, FutureExt, SinkExt, StreamExt};
use std::any;
use std::future::Future;
use std::io::{Bytes, Cursor};
use std::ops::DerefMut;
use std::pin::Pin;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
    loop {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let _ = handle_socket(socket).await;
        });
    }
}

const MAX_MESSAGES: usize = 100;

struct FrameWrite<'a> {
    future: Mutex<BoxFuture<'a, Result<(), tokio_tungstenite::tungstenite::Error>>>,
}

trait MessageEncoder {
    fn encode<T: BufMut>(&self, target: &mut T) -> anyhow::Result<usize>;
    fn decode(buff: &mut Cursor<&[u8]>) -> anyhow::Result<Self>
    where
        Self: Sized;
}

trait MessageWriter {
    fn write_message(
        &mut self,
        message: Message,
    ) -> anyhow::Result<futures::sink::Send<'_, Self, tokio_tungstenite::tungstenite::Message>>;
}

enum Message {
    Ping(PingPayload),
    Pong(PongPayload)
}

impl Message {
    pub fn encode(&self, buff: &mut BytesMut) -> anyhow::Result<usize> {
        let buffer_size = match &self {
            Message::Ping(payload) => payload.encode(buff)?,
            Message::Pong(payload) => payload.encode(buff)?,
        };

        Ok(buffer_size)
    }
}

impl MessageWriter for SinkStream {
    fn write_message(
        &mut self,
        message: Message,
    ) -> anyhow::Result<futures::sink::Send<'_, Self, tokio_tungstenite::tungstenite::Message>>
    {
        use tokio_tungstenite::tungstenite::Message;

        let mut buffer = bytes::BytesMut::new();
        message.encode(&mut buffer)?;

        Ok(self.send(Message::Binary(Vec::from(&buffer[..]))))
    }
}

#[derive(Debug)]
struct PingPayload {
    timestamp: DateTime<Utc>,
}

#[derive(Debug)]
struct PongPayload {
    timestamp: DateTime<Utc>,
}

impl MessageEncoder for PongPayload {
    fn encode<T: BufMut>(&self, target: &mut T) -> anyhow::Result<usize> {

        target.put_u16(0x17);
        target.put_i64(self.timestamp.timestamp_millis());

        Ok(0)
    }

    fn decode(buff: &mut Cursor<&[u8]>) -> anyhow::Result<Self>
    where
        Self: Sized {

        let data = buff.get_i64(); // TODO: abstract this to prevent panics.
        let timestamp = NaiveDateTime::from_timestamp_millis(data).unwrap();
        let timestamp: DateTime<Utc> = DateTime::from_naive_utc_and_offset(timestamp, Utc);

        Ok(Self { timestamp })
    }
}

impl MessageEncoder for PingPayload {
    fn encode<T: BufMut>(&self, target: &mut T) -> anyhow::Result<usize> {
        target.put_u16(0x17);
        target.put_i64(self.timestamp.timestamp_millis());

        Ok(0)
    }

    fn decode(buff: &mut Cursor<&[u8]>) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        let data = buff.get_i64(); // TODO: abstract this to prevent panics.
        let timestamp = NaiveDateTime::from_timestamp_millis(data).unwrap();
        let timestamp: DateTime<Utc> = DateTime::from_naive_utc_and_offset(timestamp, Utc);

        Ok(Self { timestamp })
    }
}

impl TryFrom<Cursor<&[u8]>> for Message {
    type Error = anyhow::Error;

    fn try_from(mut value: Cursor<&[u8]>) -> Result<Self, Self::Error> {
        let message_type = value.get_u16();
        let decoded_msg = match message_type {
            0x15 => Message::Ping(PingPayload::decode(&mut value)?),
            actual => {
                eprintln!("invalid frame type: {}", actual);
                bail!("invalid frame type")
            }
        };

        match value.get_ref().is_empty() {
            true => Ok(decoded_msg),
            false => bail!("buffer not consumed"),
        }
    }
}

async fn handle_socket(socket: TcpStream) -> anyhow::Result<()> {
    let websocket_stream = tokio_tungstenite::accept_async(socket).await?;
    let (mut writer, mut read) = websocket_stream.split();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(MAX_MESSAGES);

    tokio::spawn(async move {
        use tokio_tungstenite::tungstenite::Message;
        loop {
            let msg = match read.next().await {
                Some(msg) => msg?,
                None => {
                    eprintln!("failed to accept new message");
                    break;
                }
            };

            match msg {
                Message::Binary(data) => {
                    println!("received new message from client");
                    sender.send(data).await?;
                }
                Message::Text(_) => {
                    eprintln!("server cannot receive text data.");
                    break;
                }
                _ => {
                    unreachable!()
                }
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    loop {
        let maybe_msg = receiver.recv().await;
        let maybe_msg = match maybe_msg {
            None => {
                println!("received None from client sender");
                break;
            }
            Some(msg) => msg,
        };

        let cursor = Cursor::new(&maybe_msg[..]);
        let frame = Message::try_from(cursor)?;

        match frame {
            Message::Ping(payload) => handle_ping(payload, &mut writer).await?,
            Message::Pong(_) => {
                eprintln!("received non-expected frame: Pong");
                continue;
            }
        }
    }

    Ok(())
}

type SinkStream = SplitSink<WebSocketStream<TcpStream>, tokio_tungstenite::tungstenite::Message>;

async fn handle_ping(_: PingPayload, writer: &mut SinkStream) -> anyhow::Result<()> {
    let payload = PongPayload { timestamp: Utc::now() };
    writer.write_message(Message::Pong(payload))?.await?;

    Ok(())
}
