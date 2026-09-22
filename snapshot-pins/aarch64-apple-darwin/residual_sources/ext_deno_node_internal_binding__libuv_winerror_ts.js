"use strict"; return ((function() {
  const { core } = __bootstrap;
  const { op_node_sys_to_uv_error } = core.ops;
  function uvTranslateSysError(sysErrno) {
    return op_node_sys_to_uv_error(sysErrno);
  }
  return {
    uvTranslateSysError
  };
})());