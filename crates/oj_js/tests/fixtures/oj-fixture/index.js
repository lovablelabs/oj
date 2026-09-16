// CommonJS on purpose: importing this package from an ES module exercises the
// engine's CJS-to-ESM translation, and require() exercises the CJS path.
module.exports = {
  greet: function (name) {
    return "hello " + name;
  },
  answer: 42,
};
