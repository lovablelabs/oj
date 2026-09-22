"use strict"; return ((function() {
  const { primordials } = __bootstrap;
  const { ReflectApply, SafeMap } = primordials;
  const callbacks = new SafeMap();
  let seq = 0;
  function sendHelper(proc, message, handle, cb) {
    if (!proc.connected) {
      return false;
    }
    // Mark message as internal. Mirrors lib/internal/child_process.js prefix.
    message = {
      cmd: "NODE_CLUSTER",
      ...message,
      seq
    };
    if (typeof cb === "function") {
      callbacks.set(seq, cb);
    }
    seq += 1;
    return proc.send(message, handle);
  }
  // Returns an internalMessage listener that hands off normal messages to the
  // callback but intercepts and redirects ACK messages.
  function internal(worker, cb) {
    return function onInternalMessage(message, _handle) {
      if (message.cmd !== "NODE_CLUSTER") {
        return;
      }
      let fn = cb;
      if (message.ack !== undefined) {
        const callback = callbacks.get(message.ack);
        if (callback !== undefined) {
          fn = callback;
          callbacks.delete(message.ack);
        }
      }
      ReflectApply(fn, worker, arguments);
    };
  }
  return {
    sendHelper,
    internal
  };
})());