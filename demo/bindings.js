export const Lang = Object.freeze({
  0: "Rust",
  "Rust": 0,
  1: "Js",
  "Js": 1,
  2: "Wasmtime",
  "Wasmtime": 2,
});
let DATA_VIEW = new DataView(new ArrayBuffer());
function data_view(mem) {
  if (DATA_VIEW.buffer !== mem.buffer) DATA_VIEW = new DataView(mem.buffer);
  return DATA_VIEW;
}
const UTF8_DECODER = new TextDecoder('utf-8');

let UTF8_ENCODED_LEN = 0;
const UTF8_ENCODER = new TextEncoder('utf-8');

function utf8_encode(s, realloc, memory) {
  if (typeof s !== 'string') throw new TypeError('expected a string');
  
  if (s.length === 0) {
    UTF8_ENCODED_LEN = 0;
    return 1;
  }
  
  let alloc_len = 0;
  let ptr = 0;
  let writtenTotal = 0;
  while (s.length > 0) {
    ptr = realloc(ptr, alloc_len, alloc_len + s.length, 1);
    alloc_len += s.length;
    const { read, written } = UTF8_ENCODER.encodeInto(
    s,
    new Uint8Array(memory.buffer, ptr + writtenTotal, alloc_len - writtenTotal),
    );
    writtenTotal += written;
    s = s.slice(read);
  }
  if (alloc_len > writtenTotal)
  ptr = realloc(ptr, alloc_len, writtenTotal, 1);
  UTF8_ENCODED_LEN = writtenTotal;
  return ptr;
}
export class Demo {
  static async instantiate(module, imports) {
    const me = new Demo();
    
    let ret;
    if (module instanceof WebAssembly.Module) {
      ret = {
        module,
        instance: await WebAssembly.instantiate(module, imports),
      };
    } else if (module instanceof ArrayBuffer || module instanceof Uint8Array) {
      ret = await WebAssembly.instantiate(module, imports);
    } else {
      ret = await WebAssembly.instantiateStreaming(module, imports);
    }
    me.module = ret.module;
    me.instance = ret.instance;
    me._exports = me.instance.exports;
    return me;
  }
  render(arg0, arg1, arg2) {
    const free = this._exports["canonical_abi_free"];
    const realloc = this._exports["canonical_abi_realloc"];
    const memory = this._exports.memory;
    const ptr0 = utf8_encode(arg0, realloc, memory);
    const len0 = UTF8_ENCODED_LEN;
    const variant1 = arg1;
    if (!(variant1 in Lang))
    throw new RangeError("invalid variant specified for Lang");
    const variant2 = arg2;
    let variant2_0;
    switch (variant2) {
      case false: {
        variant2_0 = 0;
        break;
      }
      case true: {
        variant2_0 = 1;
        break;
      }
      default:
      throw new RangeError("invalid variant specified for bool");
    }
    const ret = this._exports['render'](ptr0, len0, Number.isInteger(variant1) ? variant1 : Lang[variant1], variant2_0);
    let variant7;
    switch (data_view(memory).getInt32(ret + 0, true)) {
      case 0: {
        const len5 = data_view(memory).getInt32(ret + 16, true);
        const base5 = data_view(memory).getInt32(ret + 8, true);
        const result5 = [];
        for (let i = 0; i < len5; i++) {
          const base = base5 + i * 16;
          const ptr3 = data_view(memory).getInt32(base + 0, true);
          const len3 = data_view(memory).getInt32(base + 4, true);
          const list3 = UTF8_DECODER.decode(new Uint8Array(memory.buffer, ptr3, len3));
          free(ptr3, len3, 1);
          const ptr4 = data_view(memory).getInt32(base + 8, true);
          const len4 = data_view(memory).getInt32(base + 12, true);
          const list4 = UTF8_DECODER.decode(new Uint8Array(memory.buffer, ptr4, len4));
          free(ptr4, len4, 1);
          result5.push([list3, list4]);
        }
        free(base5, len5 * 16, 4);
        variant7 = {
          tag: "ok",
          val: result5,
        };
        break;
      }
      case 1: {
        const ptr6 = data_view(memory).getInt32(ret + 8, true);
        const len6 = data_view(memory).getInt32(ret + 16, true);
        const list6 = UTF8_DECODER.decode(new Uint8Array(memory.buffer, ptr6, len6));
        free(ptr6, len6, 1);
        variant7 = {
          tag: "err",
          val: list6,
        };
        break;
      }
      default:
      throw new RangeError("invalid variant discriminant for expected");
    }
    return variant7;
  }
}