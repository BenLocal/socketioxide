//! ## Encoder for http payloads
//!
//! There is 3 different encoders:
//! * engine.io v4 encoder
//! * engine.io v3 encoder:
//!    * string encoder (used when there is no binary packet or when the client does not support binary)
//!    * binary encoder (used when there is binary packets and the client supports binary)
//!

use tokio::sync::MutexGuard;

use crate::{
    errors::Error, packet::Packet, peekable::PeekableReceiver, transport::polling::payload::Payload,
};

/// Try to immediately poll a new packet from the rx channel and check that the new packet can be added to the payload
///
/// Manually close the channel if the packet is a close packet
/// It will allow to notify the [`Socket`](crate::socket::Socket) that the session is closed
///
/// ## Arguments
/// * `rx` - The channel to poll
/// * `payload_len` - The current payload length
/// * `max_payload` - The maximum payload length
/// * `b64` - If binary packets should be encoded in base64
fn try_recv_packet(
    rx: &mut MutexGuard<'_, PeekableReceiver<Packet>>,
    payload_len: usize,
    max_payload: u64,
    b64: bool,
) -> Option<Packet> {
    if let Some(packet) = rx.peek() {
        if (payload_len + packet.get_size_hint(b64)) as u64 > max_payload {
            #[cfg(feature = "tracing")]
            tracing::debug!("payload too big, stopping encoding for this payload");
            return None;
        }
    }

    let packet = rx.try_recv().ok();

    if let Some(Packet::Close) = packet {
        #[cfg(feature = "tracing")]
        tracing::debug!("Received close packet, closing channel");
        rx.try_recv().ok();
        rx.close();
    }

    #[cfg(feature = "tracing")]
    tracing::debug!("sending packet: {:?}", packet);
    packet
}

/// Same as [`try_recv_packet`]
/// but wait for a new packet if there is no packet in the buffer
async fn recv_packet(rx: &mut MutexGuard<'_, PeekableReceiver<Packet>>) -> Result<Packet, Error> {
    let packet = rx.recv().await.ok_or(Error::Aborted)?;
    if packet == Packet::Close {
        #[cfg(feature = "tracing")]
        tracing::debug!("Received close packet, closing channel");
        rx.close();
    }

    #[cfg(feature = "tracing")]
    tracing::debug!("sending packet: {:?}", packet);
    Ok(packet)
}

/// Encode multiple packets into a string payload according to the
/// [engine.io v4 protocol](https://socket.io/fr/docs/v4/engine-io-protocol/#http-long-polling-1)
pub async fn v4_encoder(
    mut rx: MutexGuard<'_, PeekableReceiver<Packet>>,
    max_payload: u64,
) -> Result<Payload, Error> {
    use crate::transport::polling::payload::PACKET_SEPARATOR_V4;

    #[cfg(feature = "tracing")]
    tracing::debug!("encoding payload with v4 encoder");
    let mut data: String = String::new();

    // Send all packets in the buffer
    const PUNCTUATION_LEN: usize = 1;
    while let Some(packet) =
        try_recv_packet(&mut rx, data.len() + PUNCTUATION_LEN, max_payload, true)
    {
        let packet: String = packet.try_into()?;

        if !data.is_empty() {
            data.push(std::char::from_u32(PACKET_SEPARATOR_V4 as u32).unwrap());
        }
        data.push_str(&packet);
    }

    // If there is no packet in the buffer, wait for the next packet
    if data.is_empty() {
        let packet = recv_packet(&mut rx).await?;
        let packet: String = packet.try_into()?;
        data.push_str(&packet);
    }

    Ok(Payload::new(data, false))
}

/// Encode one packet into a *binary* payload according to the
/// [engine.io v3 protocol](https://github.com/socketio/engine.io-protocol/tree/v3#payload)
#[cfg(feature = "v3")]
pub fn v3_bin_packet_encoder(packet: Packet, data: &mut Vec<u8>) -> Result<(), Error> {
    use crate::transport::polling::payload::BINARY_PACKET_SEPARATOR_V3;
    match packet {
        Packet::BinaryV3(bin) => {
            data.push(0x1);

            let len = (bin.len() + 1).to_string();
            for char in len.chars() {
                data.push(char as u8 - 48);
            }
            data.push(BINARY_PACKET_SEPARATOR_V3); // separator
            data.push(0x04); // message packet type
            data.extend_from_slice(&bin); // raw data
        }
        packet => {
            let packet: String = packet.try_into()?;
            data.push(0x0); // 0 = string

            let len = packet.len().to_string();
            for char in len.chars() {
                data.push(char as u8 - 48);
            }
            data.push(BINARY_PACKET_SEPARATOR_V3); // separator
            data.extend_from_slice(packet.as_bytes()); // packet
        }
    };
    Ok(())
}

/// Encode one packet into a *string* payload according to the
/// [engine.io v3 protocol](https://github.com/socketio/engine.io-protocol/tree/v3#payload)
#[cfg(feature = "v3")]
pub fn v3_string_packet_encoder(packet: Packet, data: &mut Vec<u8>) -> Result<(), Error> {
    use crate::transport::polling::payload::STRING_PACKET_SEPARATOR_V3;
    let packet: String = packet.try_into()?;
    let packet = format!(
        "{}{}{}",
        packet.chars().count(),
        STRING_PACKET_SEPARATOR_V3 as char,
        packet
    );
    data.extend_from_slice(packet.as_bytes());
    Ok(())
}

/// Encode multiple packet packet into a *string* payload if there is no binary packet or into a *binary* payload if there is binary packets
/// according to the [engine.io v3 protocol](https://github.com/socketio/engine.io-protocol/tree/v3#payload)
#[cfg(feature = "v3")]
pub async fn v3_binary_encoder(
    mut rx: MutexGuard<'_, PeekableReceiver<Packet>>,
    max_payload: u64,
) -> Result<Payload, Error> {
    let mut data: Vec<u8> = Vec::new();
    let mut packet_buffer: Vec<Packet> = Vec::new();

    // estimated size of the `packet_buffer` in bytes
    let mut estimated_size: usize = 0;
    // number of digits of the max packet size, used to approximate the payload size
    let max_packet_size_len = max_payload.checked_ilog10().unwrap_or(0) as usize + 1;

    #[cfg(feature = "tracing")]
    tracing::debug!("encoding payload with v3 binary encoder");
    // buffer all packets to find if there is binary packets
    let mut has_binary = false;

    while let Some(packet) = try_recv_packet(&mut rx, estimated_size, max_payload, false) {
        if packet.is_binary() {
            has_binary = true;
        }

        const PUNCTUATION_LEN: usize = 2;
        estimated_size += packet.get_size_hint(false) + max_packet_size_len + PUNCTUATION_LEN;

        packet_buffer.push(packet);
    }

    if has_binary {
        for packet in packet_buffer {
            v3_bin_packet_encoder(packet, &mut data)?;
        }
    } else {
        for packet in packet_buffer {
            v3_string_packet_encoder(packet, &mut data)?;
        }
    }

    // If there is no packet in the buffer, wait for the next packet
    if data.is_empty() {
        let packet = recv_packet(&mut rx).await?;

        match packet {
            Packet::BinaryV3(_) | Packet::Binary(_) => {
                v3_bin_packet_encoder(packet, &mut data)?;
                has_binary = true;
            }
            packet => {
                v3_string_packet_encoder(packet, &mut data)?;
            }
        };
    }

    #[cfg(feature = "tracing")]
    tracing::debug!("sending packet: {:?}", &data);
    Ok(Payload::new(data, has_binary))
}

/// Encode multiple packet packet into a *string* payload according to the
/// [engine.io v3 protocol](https://github.com/socketio/engine.io-protocol/tree/v3#payload)
#[cfg(feature = "v3")]
pub async fn v3_string_encoder(
    mut rx: MutexGuard<'_, PeekableReceiver<Packet>>,
    max_payload: u64,
) -> Result<Payload, Error> {
    let mut data: Vec<u8> = Vec::new();

    #[cfg(feature = "tracing")]
    tracing::debug!("encoding payload with v3 string encoder");

    const PUNCTUATION_LEN: usize = 2;
    // number of digits of the max packet size, used to approximate the payload size
    let max_packet_size_len = max_payload.checked_ilog10().unwrap_or(0) as usize + 1;
    // Current size of the payload
    let current_size = data.len() + PUNCTUATION_LEN + max_packet_size_len;
    while let Some(packet) = try_recv_packet(&mut rx, current_size, max_payload, true) {
        v3_string_packet_encoder(packet, &mut data)?;
    }

    // If there is no packet in the buffer, wait for the next packet
    if data.is_empty() {
        let packet = recv_packet(&mut rx).await?;
        v3_string_packet_encoder(packet, &mut data)?;
    }

    Ok(Payload::new(data, false))
}

#[cfg(test)]
mod tests {

    use tokio::sync::Mutex;

    use super::*;
    const MAX_PAYLOAD: u64 = 100_000;

    #[tokio::test]
    async fn encode_v4_payload() {
        const PAYLOAD: &str = "4hello€\x1ebAQIDBA==\x1e4hello€";
        let (tx, rx) = tokio::sync::mpsc::channel::<Packet>(10);
        let rx = Mutex::new(PeekableReceiver::new(rx));
        let rx = rx.lock().await;
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::Binary(vec![1, 2, 3, 4])).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        let Payload { data, .. } = v4_encoder(rx, MAX_PAYLOAD).await.unwrap();
        assert_eq!(data, PAYLOAD.as_bytes());
    }

    #[tokio::test]
    async fn max_payload_v4() {
        const MAX_PAYLOAD: u64 = 10;
        let (tx, rx) = tokio::sync::mpsc::channel::<Packet>(10);
        let mutex = Mutex::new(PeekableReceiver::new(rx));
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::Binary(vec![1, 2, 3, 4])).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        {
            let rx = mutex.lock().await;
            let Payload { data, .. } = v4_encoder(rx, MAX_PAYLOAD).await.unwrap();
            assert_eq!(data, "4hello€".as_bytes());
        }
        {
            let rx = mutex.lock().await;
            let Payload { data, .. } = v4_encoder(rx, MAX_PAYLOAD + 10).await.unwrap();
            assert_eq!(data, "bAQIDBA==\x1e4hello€".as_bytes());
        }
        {
            let rx = mutex.lock().await;
            let Payload { data, .. } = v4_encoder(rx, MAX_PAYLOAD + 10).await.unwrap();
            assert_eq!(data, "4hello€".as_bytes());
        }
    }

    #[cfg(feature = "v3")]
    #[tokio::test]
    async fn encode_v3b64_payload() {
        const PAYLOAD: &str = "7:4hello€10:b4AQIDBA==7:4hello€";
        let (tx, rx) = tokio::sync::mpsc::channel::<Packet>(10);
        let mutex = Mutex::new(PeekableReceiver::new(rx));
        let rx = mutex.lock().await;

        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::BinaryV3(vec![1, 2, 3, 4])).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        let Payload {
            data, has_binary, ..
        } = v3_string_encoder(rx, MAX_PAYLOAD).await.unwrap();
        assert_eq!(data, PAYLOAD.as_bytes());
        assert!(!has_binary);
    }

    #[cfg(feature = "v3")]
    #[tokio::test]
    async fn max_payload_v3_b64() {
        const MAX_PAYLOAD: u64 = 10;

        let (tx, rx) = tokio::sync::mpsc::channel::<Packet>(10);
        let mutex = Mutex::new(PeekableReceiver::new(rx));
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::BinaryV3(vec![1, 2, 3, 4])).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        {
            let rx = mutex.lock().await;
            let Payload { data, .. } = v3_string_encoder(rx, MAX_PAYLOAD).await.unwrap();
            assert_eq!(data, "7:4hello€".as_bytes());
        }
        {
            let rx = mutex.lock().await;
            let Payload { data, .. } = v3_string_encoder(rx, MAX_PAYLOAD + 10).await.unwrap();
            assert_eq!(data, "10:b4AQIDBA==7:4hello€7:4hello€".as_bytes());
        }
    }

    #[cfg(feature = "v3")]
    #[tokio::test]
    async fn encode_v3binary_payload() {
        const PAYLOAD: [u8; 20] = [
            0, 9, 255, 52, 104, 101, 108, 108, 111, 226, 130, 172, 1, 5, 255, 4, 1, 2, 3, 4,
        ];
        let (tx, rx) = tokio::sync::mpsc::channel::<Packet>(10);
        let mutex = Mutex::new(PeekableReceiver::new(rx));
        let rx = mutex.lock().await;

        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::BinaryV3(vec![1, 2, 3, 4])).unwrap();
        let Payload {
            data, has_binary, ..
        } = v3_binary_encoder(rx, MAX_PAYLOAD).await.unwrap();
        assert_eq!(data, PAYLOAD);
        assert!(has_binary);
    }

    #[cfg(feature = "v3")]
    #[tokio::test]
    async fn max_payload_v3_binary() {
        const MAX_PAYLOAD: u64 = 25;

        const PAYLOAD: [u8; 23] = [
            0, 1, 1, 255, 52, 104, 101, 108, 108, 111, 111, 111, 226, 130, 172, 1, 5, 255, 4, 1, 2,
            3, 4,
        ];
        let (tx, rx) = tokio::sync::mpsc::channel::<Packet>(10);
        let mutex = Mutex::new(PeekableReceiver::new(rx));
        tx.try_send(Packet::Message("hellooo€".into())).unwrap();
        tx.try_send(Packet::BinaryV3(vec![1, 2, 3, 4])).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        tx.try_send(Packet::Message("hello€".into())).unwrap();
        {
            let rx = mutex.lock().await;
            let Payload { data, .. } = v3_binary_encoder(rx, MAX_PAYLOAD).await.unwrap();
            assert_eq!(data, PAYLOAD);
        }
        {
            let rx = mutex.lock().await;
            let Payload { data, .. } = v3_binary_encoder(rx, MAX_PAYLOAD).await.unwrap();
            assert_eq!(data, "7:4hello€7:4hello€".as_bytes());
        }
    }
}
