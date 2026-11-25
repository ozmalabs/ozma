/**
 * Ozma Screen Renderer — generates images for any display.
 *
 * HTTP API:
 *   POST /render    — render a template, ScreenLayout (`layout`), or Widget IR
 *                     (`ir`) with data; returns PNG/JPEG/SVG
 *   GET  /templates — list available templates
 *   GET  /health    — health check
 *
 * WebSocket:
 *   Connect to ws://localhost:7390 for real-time render streaming.
 *   Send: {"template": "status", "data": {...}, "width": 480, "height": 480}
 *      or {"ir": {...}} / {"layout": {...}, "data": {...}}
 *   Receive: binary PNG frame
 *
 * Supports screen sizes from 72x72 (Stream Deck key) to 4K.
 */

const express = require('express');
const { WebSocketServer } = require('ws');
const http = require('http');
const { renderTemplate, renderIr, listTemplates } = require('./templates');

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
    const { template, ir, layout, data, width, height, format } = req.body;
    const w = width || 480;
    const h = height || 480;
    const fmt = format || 'png';
    const semantic = ir || layout;

    const buffer = semantic
      ? await renderIr(semantic, data || {}, w, h, fmt)
      : await renderTemplate(template || 'status', data || {}, w, h, fmt);
    const type = fmt === 'jpeg' ? 'image/jpeg' : (fmt === 'svg' ? 'image/svg+xml' : 'image/png');
    res.type(type).send(buffer);
  } catch (err) {
    res.status(err.code ? 400 : 500).json({ error: err.message, code: err.code });
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
      const semantic = req.ir || req.layout;
      const buffer = semantic
        ? await renderIr(
          semantic,
          req.data || {},
          req.width || 480,
          req.height || 480,
          req.format || 'png',
        )
        : await renderTemplate(
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
