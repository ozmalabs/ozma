# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Moonlight RTSP session server implementation.

Implements the RTSP session negotiation for Moonlight clients:
- TCP server on port 48010
- OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN handlers
- SDP generation with video/audio/control streams
- Session state machine and cleanup
"""

from __future__ import annotations

import asyncio
import hashlib
import logging
import secrets
import socket
import struct
import time
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Dict, Optional
from urllib.parse import urlparse

from .moonlight_pairing import PairingDatabase

log = logging.getLogger("ozma.moonlight.rtsp")


class SessionState(Enum):
    """RTSP session state machine."""
    INIT = "INIT"
    DESCRIBED = "DESCRIBED"
    SETUP = "SETUP"
    PLAYING = "PLAYING"
    TEARDOWN = "TEARDOWN"


@dataclass
class SessionConfig:
    """Per-session configuration negotiated via RTSP headers."""
    width: int = 1920
    height: int = 1080
    fps: int = 60
    bitrate: int = 10000  # kbps
    codec: str = "h265"  # h265, h264
    audio_codec: str = "opus"  # opus, pcm
    enable_audio: bool = True
    enable_video: bool = True


@dataclass
class MoonlightSession:
    """Active Moonlight streaming session."""
    session_id: str
    client_addr: str
    client_port: int
    video_port: int
    audio_port: int
    control_port: int
    config: SessionConfig
    state: SessionState = SessionState.INIT
    last_activity: float = field(default_factory=lambda: time.time())
    
    def to_dict(self) -> Dict[str, Any]:
        return {
            "session_id": self.session_id,
            "client_addr": self.client_addr,
            "client_port": self.client_port,
            "video_port": self.video_port,
            "audio_port": self.audio_port,
            "control_port": self.control_port,
            "state": self.state.value,
            "config": {
                "width": self.config.width,
                "height": self.config.height,
                "fps": self.config.fps,
                "bitrate": self.config.bitrate,
                "codec": self.config.codec,
                "audio_codec": self.config.audio_codec,
                "enable_audio": self.config.enable_audio,
                "enable_video": self.config.enable_video,
            }
        }


class MoonlightRTSPServer:
    """
    Moonlight RTSP session server.
    
    Handles RTSP session negotiation for Moonlight clients:
    - OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN
    - SDP generation with video/audio/control streams
    - Session state machine and cleanup
    """
    
    def __init__(self, data_dir: str, pairing_db: PairingDatabase) -> None:
        self._data_dir = data_dir
        self._pairing_db = pairing_db
        self._sessions: Dict[str, MoonlightSession] = {}
        self._server: Optional[asyncio.AbstractServer] = None
        self._session_cleanup_task: Optional[asyncio.Task] = None
        
    async def start(self) -> None:
        """Start the RTSP server on port 48010."""
        self._server = await asyncio.start_server(
            self._handle_client,
            '0.0.0.0',
            48010
        )
        
        # Start session cleanup task
        self._session_cleanup_task = asyncio.create_task(
            self._cleanup_sessions(), 
            name="rtsp-session-cleanup"
        )
        
        log.info("Moonlight RTSP server started on port 48010")
        
    async def stop(self) -> None:
        """Stop the RTSP server."""
        if self._session_cleanup_task:
            self._session_cleanup_task.cancel()
            try:
                await self._session_cleanup_task
            except asyncio.CancelledError:
                pass
                
        if self._server:
            self._server.close()
            await self._server.wait_closed()
            
        # Close all sessions
        for session in list(self._sessions.values()):
            await self._teardown_session(session)
            
        log.info("Moonlight RTSP server stopped")
        
    def list_sessions(self) -> list[Dict[str, Any]]:
        """Get all active sessions."""
        return [session.to_dict() for session in self._sessions.values()]
        
    async def _handle_client(
        self, 
        reader: asyncio.StreamReader, 
        writer: asyncio.StreamWriter
    ) -> None:
        """Handle a single RTSP client connection."""
        client_addr = writer.get_extra_info('peername')
        if client_addr:
            log.info(f"RTSP client connected from {client_addr[0]}:{client_addr[1]}")
            
        try:
            while True:
                # Read RTSP request
                request_line = await reader.readline()
                if not request_line:
                    break
                    
                request_line = request_line.decode('utf-8').strip()
                if not request_line:
                    continue
                    
                # Parse request line
                parts = request_line.split()
                if len(parts) < 3:
                    continue
                    
                method, uri, version = parts
                
                # Read headers
                headers = {}
                while True:
                    line = await reader.readline()
                    if not line or line == b'\r\n':
                        break
                    line = line.decode('utf-8').strip()
                    if ':' in line:
                        key, value = line.split(':', 1)
                        headers[key.strip().lower()] = value.strip()
                        
                # Handle request
                response = await self._handle_request(
                    method, uri, headers, reader, writer
                )
                
                if response:
                    writer.write(response.encode('utf-8'))
                    await writer.drain()
                    
                # Check if we should close the connection
                if method == "TEARDOWN" or headers.get('connection', '').lower() == 'close':
                    break
                    
        except Exception as e:
            log.error(f"RTSP client error: {e}")
        finally:
            writer.close()
            await writer.wait_closed()
            
    async def _handle_request(
        self,
        method: str,
        uri: str,
        headers: Dict[str, str],
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter
    ) -> Optional[str]:
        """Handle an RTSP request."""
        client_addr = writer.get_extra_info('peername')
        if not client_addr:
            return None
            
        client_ip, client_port = client_addr[0], client_addr[1]
        
        # Extract session ID from headers
        session_id = headers.get('session', '')
        
        try:
            if method == "OPTIONS":
                return self._handle_options(uri, headers)
            elif method == "DESCRIBE":
                return await self._handle_describe(uri, headers, client_ip, client_port)
            elif method == "SETUP":
                return await self._handle_setup(uri, headers, client_ip, client_port, session_id)
            elif method == "PLAY":
                return await self._handle_play(uri, headers, session_id)
            elif method == "TEARDOWN":
                return await self._handle_teardown(uri, headers, session_id)
            else:
                return self._build_response(405, "Method Not Allowed")
        except Exception as e:
            log.error(f"Error handling {method} request: {e}")
            return self._build_response(500, "Internal Server Error")
            
    def _handle_options(self, uri: str, headers: Dict[str, str]) -> str:
        """Handle OPTIONS request."""
        response_headers = {
            'Public': 'OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN',
            'Server': 'Ozma Moonlight Server'
        }
        return self._build_response(200, "OK", response_headers)
        
    async def _handle_describe(
        self, 
        uri: str, 
        headers: Dict[str, str], 
        client_ip: str, 
        client_port: int
    ) -> str:
        """Handle DESCRIBE request."""
        # Create a new session
        session_id = secrets.token_hex(8)
        
        # Parse configuration from headers
        config = self._parse_session_config(headers)
        
        # Allocate ports for this session
        video_port = self._find_free_port()
        audio_port = self._find_free_port()
        control_port = self._find_free_port()
        
        # Create session
        session = MoonlightSession(
            session_id=session_id,
            client_addr=client_ip,
            client_port=client_port,
            video_port=video_port,
            audio_port=audio_port,
            control_port=control_port,
            config=config
        )
        
        self._sessions[session_id] = session
        session.state = SessionState.DESCRIBED
        session.last_activity = time.time()
        
        # Generate SDP
        sdp = self._generate_sdp(session)
        
        response_headers = {
            'Content-Type': 'application/sdp',
            'Content-Length': str(len(sdp)),
            'Session': session_id
        }
        
        return self._build_response(200, "OK", response_headers, sdp)
        
    async def _handle_setup(
        self,
        uri: str,
        headers: Dict[str, str],
        client_ip: str,
        client_port: int,
        session_id: str
    ) -> str:
        """Handle SETUP request."""
        if not session_id or session_id not in self._sessions:
            return self._build_response(454, "Session Not Found")
            
        session = self._sessions[session_id]
        
        # Verify client address matches
        if session.client_addr != client_ip:
            return self._build_response(403, "Forbidden")
            
        session.state = SessionState.SETUP
        session.last_activity = time.time()
        
        # Extract transport parameters
        transport = headers.get('transport', '')
        client_ports = None
        if 'client_port=' in transport:
            ports_part = transport.split('client_port=')[1].split(';')[0]
            if '-' in ports_part:
                port_range = ports_part.split('-')
                client_ports = (int(port_range[0]), int(port_range[1]))
            else:
                client_ports = (int(ports_part), int(ports_part) + 1)
                
        response_headers = {
            'Session': session_id,
            'Transport': f'RTP/AVP;unicast;client_port={session.video_port}-{session.audio_port};server_port={session.video_port}-{session.audio_port}'
        }
        
        return self._build_response(200, "OK", response_headers)
        
    async def _handle_play(
        self,
        uri: str,
        headers: Dict[str, str],
        session_id: str
    ) -> str:
        """Handle PLAY request."""
        if not session_id or session_id not in self._sessions:
            return self._build_response(454, "Session Not Found")
            
        session = self._sessions[session_id]
        session.state = SessionState.PLAYING
        session.last_activity = time.time()
        
        response_headers = {
            'Session': session_id,
            'RTP-Info': f'url=rtsp://localhost:48010/stream/video;seq=0,url=rtsp://localhost:48010/stream/audio;seq=0'
        }
        
        return self._build_response(200, "OK", response_headers)
        
    async def _handle_teardown(
        self,
        uri: str,
        headers: Dict[str, str],
        session_id: str
    ) -> str:
        """Handle TEARDOWN request."""
        if not session_id or session_id not in self._sessions:
            return self._build_response(454, "Session Not Found")
            
        session = self._sessions[session_id]
        await self._teardown_session(session)
        
        response_headers = {
            'Session': session_id
        }
        
        return self._build_response(200, "OK", response_headers)
        
    def _parse_session_config(self, headers: Dict[str, str]) -> SessionConfig:
        """Parse session configuration from RTSP headers."""
        config = SessionConfig()
        
        # Parse width/height from x-nv-video-options
        video_options = headers.get('x-nv-video-options', '')
        if 'width=' in video_options:
            try:
                width_str = video_options.split('width=')[1].split(',')[0]
                config.width = int(width_str)
            except (ValueError, IndexError):
                pass
                
        if 'height=' in video_options:
            try:
                height_str = video_options.split('height=')[1].split(',')[0]
                config.height = int(height_str)
            except (ValueError, IndexError):
                pass
                
        if 'fps=' in video_options:
            try:
                fps_str = video_options.split('fps=')[1].split(',')[0]
                config.fps = int(fps_str)
            except (ValueError, IndexError):
                pass
                
        if 'bitrate=' in video_options:
            try:
                bitrate_str = video_options.split('bitrate=')[1].split(',')[0]
                config.bitrate = int(bitrate_str)
            except (ValueError, IndexError):
                pass
                
        # Parse codec preference
        if 'h264' in video_options.lower():
            config.codec = 'h264'
            
        # Parse audio options
        audio_options = headers.get('x-nv-audio-options', '')
        if 'disabled' in audio_options.lower():
            config.enable_audio = False
        elif 'pcm' in audio_options.lower():
            config.audio_codec = 'pcm'
            
        return config
        
    def _generate_sdp(self, session: MoonlightSession) -> str:
        """Generate SDP for the session."""
        sdp_lines = [
            'v=0',
            f'o=- {session.session_id} 1 IN IP4 127.0.0.1',
            's=Ozma Moonlight Stream',
            'c=IN IP4 127.0.0.1',
            't=0 0',
            'a=control:*',
            'a=range:npt=0-',
        ]
        
        # Video stream (H.265 primary, H.264 fallback)
        sdp_lines.extend([
            'm=video 0 RTP/AVP 96',
            'a=rtpmap:96 H265/90000',
            'a=control:streamid=video',
            'a=framerate:%d' % session.config.fps,
            'a=fmtp:96 profile-id=1',
            'm=video 0 RTP/AVP 97',
            'a=rtpmap:97 H264/90000',
            'a=control:streamid=video2',
            'a=framerate:%d' % session.config.fps,
            'a=fmtp:97 profile-level-id=42C01F',
        ])
        
        # Audio stream (OPUS primary, PCM fallback)
        if session.config.enable_audio:
            if session.config.audio_codec == 'opus':
                sdp_lines.extend([
                    'm=audio 0 RTP/AVP 98',
                    'a=rtpmap:98 opus/48000/2',
                    'a=control:streamid=audio',
                ])
            else:  # PCM
                sdp_lines.extend([
                    'm=audio 0 RTP/AVP 99',
                    'a=rtpmap:99 L16/48000/2',
                    'a=control:streamid=audio',
                ])
                
        # Control stream
        sdp_lines.extend([
            'm=application 0 RTP/AVP 100',
            'a=rtpmap:100 ENet-channel/90000',
            'a=control:streamid=control',
        ])
        
        return '\r\n'.join(sdp_lines) + '\r\n'
        
    def _build_response(
        self, 
        status_code: int, 
        status_text: str, 
        headers: Optional[Dict[str, str]] = None,
        body: str = ""
    ) -> str:
        """Build an RTSP response."""
        response_lines = [f"RTSP/1.0 {status_code} {status_text}"]
        
        if headers:
            for key, value in headers.items():
                response_lines.append(f"{key}: {value}")
                
        response_lines.append(f"Date: {time.time()}")
        response_lines.append("")
        
        if body:
            response_lines.append(body)
            
        return "\r\n".join(response_lines) + "\r\n"
        
    def _find_free_port(self) -> int:
        """Find a free UDP port."""
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.bind(('', 0))
            port = s.getsockname()[1]
        return port
        
    async def _teardown_session(self, session: MoonlightSession) -> None:
        """Teardown a session."""
        session.state = SessionState.TEARDOWN
        if session.session_id in self._sessions:
            del self._sessions[session.session_id]
        log.info(f"Session {session.session_id} torn down")
        
    async def _cleanup_sessions(self) -> None:
        """Periodically clean up expired sessions."""
        while True:
            try:
                await asyncio.sleep(10)  # Check every 10 seconds
                current_time = time.time()
                
                expired_sessions = []
                for session in self._sessions.values():
                    # Expire sessions after 30 seconds of inactivity
                    if current_time - session.last_activity > 30:
                        expired_sessions.append(session.session_id)
                        
                for session_id in expired_sessions:
                    if session_id in self._sessions:
                        session = self._sessions[session_id]
                        await self._teardown_session(session)
                        log.info(f"Session {session_id} expired and cleaned up")
                        
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error(f"Error in session cleanup: {e}")
