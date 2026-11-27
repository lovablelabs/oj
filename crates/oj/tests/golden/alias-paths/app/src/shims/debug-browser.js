export default function debug(namespace) {
  return (...args) => console.log(`[${namespace}]`, ...args);
}
