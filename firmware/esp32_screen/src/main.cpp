/**
 * Ozma ESP32 Screen Firmware
 *
 * Connects to the ozma controller via WebSocket, receives a UI layout
 * definition, then renders widgets locally from data-only updates.
 *
 * This achieves 60fps rendering for VU meters and gauges — impossible
 * with server-side frame push over WiFi.
 */

#include <Arduino.h>
#include <WiFi.h>
#include <WebSocketsClient.h>
#include <ArduinoJson.h>
#include <TFT_eSPI.h>

// ── Configuration ───────────────────────────────────────────────────────────

const char* WIFI_SSID = "ozma-network";
const char* WIFI_PASS = "ozmapass";
const char* CONTROLLER_HOST = "10.0.100.1";
const int   CONTROLLER_PORT = 7391;
const char* DEVICE_ID = "esp32-desk-1";
const int   SCREEN_WIDTH = 240;
const int   SCREEN_HEIGHT = 320;

// ── Globals ─────────────────────────────────────────────────────────────────

TFT_eSPI tft = TFT_eSPI();
WebSocketsClient ws;

// Current data values (updated from WebSocket)
StaticJsonDocument<4096> currentData;

// Layout definition (received once from controller)
StaticJsonDocument<8192> layoutDef;
bool layoutReceived = false;

// Scenario colour
uint16_t scenarioColor = TFT_BLUE;

// ── Widget rendering ────────────────────────────────────────────────────────

uint16_t hexToColor565(const char* hex) {
  if (!hex || hex[0] != '#' || strlen(hex) < 7) return TFT_WHITE;
  long rgb = strtol(hex + 1, NULL, 16);
  uint8_t r = (rgb >> 16) & 0xFF;
  uint8_t g = (rgb >> 8) & 0xFF;
  uint8_t b = rgb & 0xFF;
  return tft.color565(r, g, b);
}

void drawGauge(JsonObject w) {
  int x = w["x"].as<int>() + w["w"].as<int>() / 2;
  int y = w["y"].as<int>() + w["h"].as<int>() / 2;
  int r = min(w["w"].as<int>(), w["h"].as<int>()) / 2 - 4;

  const char* metric = w["metric"] | "";
  float value = currentData[metric] | 0.0f;
  float maxVal = w["max"] | 100.0f;
  float pct = constrain(value / maxVal, 0.0f, 1.0f);

  uint16_t color = hexToColor565(w["color"] | "#5b6fff");

  // Background arc
  for (int a = -135; a <= 135; a += 3) {
    float rad = a * DEG_TO_RAD;
    tft.drawPixel(x + r * cos(rad), y + r * sin(rad), TFT_DARKGREY);
  }

  // Value arc
  int endAngle = -135 + (int)(270 * pct);
  for (int a = -135; a <= endAngle; a += 2) {
    float rad = a * DEG_TO_RAD;
    for (int t = -2; t <= 2; t++) {
      tft.drawPixel(x + (r + t) * cos(rad), y + (r + t) * sin(rad), color);
    }
  }

  // Value text
  tft.setTextColor(TFT_WHITE, TFT_BLACK);
  tft.setTextDatum(MC_DATUM);
  tft.setTextSize(2);
  tft.drawFloat(value, 1, x, y - 5);

  // Label
  const char* label = w["label"] | "";
  if (strlen(label) > 0) {
    tft.setTextSize(1);
    tft.setTextColor(TFT_DARKGREY);
    tft.drawString(label, x, y + r * 0.5);
  }
}

void drawBar(JsonObject w) {
  int x = w["x"].as<int>();
  int y = w["y"].as<int>();
  int bw = w["w"].as<int>();
  int bh = w["h"].as<int>();

  const char* metric = w["metric"] | "";
  float value = currentData[metric] | 0.0f;
  float maxVal = w["max"] | 100.0f;
  float pct = constrain(value / maxVal, 0.0f, 1.0f);

  uint16_t color = hexToColor565(w["color"] | "#a78bfa");

  tft.fillRect(x, y, bw, bh, TFT_DARKGREY);
  tft.fillRect(x, y, (int)(bw * pct), bh, color);

  // Label
  const char* label = w["label"] | "";
  if (strlen(label) > 0) {
    tft.setTextColor(TFT_WHITE);
    tft.setTextSize(1);
    tft.setTextDatum(ML_DATUM);
    tft.drawString(label, x + 2, y + bh / 2);
  }
}

void drawVUMeter(JsonObject w) {
  int x = w["x"].as<int>();
  int y = w["y"].as<int>();
  int bw = w["w"].as<int>();
  int bh = w["h"].as<int>();
  int segments = w["segments"] | 20;

  const char* metric = w["metric"] | "";
  float value = constrain(currentData[metric] | 0.0f, 0.0f, 1.0f);

  int activeSegs = (int)(segments * value);
  int segH = bh / segments - 1;

  for (int i = 0; i < segments; i++) {
    int sy = y + bh - (i + 1) * (segH + 1);
    uint16_t color;
    if (i >= segments * 0.85) color = TFT_RED;
    else if (i >= segments * 0.7) color = TFT_YELLOW;
    else color = TFT_GREEN;

    if (i < activeSegs) {
      tft.fillRect(x, sy, bw, segH, color);
    } else {
      tft.fillRect(x, sy, bw, segH, tft.color565(20, 20, 30));
    }
  }
}

void drawLabel(JsonObject w) {
  const char* text = w["text"] | "";
  int x = w["x"].as<int>();
  int y = w["y"].as<int>();
  int fontSize = w["font_size"] | 14;

  uint16_t color = hexToColor565(w["color"] | "#ffffff");
  tft.setTextColor(color, TFT_BLACK);
  tft.setTextSize(fontSize > 16 ? 2 : 1);
  tft.setTextDatum(TL_DATUM);

  // Simple variable interpolation
  String s(text);
  for (JsonPair kv : currentData.as<JsonObject>()) {
    String key = String("{") + kv.key().c_str() + "}";
    if (kv.value().is<float>()) {
      s.replace(key, String(kv.value().as<float>(), 1));
    } else if (kv.value().is<const char*>()) {
      s.replace(key, kv.value().as<const char*>());
    }
  }
  tft.drawString(s, x, y);
}

void drawNumber(JsonObject w) {
  const char* metric = w["metric"] | "";
  float value = currentData[metric] | 0.0f;
  int x = w["x"].as<int>() + w["w"].as<int>() / 2;
  int y = w["y"].as<int>() + w["h"].as<int>() / 2;
  int decimals = w["decimals"] | 1;

  uint16_t color = hexToColor565(w["color"] | "#ffffff");
  tft.setTextColor(color, TFT_BLACK);
  tft.setTextDatum(MC_DATUM);
  tft.setTextSize(3);
  tft.drawFloat(value, decimals, x, y - 10);

  const char* unit = w["unit"] | "";
  if (strlen(unit) > 0) {
    tft.setTextSize(1);
    tft.setTextColor(TFT_DARKGREY);
    tft.drawString(unit, x, y + 15);
  }
}

// ── Layout renderer ─────────────────────────────────────────────────────────

void renderLayout() {
  if (!layoutReceived) return;

  JsonArray widgets = layoutDef["layout"]["widgets"];
  for (JsonObject w : widgets) {
    const char* type = w["type"] | "";

    if (strcmp(type, "gauge") == 0) drawGauge(w);
    else if (strcmp(type, "bar") == 0) drawBar(w);
    else if (strcmp(type, "vu_meter") == 0) drawVUMeter(w);
    else if (strcmp(type, "label") == 0) drawLabel(w);
    else if (strcmp(type, "number") == 0) drawNumber(w);
  }
}

// ── WebSocket handler ───────────────────────────────────────────────────────

void wsEvent(WStype_t type, uint8_t* payload, size_t length) {
  switch (type) {
    case WStype_CONNECTED: {
      Serial.println("WebSocket connected");
      // Register with controller
      StaticJsonDocument<512> reg;
      reg["type"] = "register";
      reg["device_id"] = DEVICE_ID;
      reg["width"] = SCREEN_WIDTH;
      reg["height"] = SCREEN_HEIGHT;
      JsonArray caps = reg.createNestedArray("capabilities");
      caps.add("gauge"); caps.add("bar"); caps.add("vu_meter");
      caps.add("label"); caps.add("number"); caps.add("sparkline");
      String msg;
      serializeJson(reg, msg);
      ws.sendTXT(msg);
      break;
    }

    case WStype_TEXT: {
      StaticJsonDocument<8192> doc;
      DeserializationError err = deserializeJson(doc, payload, length);
      if (err) break;

      const char* msgType = doc["type"] | "";

      if (strcmp(msgType, "layout") == 0) {
        layoutDef = doc;
        layoutReceived = true;
        // Clear screen and draw background
        uint16_t bg = hexToColor565(doc["layout"]["background"] | "#0a0a0f");
        tft.fillScreen(bg);
        Serial.println("Layout received");
      }
      else if (strcmp(msgType, "data") == 0) {
        currentData = doc["d"];
      }
      else if (strcmp(msgType, "scenario") == 0) {
        scenarioColor = hexToColor565(doc["color"] | "#5b6fff");
        currentData["scenario_name"] = doc["name"] | "";
        currentData["scenario_color"] = doc["color"] | "#5b6fff";
      }
      break;
    }

    case WStype_DISCONNECTED:
      Serial.println("WebSocket disconnected");
      layoutReceived = false;
      break;

    default:
      break;
  }
}

// ── Setup & loop ────────────────────────────────────────────────────────────

void setup() {
  Serial.begin(115200);

  // Display init
  tft.init();
  tft.setRotation(0);
  tft.fillScreen(TFT_BLACK);
  tft.setTextColor(TFT_WHITE);
  tft.setTextDatum(MC_DATUM);
  tft.drawString("Ozma Screen", SCREEN_WIDTH / 2, SCREEN_HEIGHT / 2 - 10);
  tft.drawString("Connecting...", SCREEN_WIDTH / 2, SCREEN_HEIGHT / 2 + 10);

  // WiFi
  WiFi.begin(WIFI_SSID, WIFI_PASS);
  while (WiFi.status() != WL_CONNECTED) {
    delay(500);
    Serial.print(".");
  }
  Serial.println("\nWiFi connected: " + WiFi.localIP().toString());

  // WebSocket
  ws.begin(CONTROLLER_HOST, CONTROLLER_PORT, "/");
  ws.onEvent(wsEvent);
  ws.setReconnectInterval(2000);
}

void loop() {
  ws.loop();
  renderLayout();
  delay(16); // ~60fps
}
