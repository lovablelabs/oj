"use strict"; return ((function() {
  const { core } = __bootstrap;
  const { op_node_get_first_expression } = core.ops;
  return {
    getErrorSourceExpression: op_node_get_first_expression
  };
})());