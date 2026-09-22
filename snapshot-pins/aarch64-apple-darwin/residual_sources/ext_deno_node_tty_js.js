"use strict"; return ((function () {
const { core } = __bootstrap;
const { TTY } = core.ops;

// Returns true when the given numeric fd is associated with a TTY and false otherwise.
function isatty(fd) {
  if (typeof fd !== "number" || fd >> 0 !== fd || fd < 0) {
    return false;
  }
  return TTY.isTTY(fd);
}

return { isatty };
})());