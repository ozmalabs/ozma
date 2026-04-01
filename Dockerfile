# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
# Ozma Controller — Docker image
#
# Usage:
#   docker compose up          (recommended — uses docker-compose.yml)
#   docker build -t ozma .     (standalone build)
#   docker run -d --name ozma --net=host ozma
#
# The controller needs host networking for:
#   - mDNS discovery (multicast UDP)
#   - UDP HID packets to nodes
#   - PipeWire audio routing (host PipeWire socket)

FROM python:3.13-slim

# System deps for evdev, PipeWire tools, ffmpeg, avahi
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg \
    pipewire \
    pipewire-pulse \
    wireplumber \
    avahi-utils \
    v4l-utils \
    libevdev2 \
    libevdev-dev \
    gcc \
    python3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/ozma

# Install Python deps
COPY controller/requirements.txt /opt/ozma/requirements.txt
RUN pip install --no-cache-dir -r requirements.txt pynacl

# Copy application code
COPY controller/ /opt/ozma/controller/
COPY softnode/ /opt/ozma/softnode/

# Working directory for the controller
WORKDIR /opt/ozma/controller

# Expose ports
#   7380 — REST API + WebSocket
#   7331 — UDP HID (outbound to nodes, but also listens for responses)
EXPOSE 7380/tcp

# Health check
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:7380/health', timeout=2)" || exit 1

# Run the controller
CMD ["python", "main.py", "--virtual-only"]
