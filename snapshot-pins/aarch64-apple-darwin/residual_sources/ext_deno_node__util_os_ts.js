"use strict"; return ((function() {
  const { core } = __bootstrap;
  const { op_node_build_os } = core.ops;
  const osType = op_node_build_os();
  const isAndroid = osType === "android";
  const isWindows = osType === "windows";
  const isLinux = osType === "linux" || osType === "android";
  const isMacOS = osType === "darwin";
  return {
    osType,
    isAndroid,
    isWindows,
    isLinux,
    isMacOS
  };
})());