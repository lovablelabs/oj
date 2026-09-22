"use strict"; return ((function () {
const { core, primordials } = __bootstrap;
const { Buffer } = core.loadExtScript("ext:deno_node/internal/buffer.mjs");
const { codes } = core.loadExtScript("ext:deno_node/internal/errors.ts");
const {
  isArrayBufferView,
  isTypedArray,
  isUint8Array,
} = core.loadExtScript("ext:deno_node/internal/util/types.ts");

const {
  Array,
  ArrayBufferPrototypeSlice,
  ArrayIsArray,
  ArrayPrototypeReduce,
  DataViewPrototypeGetBuffer,
  DataViewPrototypeGetByteLength,
  DataViewPrototypeGetByteOffset,
  TypedArrayPrototypeGetBuffer,
  TypedArrayPrototypeGetByteLength,
  TypedArrayPrototypeGetByteOffset,
} = primordials;

function convertProtocols(protocols) {
  const lengths = new Array(protocols.length);
  const buffer = Buffer.allocUnsafe(
    ArrayPrototypeReduce(protocols, (total, protocol, index) => {
      const length = Buffer.byteLength(protocol);
      if (length > 255) {
        throw new codes.ERR_OUT_OF_RANGE(
          `The byte length of the protocol at index ${index} exceeds the maximum length.`,
          "<= 255",
          length,
          true,
        );
      }
      lengths[index] = length;
      return total + 1 + length;
    }, 0),
  );

  let offset = 0;
  for (let i = 0; i < protocols.length; i++) {
    buffer[offset++] = lengths[i];
    buffer.write(protocols[i], offset);
    offset += lengths[i];
  }

  return buffer;
}

function convertALPNProtocols(protocols, out) {
  if (ArrayIsArray(protocols)) {
    out.ALPNProtocols = convertProtocols(protocols);
  } else if (isUint8Array(protocols)) {
    out.ALPNProtocols = Buffer.from(protocols);
  } else if (isArrayBufferView(protocols)) {
    const buffer = isTypedArray(protocols)
      ? TypedArrayPrototypeGetBuffer(protocols)
      : DataViewPrototypeGetBuffer(protocols);
    const byteOffset = isTypedArray(protocols)
      ? TypedArrayPrototypeGetByteOffset(protocols)
      : DataViewPrototypeGetByteOffset(protocols);
    const byteLength = isTypedArray(protocols)
      ? TypedArrayPrototypeGetByteLength(protocols)
      : DataViewPrototypeGetByteLength(protocols);
    out.ALPNProtocols = Buffer.from(
      ArrayBufferPrototypeSlice(buffer, byteOffset, byteOffset + byteLength),
    );
  }
}

return { convertALPNProtocols, default: { convertALPNProtocols } };
})());