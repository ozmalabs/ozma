/**
 * Ozma Screen Renderer — generates images for any display.
 *
 * HTTP API:
 *   POST /render    — render a template with data, return PNG
 *   GET  /templates — list available templates
 *   GET  /health    — health check
 *
 * WebSocket:
 *   Connect to ws://localhost:7390 for real-time render streaming.
 *   Send: {"template": "status", "data": {...}, "width": 480, "height": 480}
 *   Receive: binary PNG frame
 *
 * Supports screen sizes from 72x72 (Stream Deck key) to 4K.
 */

const express = require('express');
const { WebSocketServer } = require('ws');
const http = require('http');
const { renderTemplate, listTemplates } = require('./templates');

const PORT = process.env.OZMA_RENDERER_PORT || 7390;

const app = express();
app.use(express.json({ limit: '1mb' }));

// ── REST API ────────────────────────────────────────────────────────────────

app.get('/health', (req, res) => {
  res.json({ ok: true, templates: listTemplates().length });
});

app.get('/templates', (req, res) => {
  res.json({ templates: listTemplates() });
});

app.post('/render', async (req, res) => {
  try {
    const { template, data, width, height, format } = req.body;
    const w = width || 480;
    const h = height || 480;
    const fmt = format || 'png';

    const buffer = await renderTemplate(template || 'status', data || {}, w, h, fmt);
    res.type(fmt === 'jpeg' ? 'image/jpeg' : 'image/png').send(buffer);
  } catch (err) {
    res.status(500).json({ error: err.message });
  }
});

// ── Server ──────────────────────────────────────────────────────────────────

const server = http.createServer(app);

// ── WebSocket for real-time streaming ───────────────────────────────────────

const wss = new WebSocketServer({ server });

wss.on('connection', (ws) => {
  console.log('Renderer WebSocket client connected');

  ws.on('message', async (msg) => {
    try {
      const req = JSON.parse(msg.toString());
      const buffer = await renderTemplate(
        req.template || 'status',
        req.data || {},
        req.width || 480,
        req.height || 480,
        req.format || 'png',
      );
      ws.send(buffer);
    } catch (err) {
      ws.send(JSON.stringify({ error: err.message }));
    }
  });
});

server.listen(PORT, () => {
  console.log(`Ozma Renderer listening on port ${PORT}`);
  console.log(`  REST:      http://localhost:${PORT}/render`);
  console.log(`  WebSocket: ws://localhost:${PORT}`);
  console.log(`  Templates: ${listTemplates().length} available`);
});
