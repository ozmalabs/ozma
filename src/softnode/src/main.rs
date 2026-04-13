use clap::Parser;
use ozma_proto::wire::OzmaPacket;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::net::{UnixStream, UdpSocket};

/// Soft Node - Virtual compute node with evdev HID injection
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Node name, e.g. 'vm1'
    #[clap(long)]
    name: String,

    /// UDP port to listen on
    #[clap(long, default_value_t = 7332)]
    port: u16,

    /// QMP control socket path
    #[clap(long, default_value = "")]
    qmp: String,

    /// VNC host for video streaming
    #[clap(long)]
    vnc_host: Option<String>,

    /// VNC port for video streaming
    #[clap(long)]
    vnc_port: Option<u16>,

    /// Controller API URL for registration
    #[clap(long, default_value = "http://localhost:7380/api/v1/nodes/register")]
    register_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegisterRequest {
    id: String,
    host: String,
    port: u16,
    proto: String,
    role: String,
    hw: String,
    fw: String,
    cap: String,
    vnc_host: String,
    vnc_port: String,
    api_port: String,
    audio_type: String,
    audio_sink: String,
    capture_device: String,
    machine_class: String,
}

#[derive(Debug)]
struct SoftNode {
    name: String,
    port: u16,
    qmp_path: String,
    vnc_host: Option<String>,
    vnc_port: Option<u16>,
    register_url: String,
    qmp_stream: Option<UnixStream>,
}

impl SoftNode {
    async fn new(args: Args) -> Result<Self, Box<dyn std::error::Error>> {
        let mut node = SoftNode {
            name: args.name,
            port: args.port,
            qmp_path: args.qmp,
            vnc_host: args.vnc_host,
            vnc_port: args.vnc_port,
            register_url: args.register_url,
            qmp_stream: None,
        };

        if !node.qmp_path.is_empty() {
            node.qmp_stream = Some(UnixStream::connect(&node.qmp_path).await?);
        }

        Ok(node)
    }

    async fn register(&self) -> Result<(), Box<dyn std::error::Error>> {
        let client = reqwest::Client::new();
        
        let local_ip = self.get_local_ip()?;
        
        let request = RegisterRequest {
            id: format!("{}._ozma._udp.local.", self.name),
            host: local_ip,
            port: self.port,
            proto: "1".to_string(),
            role: "compute".to_string(),
            hw: "soft".to_string(),
            fw: "0.1.0".to_string(),
            cap: if self.qmp_path.is_empty() { "qmp".to_string() } else { "qmp,power".to_string() },
            vnc_host: self.vnc_host.clone().unwrap_or_default(),
            vnc_port: self.vnc_port.unwrap_or(0).to_string(),
            api_port: "".to_string(),
            audio_type: "".to_string(),
            audio_sink: "".to_string(),
            capture_device: "".to_string(),
            machine_class: "workstation".to_string(),
        };

        let response = client
            .post(&self.register_url)
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            eprintln!("Registration failed with status: {}", response.status());
        }

        Ok(())
    }

    fn get_local_ip(&self) -> Result<String, Box<dyn std::error::Error>> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.connect("8.8.8.8:80")?;
        let addr = socket.local_addr()?;
        Ok(addr.ip().to_string())
    }

    async fn announce_mdns(&self) -> Result<(), Box<dyn std::error::Error>> {
        // In a full implementation, this would use libmdns or similar
        // For now, we'll just log that we would announce
        println!("[soft-node:{}] mDNS announced: {} @ {}:{}", 
                 self.name, self.name, self.get_local_ip()?, self.port);
        Ok(())
    }

    async fn handle_packet(&mut self, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if data.is_empty() {
            return Ok(());
        }

        // Parse the Ozma packet
        if let Ok(packet) = OzmaPacket::parse(data) {
            // Forward to QMP if available
            if let Some(ref mut stream) = self.qmp_stream {
                // In a full implementation, we would convert HID reports to QMP events
                // For now, we'll just log the packet
                println!("Received packet: {:?}", packet);
                
                // Example of how we might send to QMP:
                // let qmp_command = self.convert_hid_to_qmp(packet);
                // stream.write_all(qmp_command.as_bytes()).await?;
            }
        }

        Ok(())
    }

    async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Register with controller
        self.register().await?;
        
        // Announce via mDNS
        self.announce_mdns().await?;

        // Create UDP socket
        let addr = format!("0.0.0.0:{}", self.port);
        let socket = UdpSocket::bind(&addr).await?;
        println!("[soft-node:{}] Listening on UDP {}", self.name, addr);

        let mut buf = vec![0; 1024];
        
        loop {
            let (len, _src) = socket.recv_from(&mut buf).await?;
            if len > 0 {
                self.handle_packet(&buf[..len]).await?;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut node = SoftNode::new(args).await?;
    node.run().await
}
