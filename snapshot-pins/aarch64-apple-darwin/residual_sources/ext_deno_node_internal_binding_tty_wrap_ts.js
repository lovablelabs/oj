"use strict"; return ((function() {
  const { core } = __bootstrap;
  const { TTY } = core.ops;
  // Mark TTY as a StreamBase handle, matching Node's StreamBase::AddMethods.
  TTY.prototype.isStreamBase = true;
  return {
    TTY
  };
})());