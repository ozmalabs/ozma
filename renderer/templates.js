/**
 * Template rendering engine — generates pixel-perfect images for any screen.
 *
 * Built-in templates:
 *   status      — AIDA64-style system status panel (CPU, GPU, RAM, disk, temps)
 *   scenario    — scenario card (name, colour, node, status)
 *   metric      — single metric gauge (circular or bar)
 *   chart       — sparkline chart for time-series data
 *   grid        — multi-machine overview grid (NOC wall)
 *   text        — simple text display (for OLED screens)
 *   clock       — time + date display
 *   custom      — user-defined layout from JSON
 *
 * All templates scale to any size — from 72x72 (Stream Deck key) to 4K.
 */

const { createCanvas } = require('@napi-rs/canvas');

// ── Template registry ───────────────────────────────────────────────────────

const TEMPLATES = {};

function registerTemplate(name, renderFn) {
  TEMPLATES[name] = renderFn;
}

function listTemplates() {
  return Object.keys(TEMPLATES);
}

async function renderTemplate(name, data, width, height, format = 'png') {
  const fn = TEMPLATES[name] || TEMPLATES['text'];
  const canvas = createCanvas(width, height);
  const ctx = canvas.getContext('2d');

  // Default background
  ctx.fillStyle = '#0a0a0f';
  ctx.fillRect(0, 0, width, height);

  await fn(ctx, data, width, height);

  if (format === 'jpeg') {
    return canvas.toBuffer('image/jpeg');
  }
  return canvas.toBuffer('image/png');
}

// ── Helper functions ────────────────────────────────────────────────────────

function drawGauge(ctx, x, y, radius, value, max, label, color, textColor = '#fff') {
  const pct = Math.min(value / max, 1);
  const startAngle = 0.75 * Math.PI;
  const endAngle = 2.25 * Math.PI;
  const sweepAngle = (endAngle - startAngle) * pct;

  // Background arc
  ctx.beginPath();
  ctx.arc(x, y, radius, startAngle, endAngle);
  ctx.strokeStyle = '#222233';
  ctx.lineWidth = radius * 0.15;
  ctx.lineCap = 'round';
  ctx.stroke();

  // Value arc
  ctx.beginPath();
  ctx.arc(x, y, radius, startAngle, startAngle + sweepAngle);
  ctx.strokeStyle = color;
  ctx.lineWidth = radius * 0.15;
  ctx.lineCap = 'round';
  ctx.stroke();

  // Value text
  ctx.fillStyle = textColor;
  ctx.font = `bold ${radius * 0.6}px sans-serif`;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'middle';
  ctx.fillText(Math.round(value).toString(), x, y - radius * 0.1);

  // Label
  ctx.fillStyle = '#888';
  ctx.font = `${radius * 0.3}px sans-serif`;
  ctx.fillText(label, x, y + radius * 0.4);
}

function drawBar(ctx, x, y, w, h, value, max, color) {
  const pct = Math.min(value / max, 1);
  ctx.fillStyle = '#1a1a22';
  ctx.fillRect(x, y, w, h);
  ctx.fillStyle = color;
  ctx.fillRect(x, y, w * pct, h);
}

function drawSparkline(ctx, x, y, w, h, values, color) {
  if (!values || values.length < 2) return;
  const max = Math.max(...values, 1);
  const step = w / (values.length - 1);

  ctx.beginPath();
  ctx.moveTo(x, y + h - (values[0] / max) * h);
  for (let i = 1; i < values.length; i++) {
    ctx.lineTo(x + i * step, y + h - (values[i] / max) * h);
  }
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.5;
  ctx.stroke();
}

// ── Built-in templates ──────────────────────────────────────────────────────

// AIDA64-style system status panel
registerTemplate('status', (ctx, data, w, h) => {
  const isSmall = w < 200;
  const isTiny = w < 100;
  const padding = isTiny ? 4 : isSmall ? 8 : 16;
  const fontSize = isTiny ? 8 : isSmall ? 10 : 14;

  // Title
  const name = data.name || data.node_name || 'System';
  ctx.fillStyle = '#fff';
  ctx.font = `bold ${fontSize * 1.2}px sans-serif`;
  ctx.textAlign = 'center';
  ctx.fillText(name, w / 2, padding + fontSize);

  if (isTiny) {
    // Stream Deck key: just show one metric
    const metric = data.cpu_usage ?? data.cpu_temp ?? 0;
    const label = data.cpu_usage !== undefined ? 'CPU' : 'TEMP';
    drawGauge(ctx, w / 2, h * 0.6, w * 0.3, metric, 100, label, '#5b6fff');
    return;
  }

  const metrics = data.metrics || data;
  let y = padding + fontSize * 2.5;
  const gaugeR = Math.min(w, h) * 0.12;

  if (w >= 300) {
    // Large: gauges in a row
    const gauges = [
      { key: 'cpu_usage', label: 'CPU', max: 100, color: '#5b6fff', unit: '%' },
      { key: 'gpu_usage', label: 'GPU', max: 100, color: '#3ecf8e', unit: '%' },
      { key: 'cpu_temp', label: 'CPU°C', max: 100, color: '#f0b94a', unit: '°' },
      { key: 'gpu_temp', label: 'GPU°C', max: 100, color: '#f26464', unit: '°' },
    ];
    const gCount = gauges.filter(g => metrics[g.key] !== undefined).length || 1;
    const gSpacing = w / (gCount + 1);
    let gx = gSpacing;

    for (const g of gauges) {
      const val = metrics[g.key];
      if (val === undefined) continue;
      drawGauge(ctx, gx, y + gaugeR + 10, gaugeR, val, g.max, g.label, g.color);
      gx += gSpacing;
    }
    y += gaugeR * 2 + 30;
  }

  // Bars for RAM, disk, network
  const bars = [
    { key: 'ram_used', total: 'ram_total', label: 'RAM', color: '#a78bfa' },
    { key: 'disk_used', total: 'disk_total', label: 'Disk', color: '#38bdf8' },
  ];
  for (const b of bars) {
    const used = metrics[b.key];
    const total = metrics[b.total] || 1;
    if (used === undefined) continue;
    const pct = (used / total) * 100;

    ctx.fillStyle = '#888';
    ctx.font = `${fontSize}px sans-serif`;
    ctx.textAlign = 'left';
    ctx.fillText(`${b.label}: ${pct.toFixed(0)}%`, padding, y);
    drawBar(ctx, padding, y + 4, w - padding * 2, fontSize * 0.8, used, total, b.color);
    y += fontSize * 2.5;
  }

  // Network rates
  const rxRate = metrics.net_rx_rate;
  const txRate = metrics.net_tx_rate;
  if (rxRate !== undefined || txRate !== undefined) {
    ctx.fillStyle = '#888';
    ctx.font = `${fontSize}px sans-serif`;
    ctx.textAlign = 'left';
    const rx = rxRate !== undefined ? formatBytes(rxRate) + '/s' : '—';
    const tx = txRate !== undefined ? formatBytes(txRate) + '/s' : '—';
    ctx.fillText(`↓ ${rx}  ↑ ${tx}`, padding, y);
    y += fontSize * 2;
  }

  // Power draw
  const power = metrics.power_draw;
  if (power !== undefined) {
    ctx.fillStyle = '#888';
    ctx.font = `${fontSize}px sans-serif`;
    ctx.fillText(`Power: ${power.toFixed(1)}W`, padding, y);
  }
});

// Scenario card
registerTemplate('scenario', (ctx, data, w, h) => {
  const color = data.color || '#5b6fff';
  const name = data.name || 'Scenario';
  const active = data.active !== false;
  const isTiny = w < 100;

  if (active) {
    ctx.fillStyle = color;
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = luminance(color) > 0.5 ? '#000' : '#fff';
  } else {
    ctx.strokeStyle = color;
    ctx.lineWidth = 2;
    ctx.strokeRect(2, 2, w - 4, h - 4);
    ctx.fillStyle = color;
  }

  ctx.font = `bold ${isTiny ? 10 : 18}px sans-serif`;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'middle';
  ctx.fillText(name, w / 2, h / 2);
});

// Single metric gauge
registerTemplate('metric', (ctx, data, w, h) => {
  const value = data.value ?? 0;
  const max = data.max ?? 100;
  const label = data.label || '';
  const color = data.color || '#5b6fff';
  const r = Math.min(w, h) * 0.35;
  drawGauge(ctx, w / 2, h / 2, r, value, max, label, color);
});

// Sparkline chart
registerTemplate('chart', (ctx, data, w, h) => {
  const values = data.values || [];
  const color = data.color || '#5b6fff';
  const label = data.label || '';
  const padding = 10;

  drawSparkline(ctx, padding, padding, w - padding * 2, h - padding * 3, values, color);

  if (label) {
    ctx.fillStyle = '#888';
    ctx.font = '11px sans-serif';
    ctx.textAlign = 'left';
    ctx.fillText(label, padding, h - 4);
  }
  if (values.length > 0) {
    ctx.fillStyle = '#fff';
    ctx.font = 'bold 13px sans-serif';
    ctx.textAlign = 'right';
    ctx.fillText(values[values.length - 1].toFixed(1), w - padding, h - 4);
  }
});

// Multi-machine grid (NOC wall)
registerTemplate('grid', (ctx, data, w, h) => {
  const machines = data.machines || [];
  if (!machines.length) return;

  const cols = Math.ceil(Math.sqrt(machines.length));
  const rows = Math.ceil(machines.length / cols);
  const cellW = w / cols;
  const cellH = h / rows;

  machines.forEach((m, i) => {
    const col = i % cols;
    const row = Math.floor(i / cols);
    const x = col * cellW;
    const y = row * cellH;

    // Background
    ctx.fillStyle = m.alert ? '#3a1111' : '#111118';
    ctx.fillRect(x + 1, y + 1, cellW - 2, cellH - 2);

    // Name
    ctx.fillStyle = '#fff';
    ctx.font = `bold ${Math.min(cellW, cellH) * 0.12}px sans-serif`;
    ctx.textAlign = 'center';
    ctx.fillText(m.name || `Machine ${i + 1}`, x + cellW / 2, y + cellH * 0.2);

    // CPU gauge
    const cpu = m.cpu_usage ?? 0;
    const gr = Math.min(cellW, cellH) * 0.2;
    drawGauge(ctx, x + cellW / 2, y + cellH * 0.55, gr, cpu, 100, 'CPU', '#5b6fff');

    // Status dot
    const dotColor = m.online ? '#3ecf8e' : '#f26464';
    ctx.beginPath();
    ctx.arc(x + cellW - 8, y + 8, 4, 0, Math.PI * 2);
    ctx.fillStyle = dotColor;
    ctx.fill();
  });
});

// Simple text (for OLEDs)
registerTemplate('text', (ctx, data, w, h) => {
  const text = data.text || '';
  const lines = text.split('\n');
  const fontSize = Math.min(h / (lines.length + 1), w / 8, 24);

  ctx.fillStyle = data.color || '#fff';
  ctx.font = `${fontSize}px monospace`;
  ctx.textAlign = 'left';

  lines.forEach((line, i) => {
    ctx.fillText(line, 4, fontSize * (i + 1));
  });
});

// Clock
registerTemplate('clock', (ctx, data, w, h) => {
  const now = new Date();
  const time = now.toLocaleTimeString('en', { hour: '2-digit', minute: '2-digit', hour12: false });
  const date = now.toLocaleDateString('en', { weekday: 'short', month: 'short', day: 'numeric' });
  const isTiny = w < 100;

  ctx.fillStyle = data.color || '#fff';
  ctx.font = `bold ${isTiny ? 16 : 48}px sans-serif`;
  ctx.textAlign = 'center';
  ctx.textBaseline = 'middle';
  ctx.fillText(time, w / 2, h * 0.4);

  ctx.fillStyle = '#888';
  ctx.font = `${isTiny ? 8 : 16}px sans-serif`;
  ctx.fillText(date, w / 2, h * 0.7);
});

// ── Utilities ───────────────────────────────────────────────────────────────

function formatBytes(bytes) {
  if (bytes < 1024) return bytes + 'B';
  if (bytes < 1048576) return (bytes / 1024).toFixed(1) + 'KB';
  if (bytes < 1073741824) return (bytes / 1048576).toFixed(1) + 'MB';
  return (bytes / 1073741824).toFixed(1) + 'GB';
}

function luminance(hex) {
  const r = parseInt(hex.substr(1, 2), 16) / 255;
  const g = parseInt(hex.substr(3, 2), 16) / 255;
  const b = parseInt(hex.substr(5, 2), 16) / 255;
  return 0.299 * r + 0.587 * g + 0.114 * b;
}

module.exports = { renderTemplate, listTemplates, registerTemplate };
