// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end RTSP/RTP tests over loopback TCP and UDP.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ozma_media::{Codec, EncodedFrame, MediaFormat};
use ozma_rtp::{push_annex_b, split_annex_b, H264Depacketizer, RtpHeader};
use ozma_rtsp::{RtspConfig, RtspServer, VecVideoSource};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f, 0x8c, 0x8d, 0x40];
const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];

fn keyframe() -> EncodedFrame {
    let mut nal = vec![0x65];
    nal.extend((0..900).map(|index| (index % 251) as u8));
    let mut access_unit = Vec::new();
    push_annex_b(&mut access_unit, SPS);
    push_annex_b(&mut access_unit, PPS);
    push_annex_b(&mut access_unit, &nal);
    EncodedFrame::new(
        MediaFormat::new(Codec::H264, 1280, 720, 30, 4_000),
        1_000_000,
        7,
        true,
        access_unit,
    )
    .unwrap()
}

fn delta() -> EncodedFrame {
    let mut nal = vec![0x41, 0x9a];
    nal.extend((0..60).map(|index| (index % 199 + 1) as u8));
    let mut access_unit = Vec::new();
    push_annex_b(&mut access_unit, &nal);
    EncodedFrame::new(
        MediaFormat::new(Codec::H264, 1280, 720, 30, 4_000),
        1_033_333,
        8,
        false,
        access_unit,
    )
    .unwrap()
}

async fn start_server(frames: Vec<EncodedFrame>) -> (SocketAddr, JoinHandle<io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let frames = Arc::new(frames);
    let config = RtspConfig {
        fps: 30,
        session_timeout: Duration::from_secs(30),
        ..RtspConfig::default()
    };
    let server = RtspServer::new(config)
        .with_source_factory(move || Box::new(VecVideoSource::new((*frames).clone())));
    let handle = tokio::spawn(server.serve(listener));
    (address, handle)
}

#[derive(Debug)]
struct Response {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn session_id(&self) -> &str {
        self.header("session")
            .expect("Session header")
            .split(';')
            .next()
            .unwrap()
            .trim()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    pending: VecDeque<(u8, Vec<u8>)>,
}

impl Client {
    async fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).await.unwrap();
        stream.set_nodelay(true).unwrap();
        let (reader, writer) = stream.into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            pending: VecDeque::new(),
        }
    }

    async fn send(&mut self, request: &str) {
        self.writer.write_all(request.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn read_response(&mut self) -> Response {
        tokio::time::timeout(Duration::from_secs(5), self.read_response_inner())
            .await
            .expect("timed out waiting for an RTSP response")
    }

    async fn read_response_inner(&mut self) -> Response {
        loop {
            let mut first = [0u8; 1];
            self.reader.read_exact(&mut first).await.unwrap();
            if first[0] == b'$' {
                let (channel, packet) = self.read_interleaved_body().await;
                self.pending.push_back((channel, packet));
                continue;
            }
            if first[0] == b'\r' || first[0] == b'\n' {
                continue;
            }

            let mut line = vec![first[0]];
            self.reader.read_until(b'\n', &mut line).await.unwrap();
            let line = String::from_utf8_lossy(&line).trim().to_owned();
            let mut parts = line.splitn(3, ' ');
            let version = parts.next().unwrap_or_default();
            assert_eq!(version, "RTSP/1.0");
            let status = parts
                .next()
                .unwrap_or_default()
                .parse::<u16>()
                .expect("numeric status");
            let reason = parts.next().unwrap_or_default().to_owned();

            let mut headers = Vec::new();
            loop {
                let mut header = Vec::new();
                self.reader.read_until(b'\n', &mut header).await.unwrap();
                let header = String::from_utf8_lossy(&header);
                let header = header.trim_end_matches(['\r', '\n']);
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(':').expect("header colon");
                headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
            let content_length = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                self.reader.read_exact(&mut body).await.unwrap();
            }
            return Response {
                status,
                reason,
                headers,
                body,
            };
        }
    }

    async fn read_interleaved_body(&mut self) -> (u8, Vec<u8>) {
        let mut header = [0u8; 3];
        self.reader.read_exact(&mut header).await.unwrap();
        let channel = header[0];
        let length = u16::from_be_bytes([header[1], header[2]]) as usize;
        let mut payload = vec![0u8; length];
        self.reader.read_exact(&mut payload).await.unwrap();
        (channel, payload)
    }

    async fn next_interleaved(&mut self) -> (u8, Vec<u8>) {
        if let Some(frame) = self.pending.pop_front() {
            return frame;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut first = [0u8; 1];
                self.reader.read_exact(&mut first).await.unwrap();
                if first[0] == b'$' {
                    return self.read_interleaved_body().await;
                }
                if first[0] == b'\r' || first[0] == b'\n' {
                    continue;
                }
                panic!("unexpected RTSP data while waiting for interleaved RTP");
            }
        })
        .await
        .expect("timed out waiting for interleaved RTP")
    }

    async fn read_rtp_access_unit(&mut self) -> (Vec<u8>, bool, bool) {
        let mut depacketizer = H264Depacketizer::new();
        let mut packets = 0usize;
        loop {
            let (channel, packet) = self.next_interleaved().await;
            assert_eq!(channel, 0, "RTP uses interleaved channel 0");
            let header = RtpHeader::parse(&packet).expect("valid RTP packet");
            assert_eq!(header.payload_type, 96);
            packets += 1;
            let last = header.marker;
            if let Some(unit) = depacketizer.push(&packet).expect("depacketize") {
                assert!(last, "the marker packet closes the access unit");
                return (unit.data, unit.keyframe, true);
            }
            assert!(!last, "marker must close an access unit");
            assert!(packets < 128, "sanity packet limit");
        }
    }

    async fn write_and_read(&mut self, request: &str) -> Response {
        self.send(request).await;
        self.read_response().await
    }
}

#[tokio::test]
async fn tcp_interleaved_handshake_and_media_roundtrip() {
    let (address, server) = start_server(vec![keyframe()]).await;
    let mut client = Client::connect(address).await;
    let base = format!("rtsp://{address}/stream");

    let options = client
        .write_and_read("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: ozma-test/1.0\r\n\r\n")
        .await;
    assert_eq!(options.status, 200);
    assert_eq!(options.header("cseq"), Some("1"));
    let public = options.header("public").unwrap();
    for method in [
        "OPTIONS",
        "DESCRIBE",
        "SETUP",
        "PLAY",
        "PAUSE",
        "TEARDOWN",
        "GET_PARAMETER",
        "SET_PARAMETER",
    ] {
        assert!(public.contains(method), "Public advertises {method}");
    }

    let describe = client
        .write_and_read(&format!(
            "DESCRIBE {base} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\nUser-Agent: Hestia/0.1\r\n\r\n"
        ))
        .await;
    assert_eq!(describe.status, 200, "{}", describe.reason);
    assert_eq!(describe.header("cseq"), Some("2"));
    assert_eq!(describe.header("content-type"), Some("application/sdp"));
    assert_eq!(
        describe
            .header("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        describe.body.len()
    );
    let sdp = describe.text();
    assert!(sdp.contains("m=video 0 RTP/AVP 96"));
    assert!(sdp.contains("a=rtpmap:96 H264/90000"));
    assert!(sdp.contains("a=fmtp:96 packetization-mode=1"));
    assert!(sdp.contains("a=control:trackID=0"));
    assert!(describe
        .header("content-base")
        .unwrap()
        .starts_with("rtsp://"));
    let session = describe.session_id().to_owned();
    assert!(!session.is_empty());
    assert!(describe.header("session").unwrap().contains("timeout="));

    let setup = client
        .write_and_read(&format!(
            "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ))
        .await;
    assert_eq!(setup.status, 200, "{}", setup.reason);
    assert_eq!(setup.header("cseq"), Some("3"));
    assert_eq!(setup.session_id(), session);
    assert_eq!(
        setup.header("transport"),
        Some("RTP/AVP/TCP;unicast;interleaved=0-1")
    );

    let play = client
        .write_and_read(&format!(
            "PLAY {base} RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\nRange: npt=0.000-\r\n\r\n"
        ))
        .await;
    assert_eq!(play.status, 200, "{}", play.reason);
    assert_eq!(play.header("cseq"), Some("4"));
    assert_eq!(play.session_id(), session);
    assert!(play.header("rtp-info").unwrap().contains("seq="));

    let (aux, keyframe, closed) = client.read_rtp_access_unit().await;
    assert!(keyframe);
    assert!(closed);
    let nals: Vec<&[u8]> = split_annex_b(&aux).unwrap();
    assert_eq!(nals[0], SPS);
    assert_eq!(nals[1], PPS);
    assert_eq!(nals[2][0] & 0x1f, 5, "IDR slice");

    // The connection stays open after PLAY and after TEARDOWN.
    let teardown = client
        .write_and_read(&format!(
            "TEARDOWN {base} RTSP/1.0\r\nCSeq: 5\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(teardown.status, 200);
    assert_eq!(teardown.header("cseq"), Some("5"));
    assert_eq!(teardown.session_id(), session);

    let after = client
        .write_and_read("OPTIONS * RTSP/1.0\r\nCSeq: 6\r\n\r\n")
        .await;
    assert_eq!(after.status, 200);
    assert_eq!(after.header("cseq"), Some("6"));

    server.abort();
}

#[tokio::test]
async fn udp_setup_receives_rtp_datagrams() {
    let (address, server) = start_server(vec![keyframe()]).await;
    let mut client = Client::connect(address).await;
    let base = format!("rtsp://{address}/stream");
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client_rtp = udp.local_addr().unwrap().port();
    let client_rtcp = client_rtp.checked_add(1).unwrap();

    let describe = client
        .write_and_read(&format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n"))
        .await;
    assert_eq!(describe.status, 200);
    let session = describe.session_id().to_owned();

    let setup = client
        .write_and_read(&format!(
            "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\nTransport: RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp}\r\n\r\n"
        ))
        .await;
    assert_eq!(setup.status, 200, "{}", setup.reason);
    let transport = setup.header("transport").unwrap();
    assert!(transport.contains(&format!("client_port={client_rtp}-{client_rtcp}")));
    assert!(transport.contains("server_port="));
    let server_rtp = transport
        .split("server_port=")
        .nth(1)
        .unwrap()
        .split('-')
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();

    let play = client
        .write_and_read(&format!(
            "PLAY {base} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(play.status, 200, "{}", play.reason);

    let mut depacketizer = H264Depacketizer::new();
    let mut source_port = None;
    let aux = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buffer = vec![0u8; 2048];
        loop {
            let (length, from) = udp.recv_from(&mut buffer).await.unwrap();
            source_port.get_or_insert(from.port());
            if let Some(unit) = depacketizer
                .push(&buffer[..length])
                .expect("valid UDP RTP packet")
            {
                break unit.data;
            }
        }
    })
    .await
    .expect("timed out waiting for UDP RTP");
    assert_eq!(source_port, Some(server_rtp));
    let nals: Vec<&[u8]> = split_annex_b(&aux).unwrap();
    assert_eq!(nals[0], SPS);
    assert_eq!(nals[2][0] & 0x1f, 5);

    let teardown = client
        .write_and_read(&format!(
            "TEARDOWN {base} RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(teardown.status, 200);
    server.abort();
}

#[tokio::test]
async fn pause_parameters_and_state_errors() {
    let (address, server) = start_server(vec![keyframe(), delta()]).await;
    let mut client = Client::connect(address).await;
    let base = format!("rtsp://{address}/stream");

    let describe = client
        .write_and_read(&format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n"))
        .await;
    let session = describe.session_id().to_owned();

    // PLAY before SETUP is a state error, not a crash.
    let early = client
        .write_and_read(&format!(
            "PLAY {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(early.status, 455);
    assert_eq!(early.header("cseq"), Some("2"));

    let setup = client
        .write_and_read(&format!(
            "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n"
        ))
        .await;
    assert_eq!(setup.status, 200);
    assert_eq!(
        setup.header("transport"),
        Some("RTP/AVP/TCP;unicast;interleaved=2-3")
    );

    let play = client
        .write_and_read(&format!(
            "PLAY {base} RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(play.status, 200);

    let get = client
        .write_and_read(&format!(
            "GET_PARAMETER {base} RTSP/1.0\r\nCSeq: 5\r\nSession: {session}\r\nContent-Type: text/parameters\r\nContent-Length: 8\r\n\r\nping=yes"
        ))
        .await;
    assert_eq!(get.status, 200);
    assert_eq!(get.header("cseq"), Some("5"));
    assert_eq!(get.text(), "ping=yes");

    let set = client
        .write_and_read(&format!(
            "SET_PARAMETER {base} RTSP/1.0\r\nCSeq: 6\r\nSession: {session}\r\nContent-Length: 5\r\n\r\nquiet"
        ))
        .await;
    assert_eq!(set.status, 200);
    assert_eq!(set.header("cseq"), Some("6"));

    let pause = client
        .write_and_read(&format!(
            "PAUSE {base} RTSP/1.0\r\nCSeq: 7\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(pause.status, 200);
    assert_eq!(pause.header("cseq"), Some("7"));

    let pause_again = client
        .write_and_read(&format!(
            "PAUSE {base} RTSP/1.0\r\nCSeq: 8\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(pause_again.status, 455);

    server.abort();
}

#[tokio::test]
async fn malformed_request_gets_400_and_connection_survives() {
    let (address, server) = start_server(vec![keyframe()]).await;
    let mut client = Client::connect(address).await;

    let garbage = client.write_and_read("GARBAGE\r\n\r\n").await;
    assert_eq!(garbage.status, 400);

    let options = client
        .write_and_read("OPTIONS * RTSP/1.0\r\nCSeq: 9\r\n\r\n")
        .await;
    assert_eq!(options.status, 200);
    assert_eq!(options.header("cseq"), Some("9"));

    let describe = client
        .write_and_read(&format!(
            "DESCRIBE rtsp://{address}/stream RTSP/1.0\r\nCSeq: 10\r\n\r\n"
        ))
        .await;
    assert_eq!(describe.status, 200);
    let session = describe.session_id().to_owned();

    let unsupported = client
        .write_and_read(&format!(
            "SETUP rtsp://{address}/trackID=0 RTSP/1.0\r\nCSeq: 11\r\nSession: {session}\r\nTransport: RTP/SAVP;unicast;client_port=9000-9001\r\n\r\n"
        ))
        .await;
    assert_eq!(unsupported.status, 461);
    assert_eq!(unsupported.header("cseq"), Some("11"));

    let unknown_session = client
        .write_and_read(
            "PLAY rtsp://127.0.0.1/stream RTSP/1.0\r\nCSeq: 12\r\nSession: nope\r\n\r\n",
        )
        .await;
    assert_eq!(unknown_session.status, 454);

    let unknown_method = client
        .write_and_read("ANNOUNCE * RTSP/1.0\r\nCSeq: 13\r\n\r\n")
        .await;
    assert_eq!(unknown_method.status, 501);
    assert_eq!(unknown_method.header("cseq"), Some("13"));

    server.abort();
}

#[tokio::test]
async fn oversized_header_line_is_rejected_without_panicking() {
    let (address, server) = start_server(vec![]).await;
    let mut client = Client::connect(address).await;
    let huge = "x".repeat(32 * 1024);
    client
        .send(&format!(
            "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nX-Oversized: {huge}\r\n\r\n"
        ))
        .await;
    let response = client.read_response().await;
    assert_eq!(response.status, 400);
    server.abort();
}

#[tokio::test]
async fn setup_without_source_returns_503_on_play() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = RtspServer::new(RtspConfig::default());
    let handle = tokio::spawn(server.serve(listener));
    let mut client = Client::connect(address).await;
    let base = format!("rtsp://{address}/stream");

    let describe = client
        .write_and_read(&format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n"))
        .await;
    let session = describe.session_id().to_owned();
    let setup = client
        .write_and_read(&format!(
            "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ))
        .await;
    assert_eq!(setup.status, 200);
    let play = client
        .write_and_read(&format!(
            "PLAY {base} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"
        ))
        .await;
    assert_eq!(play.status, 503);
    handle.abort();
}
