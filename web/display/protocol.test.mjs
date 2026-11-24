// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the dependency-free Display protobuf codec.
 *
 * Run with: node --test web/display/protocol.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  EmptyFrameError,
  InvalidFrameError,
  POINTER_PHASE,
  TAG_ACTION_INVOKE,
  TAG_DATA_UPDATE,
  TAG_DEFINITION,
  TAG_INTERACTION_EVENT,
  TruncatedFrameError,
  decodeActionInvoke,
  decodeDataUpdate,
  decodeDefinition,
  decodeFieldValue,
  decodeFrame,
  decodeInteractionEvent,
  decodePointerEvent,
  decodeSemanticEvent,
  decodeVarint,
  encodeActionInvoke,
  encodeFrame,
  encodeInteractionEvent,
  encodePointerEvent,
  encodeSemanticEvent,
  encodeVarint,
  readVarint,
  splitFrame,
  zigzagDecode,
  zigzagEncode,
} from './protocol.js';

const encoder = new TextEncoder();

/* ------------------------------------------------------------------------- */
/* Handcrafted protobuf helpers (independent of the implementation)           */
/* ------------------------------------------------------------------------- */

function testVarint(value) {
  let remaining = BigInt(value);
  if (remaining < 0n) throw new RangeError('test varints are unsigned');
  const out = [];
  do {
    let byte = Number(remaining & 0x7fn);
    remaining >>= 7n;
    if (remaining !== 0n) byte |= 0x80;
    out.push(byte);
  } while (remaining !== 0n);
  return Uint8Array.from(out);
}

function concat(...parts) {
  let length = 0;
  for (const part of parts) length += part.length;
  const out = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

function key(fieldNumber, wireType) {
  return testVarint((BigInt(fieldNumber) << 3n) | BigInt(wireType));
}

function fieldVarint(fieldNumber, value) {
  return concat(key(fieldNumber, 0), testVarint(value));
}

function fieldBytes(fieldNumber, value) {
  const bytes = typeof value === 'string' ? encoder.encode(value) : Uint8Array.from(value);
  return concat(key(fieldNumber, 2), testVarint(bytes.length), bytes);
}

function fieldFixed(fieldNumber, bytes) {
  assert.equal(bytes.length, 8);
  return concat(key(fieldNumber, 1), Uint8Array.from(bytes));
}

function fieldDouble(fieldNumber, value) {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setFloat64(0, value, true);
  return concat(key(fieldNumber, 1), bytes);
}

function fieldFixed32(fieldNumber, value) {
  const bytes = new Uint8Array(4);
  new DataView(bytes.buffer).setUint32(0, value, true);
  return concat(key(fieldNumber, 5), bytes);
}

function fieldSInt(fieldNumber, value) {
  const signed = BigInt(value);
  const zigzag = signed >= 0n ? signed << 1n : ((-signed) << 1n) - 1n;
  return fieldVarint(fieldNumber, zigzag);
}

function fieldInt64(fieldNumber, value) {
  return fieldVarint(fieldNumber, BigInt.asUintN(64, BigInt(value)));
}

function fieldValue(fieldId, valueField) {
  return concat(fieldVarint(1, fieldId), valueField);
}

function dataUpdate(...values) {
  return concat(fieldVarint(1, 3), ...values.map((value) => fieldBytes(2, value)));
}

function frame(tag, payload) {
  return concat(Uint8Array.of(tag), payload);
}

/* ------------------------------------------------------------------------- */
/* Varints and zigzag                                                         */
/* ------------------------------------------------------------------------- */

test('varint encoding handles boundary values', () => {
  assert.deepEqual(Array.from(encodeVarint(0)), [0x00]);
  assert.deepEqual(Array.from(encodeVarint(1)), [0x01]);
  assert.deepEqual(Array.from(encodeVarint(127)), [0x7f]);
  assert.deepEqual(Array.from(encodeVarint(128)), [0x80, 0x01]);
  assert.deepEqual(Array.from(encodeVarint(300)), [0xac, 0x02]);

  for (const value of [0n, 1n, 127n, 128n, 300n, 16384n, 0xffffffffffffffffn]) {
    const bytes = encodeVarint(value);
    const decoded = readVarint(bytes);
    assert.equal(decoded.value, value);
    assert.equal(decoded.offset, bytes.length);
  }
});

test('varint decoding reports truncation and invalid lengths', () => {
  assert.throws(() => readVarint(Uint8Array.of(0x80)), TruncatedFrameError);
  assert.throws(() => readVarint(new Uint8Array(11).fill(0x80)), InvalidFrameError);
  assert.throws(() => decodeVarint(Uint8Array.of(0x80, 0x80)), TruncatedFrameError);
});

test('zigzag round-trips signed edge cases', () => {
  const cases = [
    0n,
    -1n,
    1n,
    -2n,
    2n,
    -64n,
    64n,
    2n ** 31n - 1n,
    -(2n ** 31n),
    2n ** 63n - 1n,
    -(2n ** 63n),
  ];
  for (const value of cases) {
    const encoded = zigzagEncode(value);
    assert.equal(zigzagDecode(encoded), value);
  }

  assert.equal(zigzagEncode(-1n), 1n);
  assert.equal(zigzagEncode(1n), 2n);
  assert.equal(zigzagEncode(-2n), 3n);
  assert.equal(zigzagEncode(2n), 4n);
  assert.equal(zigzagDecode(1n), -1n);
  assert.equal(zigzagDecode(2n), 1n);
  assert.equal(zigzagDecode(3n), -2n);

  const decoded = decodeVarint(encodeVarint(zigzagEncode(-123)));
  assert.equal(decoded.value, 245);
  assert.equal(zigzagDecode(decoded.value), -123n);
});

/* ------------------------------------------------------------------------- */
/* DataUpdate / FieldValue                                                    */
/* ------------------------------------------------------------------------- */

test('decodes a DataUpdate with every FieldValue shape', () => {
  const payload = concat(
    fieldVarint(1, 7),
    fieldBytes(2, fieldValue(1, fieldSInt(2, -5))),
    fieldBytes(2, fieldValue(2, fieldVarint(3, 1200))),
    fieldBytes(2, fieldValue(3, fieldDouble(4, 65.5))),
    fieldBytes(2, fieldValue(4, fieldVarint(5, 1))),
    fieldBytes(2, fieldValue(5, fieldBytes(6, 'Gaming 🎮'))),
    fieldBytes(2, fieldValue(6, fieldVarint(7, 2))),
    fieldBytes(2, fieldValue(7, fieldBytes(8, [1, 2, 3]))),
    fieldBytes(2, fieldValue(8, fieldInt64(9, -500))),
    fieldVarint(3, 9001),
    fieldVarint(4, 1_700_000_000_000_000n),
  );

  const decoded = decodeFrame(frame(TAG_DATA_UPDATE, payload));
  assert.equal(decoded.tag, TAG_DATA_UPDATE);
  assert.equal(decoded.kind, 'data_update');
  assert.equal(decoded.dataUpdate.definitionVersion, 7);
  assert.equal(decoded.dataUpdate.sequence, 9001);
  assert.equal(decoded.dataUpdate.timestampMicros, 1_700_000_000_000_000);
  assert.equal(decoded.dataUpdate.values.length, 8);

  assert.deepEqual(decoded.dataUpdate.values[0], {
    fieldId: 1,
    kind: 'integer',
    value: -5,
  });
  assert.deepEqual(decoded.dataUpdate.values[1], {
    fieldId: 2,
    kind: 'unsigned',
    value: 1200,
  });
  assert.deepEqual(decoded.dataUpdate.values[2], {
    fieldId: 3,
    kind: 'real',
    value: 65.5,
  });
  assert.deepEqual(decoded.dataUpdate.values[3], {
    fieldId: 4,
    kind: 'flag',
    value: true,
  });
  assert.deepEqual(decoded.dataUpdate.values[4], {
    fieldId: 5,
    kind: 'text',
    value: 'Gaming 🎮',
  });
  assert.deepEqual(decoded.dataUpdate.values[5], {
    fieldId: 6,
    kind: 'enum',
    value: 2,
  });
  assert.deepEqual(decoded.dataUpdate.values[6], {
    fieldId: 7,
    kind: 'blob',
    value: Uint8Array.from([1, 2, 3]),
  });
  assert.deepEqual(decoded.dataUpdate.values[7], {
    fieldId: 8,
    kind: 'timestamp',
    value: -500,
  });
});

test('decodes FieldValue directly and tolerates wrong-wire known fields', () => {
  // field_id=9, text field 6 encoded as varint (wrong wire type), then a valid
  // real field 4.  The mismatched field must be skipped, not rejected.
  const payload = concat(
    fieldVarint(1, 9),
    fieldVarint(6, 123),
    fieldDouble(4, -1.25),
  );
  const decoded = decodeFieldValue(payload);
  assert.equal(decoded.fieldId, 9);
  assert.equal(decoded.kind, 'real');
  assert.equal(decoded.value, -1.25);
});

test('decodes a DataUpdate with unknown fields and a group', () => {
  const payload = concat(
    fieldVarint(1, 2),
    fieldBytes(2, fieldValue(1, fieldVarint(3, 42))),
    fieldVarint(20, 123),
    fieldFixed(21, [1, 2, 3, 4, 5, 6, 7, 8]),
    fieldBytes(22, 'unknown'),
    fieldFixed32(23, 0xdeadbeef),
    key(24, 3),
    fieldVarint(1, 5),
    key(24, 4),
  );

  const decoded = decodeDataUpdate(payload);
  assert.equal(decoded.definitionVersion, 2);
  assert.deepEqual(decoded.values, [
    { fieldId: 1, kind: 'unsigned', value: 42 },
  ]);
});

/* ------------------------------------------------------------------------- */
/* DisplayDefinition                                                          */
/* ------------------------------------------------------------------------- */

test('decodes DisplayDefinition JSON and ignores unknown fields', () => {
  const document = {
    id: 'screen-1',
    version: 7,
    fields: [{ id: 1, key: 'org.ozma/node.cpu_temp', type: 'real', unit: 'celsius' }],
    actions: [],
    layout: { type: 'column', children: [{ type: 'text', text: 'CPU {org.ozma/node.cpu_temp}' }] },
  };

  const payload = concat(
    fieldBytes(1, 'screen-1'),
    fieldVarint(2, 7),
    fieldBytes(3, [9, 8, 7]),
    fieldBytes(4, JSON.stringify(document)),
    fieldBytes(5, [0x28, 0xb5, 0x2f]),
    fieldVarint(50, 123),
    fieldBytes(51, 'ignored'),
    fieldVarint(6, 0),
  );

  const decoded = decodeFrame(frame(TAG_DEFINITION, payload));
  assert.equal(decoded.tag, TAG_DEFINITION);
  assert.equal(decoded.kind, 'definition');
  assert.equal(decoded.definition.id, 'screen-1');
  assert.equal(decoded.definition.version, 7);
  assert.deepEqual(Array.from(decoded.definition.hash), [9, 8, 7]);
  assert.equal(decoded.definition.compressed, true);
  assert.deepEqual(decoded.definition.definition, document);
  assert.equal(decoded.definition.definitionJson, JSON.stringify(document));
});

test('rejects invalid definition JSON', () => {
  const payload = fieldBytes(4, 'not json');
  assert.throws(
    () => decodeDefinition(payload),
    (error) => error instanceof InvalidFrameError && /not valid JSON/.test(error.message),
  );
});

/* ------------------------------------------------------------------------- */
/* ActionInvoke                                                               */
/* ------------------------------------------------------------------------- */

test('ActionInvoke round-trips through the frame codec', () => {
  const params = { scenario: 'gaming', level: 3, enabled: true };
  const idempotencyKey = 1_234_567_890_123n;

  const encoded = encodeActionInvoke({
    actionId: 42,
    params,
    idempotencyKey,
  });
  const encodedFrame = encodeFrame(TAG_ACTION_INVOKE, encoded);
  const decoded = decodeFrame(encodedFrame);

  assert.equal(decoded.tag, TAG_ACTION_INVOKE);
  assert.equal(decoded.kind, 'action_invoke');
  assert.equal(decoded.actionInvoke.actionId, 42);
  assert.deepEqual(decoded.actionInvoke.params, params);
  assert.equal(decoded.actionInvoke.idempotencyKey, 1_234_567_890_123);
  assert.equal(decoded.actionInvoke.timestampMicros, null);

  const direct = decodeActionInvoke(encoded);
  assert.deepEqual(direct, decoded.actionInvoke);
});

test('ActionInvoke accepts prior JSON text and raw parameter bytes', () => {
  const fromString = decodeActionInvoke(encodeActionInvoke({
    actionId: 1,
    params: '{"ok":true}',
  }));
  assert.deepEqual(fromString.params, { ok: true });

  const fromBytes = decodeActionInvoke(encodeActionInvoke({
    actionId: 2,
    params: Uint8Array.from([0x7b, 0x7d]),
  }));
  assert.deepEqual(fromBytes.params, {});
});

/* ------------------------------------------------------------------------- */
/* InteractionEvent                                                           */
/* ------------------------------------------------------------------------- */

test('encodes and decodes pointer, semantic, and nested action interactions', () => {
  const pointerFrame = encodeFrame(
    TAG_INTERACTION_EVENT,
    encodeInteractionEvent({
      nodeId: 99,
      targetId: 'button-1',
      timestampMicros: 123456789,
      pointer: {
        x: -10,
        y: 20,
        phase: POINTER_PHASE.DOWN,
        button: 0,
        pressurePpt: 500,
        wheelDelta: -2,
      },
    }),
  );
  const pointer = decodeFrame(pointerFrame).interactionEvent;
  assert.equal(pointer.type, 'pointer');
  assert.equal(pointer.nodeId, 99);
  assert.equal(pointer.targetId, 'button-1');
  assert.equal(pointer.timestampMicros, 123456789);
  assert.deepEqual(pointer.pointer, {
    x: -10,
    y: 20,
    phase: POINTER_PHASE.DOWN,
    button: 0,
    pressurePpt: 500,
    wheelDelta: -2,
  });

  const semanticPayload = encodeInteractionEvent({
    nodeId: 5,
    semantic: { name: 'media.key', params: { key: 'play', source: 'browser' } },
  });
  const semantic = decodeInteractionEvent(semanticPayload);
  assert.equal(semantic.type, 'semantic');
  assert.deepEqual(semantic.semantic, {
    name: 'media.key',
    params: { key: 'play', source: 'browser' },
  });

  const actionPayload = encodeInteractionEvent({
    nodeId: 6,
    action: { actionId: 11, params: { direction: 'up' } },
  });
  const action = decodeInteractionEvent(actionPayload);
  assert.equal(action.type, 'action');
  assert.equal(action.action.actionId, 11);
  assert.deepEqual(action.action.params, { direction: 'up' });
});

test('pointer and semantic payload encoders can be used directly', () => {
  const pointer = decodePointerEvent(encodePointerEvent({ x: 1, y: 2 }));
  assert.equal(pointer.x, 1);
  assert.equal(pointer.y, 2);

  const semantic = decodeSemanticEvent(
    encodeSemanticEvent({ name: 'n', params: { a: 'b' } }),
  );
  assert.deepEqual(semantic, { name: 'n', params: { a: 'b' } });
});

/* ------------------------------------------------------------------------- */
/* Unknown tags and malformed frames                                          */
/* ------------------------------------------------------------------------- */

test('unknown display tags are returned without throwing', () => {
  const payload = Uint8Array.from([1, 2, 3]);
  const decoded = decodeFrame(encodeFrame(200, payload));
  assert.equal(decoded.tag, 200);
  assert.equal(decoded.kind, null);
  assert.deepEqual(Array.from(decoded.payload), [1, 2, 3]);
});

test('splitFrame enforces the header and payload limits', () => {
  assert.throws(() => splitFrame(new Uint8Array()), EmptyFrameError);
  assert.deepEqual(splitFrame(Uint8Array.of(7, 1, 2)), {
    tag: 7,
    payload: Uint8Array.from([1, 2]),
  });
});

test('truncated and invalid frames throw protocol errors', () => {
  assert.throws(() => decodeFrame(new Uint8Array()), EmptyFrameError);

  // DataUpdate field 1 with a length-delimited payload claiming 5 bytes.
  assert.throws(
    () => decodeFrame(Uint8Array.of(TAG_DATA_UPDATE, 0x0a, 0x05, 0x01, 0x02)),
    TruncatedFrameError,
  );

  // Definition field 4 claims more JSON bytes than are present.
  assert.throws(
    () => decodeFrame(frame(TAG_DEFINITION, concat(key(4, 2), testVarint(5), Uint8Array.of(0x7b)))),
    TruncatedFrameError,
  );

  // FieldValue text field claims more bytes than are present.
  const badFieldValue = fieldValue(1, concat(key(6, 2), testVarint(10), Uint8Array.of(0x61)));
  assert.throws(
    () => decodeFrame(frame(TAG_DATA_UPDATE, dataUpdate(badFieldValue))),
    TruncatedFrameError,
  );

  // Fixed64 real field with only four bytes present.
  const badDouble = fieldValue(1, concat(key(4, 1), Uint8Array.of(1, 2, 3, 4)));
  assert.throws(
    () => decodeFrame(frame(TAG_DATA_UPDATE, dataUpdate(badDouble))),
    TruncatedFrameError,
  );

  // A zero key is not a valid protobuf field.
  assert.throws(
    () => decodeFrame(Uint8Array.of(TAG_DATA_UPDATE, 0x00)),
    InvalidFrameError,
  );

  // Wire types 6 and 7 are invalid.
  assert.throws(
    () => decodeFrame(Uint8Array.of(TAG_DATA_UPDATE, 0x0e)),
    InvalidFrameError,
  );

  // Unknown field with an unexpected end-group tag.
  assert.throws(
    () => decodeFrame(frame(TAG_DATA_UPDATE, key(24, 4))),
    InvalidFrameError,
  );
});
