// SPDX-License-Identifier: AGPL-3.0-only
//! Loopback-only, view-only RFB 3.8 output for composed frames.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    time::timeout,
};

const MAX_PIXELS: usize = 16 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct RfbFrame {
    width: u16,
    height: u16,
    rgba: Vec<u8>,
}

impl RfbFrame {
    pub fn new(width: u16, height: u16, rgba: Vec<u8>) -> Result<Self, VncError> {
        let pixels = usize::from(width) * usize::from(height);
        if pixels == 0 || pixels > MAX_PIXELS || rgba.len() != pixels * 4 {
            return Err(VncError::Protocol(
                "invalid framebuffer dimensions or length",
            ));
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VncError {
    #[error("the view-only RFB listener requires a loopback address")]
    NonLocalBind,
    #[error("RFB protocol error: {0}")]
    Protocol(&'static str),
    #[error("RFB operation timed out")]
    TimedOut,
    #[error("RFB I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct RfbServer(TcpListener);

impl RfbServer {
    pub async fn bind(addr: SocketAddr) -> Result<Self, VncError> {
        if !addr.ip().is_loopback() {
            return Err(VncError::NonLocalBind);
        }
        Ok(Self(TcpListener::bind(addr).await?))
    }

    pub fn local_addr(&self) -> Result<SocketAddr, VncError> {
        Ok(self.0.local_addr()?)
    }

    pub async fn accept_frames(
        &self,
        frames: watch::Receiver<Option<Arc<RfbFrame>>>,
    ) -> Result<(), VncError> {
        let (mut stream, _) = self.0.accept().await?;
        serve(&mut stream, frames).await
    }
}

struct PixelFormat {
    bytes: usize,
    big_endian: bool,
    maxima: [u16; 3],
    shifts: [u8; 3],
}

const DEFAULT_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];

impl PixelFormat {
    fn parse(format: &[u8]) -> Result<Self, VncError> {
        let invalid = VncError::Protocol("unsupported pixel format");
        let bits = format[0];
        if !matches!(bits, 8 | 16 | 32) || format[3] == 0 {
            return Err(invalid);
        }
        let maxima = [word(format, 4), word(format, 6), word(format, 8)];
        let shifts = [format[10], format[11], format[12]];
        let mut occupied = 0;
        for (&maximum, shift) in maxima.iter().zip(shifts) {
            let mask = u64::from(maximum) << shift.min(32);
            if maximum == 0
                || !u32::from(maximum).wrapping_add(1).is_power_of_two()
                || mask >= (1u64 << bits)
                || occupied & mask != 0
            {
                return Err(invalid);
            }
            occupied |= mask;
        }
        Ok(Self {
            bytes: usize::from(bits / 8),
            big_endian: format[2] != 0,
            maxima,
            shifts,
        })
    }

    fn encode(&self, rgba: &[u8], output: &mut Vec<u8>) {
        let mut pixel = 0;
        for (index, &value) in rgba[..3].iter().enumerate() {
            let scaled = (u32::from(value) * u32::from(self.maxima[index]) + 127) / 255;
            pixel |= scaled << self.shifts[index];
        }
        if self.big_endian {
            output.extend_from_slice(&pixel.to_be_bytes()[4 - self.bytes..]);
        } else {
            output.extend_from_slice(&pixel.to_le_bytes()[..self.bytes]);
        }
    }
}

fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

async fn init(stream: &mut TcpStream, frame: &RfbFrame) -> Result<(), VncError> {
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0; 12];
    stream.read_exact(&mut version).await?;
    if &version != b"RFB 003.008\n" {
        return Err(VncError::Protocol("RFB 3.8 is required"));
    }
    stream.write_all(&[1, 1]).await?; // Security type None, bound to loopback.
    if stream.read_u8().await? != 1 {
        stream.write_u32(1).await?;
        let reason = b"Unsupported security type";
        stream.write_u32(reason.len() as u32).await?;
        stream.write_all(reason).await?;
        return Err(VncError::Protocol("unsupported security type"));
    }
    stream.write_u32(0).await?;
    let _shared = stream.read_u8().await?;
    stream.write_u16(frame.width).await?;
    stream.write_u16(frame.height).await?;
    stream.write_all(&DEFAULT_FORMAT).await?;
    let name = b"Ozma composed scene";
    stream.write_u32(name.len() as u32).await?;
    stream.write_all(name).await?;
    Ok(())
}

async fn serve(
    stream: &mut TcpStream,
    mut frames: watch::Receiver<Option<Arc<RfbFrame>>>,
) -> Result<(), VncError> {
    let initial = frames
        .borrow()
        .clone()
        .ok_or(VncError::Protocol("no initial framebuffer"))?;
    timeout(IO_TIMEOUT, init(stream, &initial))
        .await
        .map_err(|_| VncError::TimedOut)??;
    let mut format = PixelFormat::parse(&DEFAULT_FORMAT)?;
    let mut previous: Option<Arc<RfbFrame>> = None;
    loop {
        let kind = match stream.read_u8().await {
            Ok(kind) => kind,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        match kind {
            0 => {
                let mut body = [0; 19];
                stream.read_exact(&mut body).await?;
                format = PixelFormat::parse(&body[3..])?;
                previous = None;
            }
            2 => {
                let mut head = [0; 3];
                stream.read_exact(&mut head).await?;
                let mut encodings = vec![0; usize::from(word(&head, 1)) * 4];
                stream.read_exact(&mut encodings).await?;
            }
            3 => {
                let mut request = [0; 9];
                stream.read_exact(&mut request).await?;
                let frame = loop {
                    let latest = frames
                        .borrow_and_update()
                        .clone()
                        .unwrap_or_else(|| initial.clone());
                    if request[0] == 0
                        || previous
                            .as_ref()
                            .is_none_or(|old| !Arc::ptr_eq(old, &latest))
                    {
                        break latest;
                    }
                    if frames.changed().await.is_err() {
                        return Ok(());
                    }
                };
                if (frame.width, frame.height) != (initial.width, initial.height) {
                    return Err(VncError::Protocol(
                        "frame size changed; reconnect the viewer",
                    ));
                }
                timeout(IO_TIMEOUT, send_update(stream, &frame, &request, &format))
                    .await
                    .map_err(|_| VncError::TimedOut)??;
                previous = Some(frame);
            }
            4 => stream.read_exact(&mut [0; 7]).await.map(|_| ())?,
            5 => stream.read_exact(&mut [0; 5]).await.map(|_| ())?,
            6 => {
                let mut head = [0; 7];
                stream.read_exact(&mut head).await?;
                let length =
                    u32::from_be_bytes(head[3..].try_into().expect("fixed header")) as usize;
                if length > 1 << 20 {
                    return Err(VncError::Protocol("clipboard data too large"));
                }
                stream.read_exact(&mut vec![0; length]).await?;
            }
            _ => return Err(VncError::Protocol("unsupported viewer message")),
        }
    }
}

async fn send_update(
    stream: &mut TcpStream,
    frame: &RfbFrame,
    request: &[u8; 9],
    format: &PixelFormat,
) -> Result<(), VncError> {
    let x = word(request, 1).min(frame.width);
    let y = word(request, 3).min(frame.height);
    let width = word(request, 5).min(frame.width - x);
    let height = word(request, 7).min(frame.height - y);
    stream.write_u16(0).await?; // FramebufferUpdate and padding.
    stream
        .write_u16(u16::from(width != 0 && height != 0))
        .await?;
    if width == 0 || height == 0 {
        return Ok(());
    }
    for field in [x, y, width, height] {
        stream.write_u16(field).await?;
    }
    stream.write_i32(0).await?; // Raw encoding.
    let mut row = Vec::with_capacity(usize::from(width) * format.bytes);
    for row_index in y..y + height {
        let start = (usize::from(row_index) * usize::from(frame.width) + usize::from(x)) * 4;
        for pixel in frame.rgba[start..start + usize::from(width) * 4].chunks_exact(4) {
            format.encode(pixel, &mut row);
        }
        stream.write_all(&row).await?;
        row.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn viewer_gets_live_frames_and_input_is_ignored() {
        let server = RfbServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        let frame = |r| Arc::new(RfbFrame::new(1, 1, vec![r, 2, 3, 255]).unwrap());
        let (send, receive) = watch::channel(Some(frame(1)));
        let handle = tokio::spawn(async move { server.accept_frames(receive).await.unwrap() });
        let mut client = TcpStream::connect(addr).await.unwrap();
        timeout(IO_TIMEOUT, async {
            let mut version = [0; 12];
            client.read_exact(&mut version).await.unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            client.write_all(&version).await.unwrap();
            assert_eq!(client.read_u16().await.unwrap(), 257);
            client.write_u8(1).await.unwrap();
            assert_eq!(client.read_u32().await.unwrap(), 0);
            client.write_u8(1).await.unwrap();
            assert_eq!(client.read_u16().await.unwrap(), 1);
            assert_eq!(client.read_u16().await.unwrap(), 1);
            let mut format = [0; 16];
            client.read_exact(&mut format).await.unwrap();
            assert_eq!(format, DEFAULT_FORMAT);
            let name_len = client.read_u32().await.unwrap();
            client
                .read_exact(&mut vec![0; name_len as usize])
                .await
                .unwrap();
            client.write_all(&[4, 1, 0, 0, 0, 0, 0, 65]).await.unwrap();
            for (incremental, red) in [(0, 1), (1, 9)] {
                if incremental == 1 {
                    send.send_replace(Some(frame(9)));
                }
                client
                    .write_all(&[3, incremental, 0, 0, 0, 0, 0, 1, 0, 1])
                    .await
                    .unwrap();
                let mut response = [0; 20];
                client.read_exact(&mut response).await.unwrap();
                assert_eq!(&response[..4], &[0, 0, 0, 1]);
                assert_eq!(&response[16..], &[red, 2, 3, 0]);
            }
        })
        .await
        .unwrap();
        drop(client);
        handle.await.unwrap();
    }
}
