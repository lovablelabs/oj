function a() {
  return b() + keep(undefined);
}
function b() {
  return a();
}
