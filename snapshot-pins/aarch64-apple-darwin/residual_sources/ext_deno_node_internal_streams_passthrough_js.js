"use strict"; return ((function () {
const { core, primordials } = __bootstrap;
const Transform = core.loadExtScript(
  "ext:deno_node/internal/streams/transform.js",
).default;

const {
  ObjectSetPrototypeOf,
} = primordials;

ObjectSetPrototypeOf(PassThrough.prototype, Transform.prototype);
ObjectSetPrototypeOf(PassThrough, Transform);

function PassThrough(options) {
  if (!(this instanceof PassThrough)) {
    return new PassThrough(options);
  }

  Transform.call(this, options);
}

PassThrough.prototype._transform = function (chunk, encoding, cb) {
  cb(null, chunk);
};

return { default: PassThrough, PassThrough };
})());