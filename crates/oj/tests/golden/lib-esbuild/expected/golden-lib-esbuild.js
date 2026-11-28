function helper(input) {
  return "hello " + input;
}
function greet(name) {
  return /* @__PURE__ */ helper(name).toUpperCase();
}
var kinds = ["a", "b"];
export {
  greet,
  kinds
};
