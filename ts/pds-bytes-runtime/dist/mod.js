export class PdsError extends Error {
  constructor(kind, detail) {
    super(detail ? `${kind}: ${detail}` : kind);
    this.kind = kind;
    this.name = "PdsError";
  }
}
export class Input {
  #data;
  #offset = 0;
  #maskAt = -1;
  #mask = 0xff;
  constructor(data) {
    this.#data = data;
  }
  get offset() {
    return this.#offset;
  }
  remaining() {
    return Math.max(0, this.#data.length - this.#offset);
  }
  mask_next(and) {
    this.#maskAt = this.#offset;
    this.#mask = and;
  }
  peek() {
    if (this.#offset >= this.#data.length) {
      throw new PdsError("NotEnoughData");
    }
    const byte = this.#data[this.#offset];
    return this.#offset === this.#maskAt ? byte & this.#mask : byte;
  }
  read() {
    const byte = this.peek();
    this.#offset++;
    return byte;
  }
  skip() {
    this.#offset++;
  }
  rewind(offset) {
    this.#offset = offset;
  }
  read_bytes(n) {
    if (n > this.remaining()) {
      throw new PdsError("NotEnoughData");
    }
    const out = this.#data.slice(this.#offset, this.#offset + n);
    if (n > 0 && this.#offset === this.#maskAt) {
      out[0] &= this.#mask;
    }
    this.#offset += n;
    return out;
  }
  read_rest() {
    return this.read_bytes(this.remaining());
  }
  take(n) {
    return new Input(this.read_bytes(n));
  }
}
export class Output {
  #buf = new Uint8Array(64);
  #len = 0;
  get length() {
    return this.#len;
  }
  #reserve(n) {
    if (this.#len + n <= this.#buf.length) {
      return;
    }
    let size = this.#buf.length * 2;
    while (size < this.#len + n) {
      size *= 2;
    }
    const next = new Uint8Array(size);
    next.set(this.#buf.subarray(0, this.#len));
    this.#buf = next;
  }
  write(bytes) {
    this.#reserve(bytes.length);
    this.#buf.set(bytes, this.#len);
    this.#len += bytes.length;
  }
  write_byte(byte) {
    this.#reserve(1);
    this.#buf[this.#len++] = byte & 0xff;
  }
  into_inner() {
    return this.#buf.slice(0, this.#len);
  }
}
export function equal(a, b) {
  if (a === b) {
    return true;
  }
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) {
    return false;
  }
  if (a instanceof Map || b instanceof Map) {
    if (!(a instanceof Map && b instanceof Map) || a.size !== b.size) {
      return false;
    }
    for (const [k, v] of a) {
      if (!b.has(k) || !equal(v, b.get(k))) {
        return false;
      }
    }
    return true;
  }
  if (ArrayBuffer.isView(a) || Array.isArray(a)) {
    const x = a;
    const y = b;
    if (!(ArrayBuffer.isView(b) || Array.isArray(b)) || x.length !== y.length) {
      return false;
    }
    for (let i = 0; i < x.length; i++) {
      if (!equal(x[i], y[i])) {
        return false;
      }
    }
    return true;
  }
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  if (ka.length !== kb.length) {
    return false;
  }
  const ra = a;
  const rb = b;
  return ka.every(k => Object.hasOwn(rb, k) && equal(ra[k], rb[k]));
}
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", {
  fatal: true,
  ignoreBOM: true
});
export function utf8_encode(s) {
  return encoder.encode(s);
}
export function utf8_decode(bytes) {
  try {
    return decoder.decode(bytes);
  } catch {
    throw new PdsError("InvalidValue", "invalid utf-8");
  }
}
function fixed(size, get, set) {
  return {
    serialize(v, output) {
      const bytes = new Uint8Array(size);
      set(new DataView(bytes.buffer), v);
      output.write(bytes);
    },
    deserialize(input) {
      const bytes = input.read_bytes(size);
      return get(new DataView(bytes.buffer, bytes.byteOffset, size));
    },
    size_of(_v) {
      return size;
    }
  };
}
export const pds_u8 = fixed(1, d => d.getUint8(0), (d, v) => d.setUint8(0, v));
export const pds_i8 = fixed(1, d => d.getInt8(0), (d, v) => d.setInt8(0, v));
export const pds_u16 = fixed(2, d => d.getUint16(0), (d, v) => d.setUint16(0, v));
export const pds_i16 = fixed(2, d => d.getInt16(0), (d, v) => d.setInt16(0, v));
export const pds_u32 = fixed(4, d => d.getUint32(0), (d, v) => d.setUint32(0, v));
export const pds_i32 = fixed(4, d => d.getInt32(0), (d, v) => d.setInt32(0, v));
export const pds_u64 = fixed(8, d => d.getBigUint64(0), (d, v) => d.setBigUint64(0, v));
export const pds_i64 = fixed(8, d => d.getBigInt64(0), (d, v) => d.setBigInt64(0, v));
export const pds_f32 = fixed(4, d => d.getFloat32(0), (d, v) => d.setFloat32(0, v));
export const pds_f64 = fixed(8, d => d.getFloat64(0), (d, v) => d.setFloat64(0, v));
export const pds_bool = {
  serialize(v, output) {
    output.write_byte(v ? 1 : 0);
  },
  deserialize(input) {
    return input.read() !== 0;
  },
  size_of(_v) {
    return 1;
  }
};
export const pds_void = {
  serialize(_v, _output) {},
  deserialize(_input) {
    return null;
  },
  size_of(_v) {
    return 0;
  }
};
//# sourceMappingURL=mod.js.map
