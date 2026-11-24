// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Dependency-free codec for the Ozma Display protobuf protocol.
 *
 * A Display frame is a one-byte tag followed by a protobuf message body:
 *
 *     +---------+----------------------------+
 *     | tag (1) | protobuf payload (<=8 MiB) |
 *     +---------+----------------------------+
 *
 * This module has no DOM dependencies and is safe to use from Node (tests) or
 * from a browser ES module.  It intentionally implements only the parts of the
 * schema needed by a browser display adapter: DisplayDefinition (tag 1),
 * DataUpdate (tag 5), ActionInvoke (tag 6), and InteractionEvent (tag 7).
 * Unknown tags and unknown protobuf fields are ignored.
 */

export const TAG_DEFINITION = 0x01;
export const TAG_DEFINITION_REF = 0x02;
export const TAG_DEFINITION_PATCH = 0x03;
export const TAG_SCENE_COMMIT = 0x04;
export const TAG_DATA_UPDATE = 0x05;
export const TAG_ACTION_INVOKE = 0x06;
export const TAG_INTERACTION_EVENT = 0x07;
export const TAG_SCENE_SNAPSHOT = 0x08;

export const TAG_NAMES = Object.freeze({
  [TAG_DEFINITION]: 'definition',
  [TAG_DEFINITION_REF]: 'definition_ref',
  [TAG_DEFINITION_PATCH]: 'definition_patch',
  [TAG_SCENE_COMMIT]: 'scene_commit',
  [TAG_DATA_UPDATE]: 'data_update',
  [TAG_ACTION_INVOKE]: 'action_invoke',
  [TAG_INTERACTION_EVENT]: 'interaction_event',
  [TAG_SCENE_SNAPSHOT]: 'scene_snapshot',
});

export const WIRE_VARINT = 0;
export const WIRE_FIXED64 = 1;
export const WIRE_LEN = 2;
export const WIRE_START_GROUP = 3;
export const WIRE_END_GROUP = 4;
export const WIRE_FIXED32 = 5;

export const HEADER_LEN = 1;
export const MAX_PAYLOAD_LEN = 8 * 1024 * 1024;

export const FIELD_VALUE_KIND = Object.freeze({
  INTEGER: 'integer',
  UNSIGNED: 'unsigned',
  REAL: 'real',
  FLAG: 'flag',
  TEXT: 'text',
  ENUM: 'enum',
  BLOB: 'blob',
  TIMESTAMP: 'timestamp',
});

export const POINTER_PHASE = Object.freeze({
  UNSPECIFIED: 0,
  DOWN: 1,
  MOVE: 2,
  UP: 3,
  CANCEL: 4,
  WHEEL: 5,
});

const TEXT_ENCODER = new TextEncoder();
const TEXT_DECODER = new TextDecoder('utf-8');
const UINT64_MASK = 0xffffffffffffffffn;
const MAX_SAFE_BIGINT = BigInt(Number.MAX_SAFE_INTEGER);
const MIN_SAFE_BIGINT = BigInt(Number.MIN_SAFE_INTEGER);

export class DisplayProtocolError extends Error {
  constructor(message) {
    super(message);
    this.name = 'DisplayProtocolError';
  }
}

export class EmptyFrameError extends DisplayProtocolError {
  constructor(message = 'empty display frame: expected a one-byte tag') {
    super(message);
    this.name = 'EmptyFrameError';
  }
}

export class TruncatedFrameError extends DisplayProtocolError {
  constructor(message) {
    super(message);
    this.name = 'TruncatedFrameError';
  }
}

export class InvalidFrameError extends DisplayProtocolError {
  constructor(message) {
    super(message);
    this.name = 'InvalidFrameError';
  }
}

/* ------------------------------------------------------------------------- */
/* Generic byte helpers                                                       */
/* ------------------------------------------------------------------------- */

/** Coerce an ArrayBuffer, typed array, DataView, or byte array to Uint8Array. */
export function asBytes(input) {
  if (input instanceof Uint8Array) return input;
  if (input instanceof ArrayBuffer) return new Uint8Array(input);
  if (ArrayBuffer.isView(input)) {
    return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
  }
  if (Array.isArray(input)) return Uint8Array.from(input);
  throw new TypeError('expected an ArrayBuffer, typed array, DataView, or byte array');
}

export function concatBytes(parts) {
  const arrays = parts.map(asBytes);
  let length = 0;
  for (const part of arrays) length += part.length;
  const out = new Uint8Array(length);
  let offset = 0;
  for (const part of arrays) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

function toBigInt(value, label = 'value') {
  if (typeof value === 'bigint') return value;
  if (typeof value === 'number') {
    if (!Number.isFinite(value) || !Number.isInteger(value)) {
      throw new TypeError(`${label} must be an integer`);
    }
    return BigInt(value);
  }
  if (typeof value === 'string' && /^-?\d+$/.test(value.trim())) {
    return BigInt(value.trim());
  }
  throw new TypeError(`${label} must be an integer`);
}

function bigintToValue(value) {
  return value >= MIN_SAFE_BIGINT && value <= MAX_SAFE_BIGINT
    ? Number(value)
    : value;
}

function appendText(bytes) {
  return TEXT_DECODER.decode(bytes);
}

/* ------------------------------------------------------------------------- */
/* Varints and zigzag                                                         */
/* ------------------------------------------------------------------------- */

/** Encode a non-negative integer as a protobuf base-128 varint. */
export function encodeVarint(value) {
  let remaining = toBigInt(value, 'varint value');
  if (remaining < 0n) {
    throw new RangeError('varint value must be non-negative');
  }
  const out = [];
  do {
    let byte = Number(remaining & 0x7fn);
    remaining >>= 7n;
    if (remaining !== 0n) byte |= 0x80;
    out.push(byte);
  } while (remaining !== 0n);
  return Uint8Array.from(out);
}

/**
 * Read a varint.  Returns `{ value: bigint, offset }`.
 *
 * A varint has at most ten bytes.  Truncated input and values wider than
 * 64 bits are rejected.
 */
export function readVarint(bytesLike, offset = 0, end) {
  const bytes = asBytes(bytesLike);
  const limit = end === undefined ? bytes.length : end;
  if (!Number.isInteger(offset) || offset < 0 || offset > limit) {
    throw new RangeError('invalid varint offset');
  }
  let result = 0n;
  let shift = 0n;
  for (let index = 0; index < 10; index += 1) {
    if (offset >= limit) {
      throw new TruncatedFrameError(`truncated varint at byte ${offset}`);
    }
    const byte = bytes[offset];
    offset += 1;
    result |= BigInt(byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) {
      if (result > UINT64_MASK) {
        throw new InvalidFrameError('varint value exceeds 64 bits');
      }
      return { value: result, offset };
    }
    shift += 7n;
  }
  throw new InvalidFrameError('varint is longer than ten bytes');
}

/** Convenience wrapper returning a number when safe, otherwise a bigint. */
export function decodeVarint(bytesLike, offset = 0, end) {
  const { value, offset: next } = readVarint(bytesLike, offset, end);
  return { value: bigintToValue(value), offset: next };
}

/** Map a signed integer to its unsigned zigzag representation. */
export function zigzagEncode(value) {
  const signed = toBigInt(value, 'zigzag value');
  return signed >= 0n ? signed << 1n : ((-signed) << 1n) - 1n;
}

/** Map an unsigned zigzag representation back to a signed integer. */
export function zigzagDecode(value) {
  const unsigned = toBigInt(value, 'zigzag value');
  if (unsigned < 0n) throw new RangeError('zigzag value must be non-negative');
  return (unsigned >> 1n) ^ -(unsigned & 1n);
}

function fieldKey(fieldNumber, wireType) {
  if (!Number.isInteger(fieldNumber) || fieldNumber < 1 || fieldNumber > 0x1fffffff) {
    throw new RangeError(`invalid protobuf field number ${fieldNumber}`);
  }
  return encodeVarint((BigInt(fieldNumber) << 3n) | BigInt(wireType));
}

function readKey(bytes, offset, end) {
  const { value, offset: next } = readVarint(bytes, offset, end);
  if (value === 0n) throw new InvalidFrameError('protobuf key is zero');
  if (value > 0xffffffffn) throw new InvalidFrameError('protobuf key is too large');
  const key = Number(value);
  const wireType = key & 0x7;
  const fieldNumber = Math.floor(key / 8);
  if (fieldNumber < 1) throw new InvalidFrameError('protobuf field number is zero');
  if (wireType > WIRE_FIXED32) {
    throw new InvalidFrameError(`invalid protobuf wire type ${wireType}`);
  }
  return { fieldNumber, wireType, offset: next };
}

function ensureAvailable(offset, count, end, what) {
  if (offset + count > end) {
    throw new TruncatedFrameError(`truncated ${what}`);
  }
}

function readLength(bytes, offset, end) {
  const { value, offset: next } = readVarint(bytes, offset, end);
  if (value > BigInt(end - next)) {
    throw new TruncatedFrameError('length-delimited field extends past the payload');
  }
  return { length: Number(value), offset: next };
}

function readLengthDelimited(bytes, offset, end, what = 'length-delimited field') {
  const { length, offset: next } = readLength(bytes, offset, end);
  if (next + length > end) {
    throw new TruncatedFrameError(`truncated ${what}`);
  }
  return { value: bytes.subarray(next, next + length), offset: next + length };
}

function readString(bytes, offset, end) {
  const field = readLengthDelimited(bytes, offset, end, 'string field');
  return { value: appendText(field.value), offset: field.offset };
}

function skipField(bytes, offset, end, fieldNumber, wireType, depth = 0) {
  if (depth > 64) throw new InvalidFrameError('protobuf group nesting is too deep');
  switch (wireType) {
    case WIRE_VARINT:
      return readVarint(bytes, offset, end).offset;
    case WIRE_FIXED64:
      ensureAvailable(offset, 8, end, '64-bit field');
      return offset + 8;
    case WIRE_LEN: {
      const { length, offset: next } = readLength(bytes, offset, end);
      if (next + length > end) {
        throw new TruncatedFrameError('truncated length-delimited field');
      }
      return next + length;
    }
    case WIRE_START_GROUP: {
      let cursor = offset;
      while (true) {
        const key = readKey(bytes, cursor, end);
        cursor = key.offset;
        if (key.wireType === WIRE_END_GROUP) {
          if (key.fieldNumber !== fieldNumber) {
            throw new InvalidFrameError('mismatched protobuf end-group');
          }
          return cursor;
        }
        cursor = skipField(
          bytes,
          cursor,
          end,
          key.fieldNumber,
          key.wireType,
          depth + 1,
        );
      }
    }
    case WIRE_END_GROUP:
      throw new InvalidFrameError('unexpected protobuf end-group');
    case WIRE_FIXED32:
      ensureAvailable(offset, 4, end, '32-bit field');
      return offset + 4;
    default:
      throw new InvalidFrameError(`invalid protobuf wire type ${wireType}`);
  }
}

function encodeKeyedVarint(fieldNumber, value) {
  return concatBytes([fieldKey(fieldNumber, WIRE_VARINT), encodeVarint(value)]);
}

function encodeKeyedString(fieldNumber, value) {
  const bytes = TEXT_ENCODER.encode(String(value));
  return concatBytes([
    fieldKey(fieldNumber, WIRE_LEN),
    encodeVarint(bytes.length),
    bytes,
  ]);
}

function encodeKeyedBytes(fieldNumber, value) {
  const bytes = asBytes(value);
  return concatBytes([
    fieldKey(fieldNumber, WIRE_LEN),
    encodeVarint(bytes.length),
    bytes,
  ]);
}

function encodeKeyedDouble(fieldNumber, value) {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setFloat64(0, Number(value), true);
  return concatBytes([fieldKey(fieldNumber, WIRE_FIXED64), bytes]);
}

function encodeKeyedZigzag(fieldNumber, value) {
  return encodeKeyedVarint(fieldNumber, zigzagEncode(value));
}

function encodeKeyedInt64(fieldNumber, value) {
  const signed = toBigInt(value, 'signed 64-bit value');
  return encodeKeyedVarint(fieldNumber, BigInt.asUintN(64, signed));
}

/* ------------------------------------------------------------------------- */
/* DisplayDefinition (tag 1)                                                  */
/* ------------------------------------------------------------------------- */

/**
 * Decode a DisplayDefinition payload.
 *
 * Returns `{ id, version, hash, definitionJson, definitionJsonBytes,
 * definition, compressed }`.  `definition` is the parsed definition JSON, or
 * null when only a compressed payload was supplied.
 */
export function decodeDefinition(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  const result = {
    id: '',
    version: 0,
    hash: null,
    definitionJson: null,
    definitionJsonBytes: null,
    definition: null,
    compressed: false,
  };

  while (offset < end) {
    const key = readKey(bytes, offset, end);
    offset = key.offset;
    const { fieldNumber, wireType } = key;

    if (fieldNumber === 1 && wireType === WIRE_LEN) {
      const field = readString(bytes, offset, end);
      result.id = field.value;
      offset = field.offset;
    } else if (fieldNumber === 2 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.version = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 3 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'definition hash');
      result.hash = field.value;
      offset = field.offset;
    } else if (fieldNumber === 4 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'definition_json');
      result.definitionJsonBytes = field.value;
      result.definitionJson = appendText(field.value);
      offset = field.offset;
    } else if (fieldNumber === 5 && wireType === WIRE_LEN) {
      // A zstd-compressed definition.  Browsers have no built-in zstd
      // decoder, so keep the flag but do not attempt decompression.
      const field = readLengthDelimited(bytes, offset, end, 'definition_zstd');
      result.compressed = true;
      offset = field.offset;
    } else {
      offset = skipField(bytes, offset, end, fieldNumber, wireType);
    }
  }

  if (result.definitionJson !== null) {
    try {
      result.definition = JSON.parse(result.definitionJson);
    } catch (error) {
      throw new InvalidFrameError(`definition_json is not valid JSON: ${error.message}`);
    }
  }
  return result;
}

/* ------------------------------------------------------------------------- */
/* DataUpdate / FieldValue (tag 5)                                            */
/* ------------------------------------------------------------------------- */

/** Decode one FieldValue message (no frame tag). */
export function decodeFieldValue(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  const result = { fieldId: 0, kind: null, value: undefined };

  while (offset < end) {
    const key = readKey(bytes, offset, end);
    offset = key.offset;
    const { fieldNumber, wireType } = key;

    if (fieldNumber === 1) {
      if (wireType !== WIRE_VARINT) {
        offset = skipField(bytes, offset, end, fieldNumber, wireType);
        continue;
      }
      const field = readVarint(bytes, offset, end);
      result.fieldId = bigintToValue(field.value);
      offset = field.offset;
      continue;
    }

    if (fieldNumber >= 2 && fieldNumber <= 9) {
      switch (fieldNumber) {
        case 2:
          if (wireType === WIRE_VARINT) {
            const field = readVarint(bytes, offset, end);
            result.kind = FIELD_VALUE_KIND.INTEGER;
            result.value = bigintToValue(zigzagDecode(field.value));
            offset = field.offset;
            continue;
          }
          break;
        case 3:
          if (wireType === WIRE_VARINT) {
            const field = readVarint(bytes, offset, end);
            result.kind = FIELD_VALUE_KIND.UNSIGNED;
            result.value = bigintToValue(field.value);
            offset = field.offset;
            continue;
          }
          break;
        case 4:
          if (wireType === WIRE_FIXED64) {
            ensureAvailable(offset, 8, end, 'double value');
            const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
            result.kind = FIELD_VALUE_KIND.REAL;
            result.value = view.getFloat64(offset, true);
            offset += 8;
            continue;
          }
          break;
        case 5:
          if (wireType === WIRE_VARINT) {
            const field = readVarint(bytes, offset, end);
            result.kind = FIELD_VALUE_KIND.FLAG;
            result.value = field.value !== 0n;
            offset = field.offset;
            continue;
          }
          break;
        case 6:
          if (wireType === WIRE_LEN) {
            const field = readString(bytes, offset, end);
            result.kind = FIELD_VALUE_KIND.TEXT;
            result.value = field.value;
            offset = field.offset;
            continue;
          }
          break;
        case 7:
          if (wireType === WIRE_VARINT) {
            const field = readVarint(bytes, offset, end);
            result.kind = FIELD_VALUE_KIND.ENUM;
            result.value = bigintToValue(BigInt.asIntN(32, field.value));
            offset = field.offset;
            continue;
          }
          break;
        case 8:
          if (wireType === WIRE_LEN) {
            const field = readLengthDelimited(bytes, offset, end, 'blob value');
            result.kind = FIELD_VALUE_KIND.BLOB;
            result.value = field.value;
            offset = field.offset;
            continue;
          }
          break;
        case 9:
          if (wireType === WIRE_VARINT) {
            const field = readVarint(bytes, offset, end);
            result.kind = FIELD_VALUE_KIND.TIMESTAMP;
            result.value = bigintToValue(BigInt.asIntN(64, field.value));
            offset = field.offset;
            continue;
          }
          break;
        default:
          break;
      }
      // A known field number with an unexpected wire type is treated as an
      // unknown field, matching proto3 parser behaviour.
      offset = skipField(bytes, offset, end, fieldNumber, wireType);
      continue;
    }

    offset = skipField(bytes, offset, end, fieldNumber, wireType);
  }

  return result;
}

/** Decode a DataUpdate payload into a compact plain object. */
export function decodeDataUpdate(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  const result = {
    definitionVersion: 0,
    values: [],
    sequence: null,
    timestampMicros: null,
  };

  while (offset < end) {
    const key = readKey(bytes, offset, end);
    offset = key.offset;
    const { fieldNumber, wireType } = key;

    if (fieldNumber === 1 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.definitionVersion = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 2 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'FieldValue');
      result.values.push(decodeFieldValue(field.value));
      offset = field.offset;
    } else if (fieldNumber === 3 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.sequence = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 4 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.timestampMicros = bigintToValue(field.value);
      offset = field.offset;
    } else {
      offset = skipField(bytes, offset, end, fieldNumber, wireType);
    }
  }

  return result;
}

/* ------------------------------------------------------------------------- */
/* ActionInvoke (tag 6)                                                       */
/* ------------------------------------------------------------------------- */

function paramsToBytes(params) {
  if (params === undefined || params === null) return new Uint8Array(0);
  if (typeof params === 'string') return TEXT_ENCODER.encode(params);
  if (
    params instanceof Uint8Array ||
    params instanceof ArrayBuffer ||
    ArrayBuffer.isView(params)
  ) {
    return asBytes(params);
  }
  return TEXT_ENCODER.encode(JSON.stringify(params));
}

function parseJsonOrText(bytes) {
  const text = appendText(bytes);
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/**
 * Encode an ActionInvoke payload (no frame tag).
 *
 * `params` may be an object (encoded as JSON), a JSON string, or raw bytes.
 * `idempotencyKey` defaults to zero, which proto3 treats as absent.
 */
export function encodeActionInvoke({
  actionId = 0,
  params,
  idempotencyKey = 0,
  paramsJson,
  timestampMicros,
} = {}) {
  const parts = [];
  const action = toBigInt(actionId, 'actionId');
  if (action !== 0n) parts.push(encodeKeyedVarint(1, action));

  if (params !== undefined && params !== null) {
    parts.push(encodeKeyedBytes(2, paramsToBytes(params)));
  }

  const key = toBigInt(idempotencyKey, 'idempotencyKey');
  if (key !== 0n) parts.push(encodeKeyedVarint(3, key));

  if (paramsJson !== undefined && paramsJson !== null) {
    parts.push(encodeKeyedBytes(4, paramsToBytes(paramsJson)));
  }

  if (timestampMicros !== undefined && timestampMicros !== null) {
    parts.push(encodeKeyedInt64(5, timestampMicros));
  }

  return concatBytes(parts);
}

/** Decode an ActionInvoke payload. */
export function decodeActionInvoke(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  const result = {
    actionId: 0,
    params: null,
    paramsJson: null,
    idempotencyKey: 0,
    timestampMicros: null,
  };

  while (offset < end) {
    const key = readKey(bytes, offset, end);
    offset = key.offset;
    const { fieldNumber, wireType } = key;

    if (fieldNumber === 1 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.actionId = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 2 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'action params');
      result.params = parseJsonOrText(field.value);
      offset = field.offset;
    } else if (fieldNumber === 3 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.idempotencyKey = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 4 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'action params_json');
      result.paramsJson = parseJsonOrText(field.value);
      offset = field.offset;
    } else if (fieldNumber === 5 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.timestampMicros = bigintToValue(BigInt.asIntN(64, field.value));
      offset = field.offset;
    } else {
      offset = skipField(bytes, offset, end, fieldNumber, wireType);
    }
  }

  return result;
}

/* ------------------------------------------------------------------------- */
/* InteractionEvent (tag 7)                                                   */
/* ------------------------------------------------------------------------- */

/** Encode a PointerEvent payload. */
export function encodePointerEvent({
  x = 0,
  y = 0,
  phase = POINTER_PHASE.UNSPECIFIED,
  button,
  pressurePpt,
  wheelDelta,
} = {}) {
  const parts = [];
  if (toBigInt(x, 'x') !== 0n) parts.push(encodeKeyedZigzag(1, x));
  if (toBigInt(y, 'y') !== 0n) parts.push(encodeKeyedZigzag(2, y));
  if (toBigInt(phase, 'phase') !== 0n) parts.push(encodeKeyedVarint(3, phase));
  if (button !== undefined && button !== null) {
    parts.push(encodeKeyedVarint(4, button));
  }
  if (pressurePpt !== undefined && pressurePpt !== null) {
    parts.push(encodeKeyedVarint(5, pressurePpt));
  }
  if (wheelDelta !== undefined && wheelDelta !== null) {
    parts.push(encodeKeyedZigzag(6, wheelDelta));
  }
  return concatBytes(parts);
}

/** Decode a PointerEvent payload. */
export function decodePointerEvent(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  const result = {
    x: 0,
    y: 0,
    phase: POINTER_PHASE.UNSPECIFIED,
    button: null,
    pressurePpt: null,
    wheelDelta: null,
  };

  while (offset < end) {
    const key = readKey(bytes, offset, end);
    offset = key.offset;
    const { fieldNumber, wireType } = key;

    if (fieldNumber === 1 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.x = bigintToValue(BigInt.asIntN(32, zigzagDecode(field.value)));
      offset = field.offset;
    } else if (fieldNumber === 2 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.y = bigintToValue(BigInt.asIntN(32, zigzagDecode(field.value)));
      offset = field.offset;
    } else if (fieldNumber === 3 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.phase = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 4 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.button = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 5 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.pressurePpt = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 6 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.wheelDelta = bigintToValue(BigInt.asIntN(32, zigzagDecode(field.value)));
      offset = field.offset;
    } else {
      offset = skipField(bytes, offset, end, fieldNumber, wireType);
    }
  }

  return result;
}

function decodeMapEntry(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  let key = '';
  let value = '';
  while (offset < end) {
    const field = readKey(bytes, offset, end);
    offset = field.offset;
    if (field.fieldNumber === 1 && field.wireType === WIRE_LEN) {
      const stringField = readString(bytes, offset, end);
      key = stringField.value;
      offset = stringField.offset;
    } else if (field.fieldNumber === 2 && field.wireType === WIRE_LEN) {
      const stringField = readString(bytes, offset, end);
      value = stringField.value;
      offset = stringField.offset;
    } else {
      offset = skipField(
        bytes,
        offset,
        end,
        field.fieldNumber,
        field.wireType,
      );
    }
  }
  return [key, value];
}

/** Encode a SemanticEvent payload. */
export function encodeSemanticEvent({ name = '', params = {} } = {}) {
  const parts = [];
  if (name) parts.push(encodeKeyedString(1, name));
  const entries = Object.entries(params || {}).sort(([a], [b]) => a.localeCompare(b));
  for (const [key, value] of entries) {
    const entry = concatBytes([
      encodeKeyedString(1, key),
      encodeKeyedString(2, String(value)),
    ]);
    parts.push(encodeKeyedBytes(2, entry));
  }
  return concatBytes(parts);
}

/** Decode a SemanticEvent payload. */
export function decodeSemanticEvent(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  const result = { name: '', params: {} };

  while (offset < end) {
    const key = readKey(bytes, offset, end);
    offset = key.offset;
    if (key.fieldNumber === 1 && key.wireType === WIRE_LEN) {
      const field = readString(bytes, offset, end);
      result.name = field.value;
      offset = field.offset;
    } else if (key.fieldNumber === 2 && key.wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'map entry');
      const [mapKey, mapValue] = decodeMapEntry(field.value);
      result.params[mapKey] = mapValue;
      offset = field.offset;
    } else {
      offset = skipField(bytes, offset, end, key.fieldNumber, key.wireType);
    }
  }

  return result;
}

/**
 * Encode an InteractionEvent payload.
 *
 * Supply one of `pointer`, `semantic`, or `action`.  The action form embeds an
 * ActionInvoke, which lets a browser send an action without a separate frame.
 */
export function encodeInteractionEvent({
  nodeId = 0,
  targetId,
  timestampMicros,
  pointer,
  semantic,
  action,
} = {}) {
  const parts = [];
  if (toBigInt(nodeId, 'nodeId') !== 0n) {
    parts.push(encodeKeyedVarint(1, nodeId));
  }
  if (targetId !== undefined && targetId !== null && targetId !== '') {
    parts.push(encodeKeyedString(2, targetId));
  }
  if (timestampMicros !== undefined && timestampMicros !== null) {
    parts.push(encodeKeyedInt64(3, timestampMicros));
  }
  if (pointer !== undefined && pointer !== null) {
    parts.push(encodeKeyedBytes(4, encodePointerEvent(pointer)));
  }
  if (semantic !== undefined && semantic !== null) {
    parts.push(encodeKeyedBytes(6, encodeSemanticEvent(semantic)));
  }
  if (action !== undefined && action !== null) {
    parts.push(encodeKeyedBytes(7, encodeActionInvoke(action)));
  }
  return concatBytes(parts);
}

/** Decode an InteractionEvent payload (pointer, semantic, and action forms). */
export function decodeInteractionEvent(bytesLike) {
  const bytes = asBytes(bytesLike);
  let offset = 0;
  const end = bytes.length;
  const result = {
    nodeId: 0,
    targetId: null,
    timestampMicros: null,
    type: null,
    pointer: null,
    semantic: null,
    action: null,
  };

  while (offset < end) {
    const key = readKey(bytes, offset, end);
    offset = key.offset;
    const { fieldNumber, wireType } = key;

    if (fieldNumber === 1 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.nodeId = bigintToValue(field.value);
      offset = field.offset;
    } else if (fieldNumber === 2 && wireType === WIRE_LEN) {
      const field = readString(bytes, offset, end);
      result.targetId = field.value;
      offset = field.offset;
    } else if (fieldNumber === 3 && wireType === WIRE_VARINT) {
      const field = readVarint(bytes, offset, end);
      result.timestampMicros = bigintToValue(BigInt.asIntN(64, field.value));
      offset = field.offset;
    } else if (fieldNumber === 4 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'pointer event');
      result.type = 'pointer';
      result.pointer = decodePointerEvent(field.value);
      offset = field.offset;
    } else if (fieldNumber === 6 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'semantic event');
      result.type = 'semantic';
      result.semantic = decodeSemanticEvent(field.value);
      offset = field.offset;
    } else if (fieldNumber === 7 && wireType === WIRE_LEN) {
      const field = readLengthDelimited(bytes, offset, end, 'action invoke');
      result.type = 'action';
      result.action = decodeActionInvoke(field.value);
      offset = field.offset;
    } else {
      // KeyEvent (field 5) is intentionally not decoded; skip it and other
      // fields so newer protocol additions remain readable.
      offset = skipField(bytes, offset, end, fieldNumber, wireType);
    }
  }

  return result;
}

/* ------------------------------------------------------------------------- */
/* Frame codec                                                                */
/* ------------------------------------------------------------------------- */

/** Encode `[tag][payload]`. */
export function encodeFrame(tag, payload = new Uint8Array(0)) {
  if (!Number.isInteger(tag) || tag < 0 || tag > 0xff) {
    throw new RangeError(`display tag must be an integer in 0..255, got ${tag}`);
  }
  const body = asBytes(payload);
  if (body.length > MAX_PAYLOAD_LEN) {
    throw new DisplayProtocolError(
      `display payload is ${body.length} bytes, exceeding the ${MAX_PAYLOAD_LEN} byte limit`,
    );
  }
  const frame = new Uint8Array(HEADER_LEN + body.length);
  frame[0] = tag;
  frame.set(body, HEADER_LEN);
  return frame;
}

/** Split a frame into `{ tag, payload }` without decoding the payload. */
export function splitFrame(frameLike) {
  const frame = asBytes(frameLike);
  if (frame.length === 0) throw new EmptyFrameError();
  const payload = frame.subarray(HEADER_LEN);
  if (payload.length > MAX_PAYLOAD_LEN) {
    throw new DisplayProtocolError(
      `display payload is ${payload.length} bytes, exceeding the ${MAX_PAYLOAD_LEN} byte limit`,
    );
  }
  return { tag: frame[0], payload };
}

/**
 * Decode a complete Display frame.
 *
 * Known tags are decoded into a property on the result (`definition`,
 * `dataUpdate`, `actionInvoke`, or `interactionEvent`).  Unknown tags are
 * returned with their raw payload and a null `kind`; they never throw.
 */
export function decodeFrame(frameLike) {
  const { tag, payload } = splitFrame(frameLike);
  const result = { tag, kind: TAG_NAMES[tag] ?? null, payload };

  switch (tag) {
    case TAG_DEFINITION:
      return { ...result, definition: decodeDefinition(payload) };
    case TAG_DATA_UPDATE:
      return { ...result, dataUpdate: decodeDataUpdate(payload) };
    case TAG_ACTION_INVOKE:
      return { ...result, actionInvoke: decodeActionInvoke(payload) };
    case TAG_INTERACTION_EVENT:
      return { ...result, interactionEvent: decodeInteractionEvent(payload) };
    default:
      return result;
  }
}
