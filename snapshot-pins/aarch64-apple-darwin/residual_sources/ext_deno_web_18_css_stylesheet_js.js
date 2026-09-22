"use strict"; return ((function () {
const { core, primordials } = __bootstrap;
const { CSSRule, CSSStyleSheet } = core.ops;
const {
  ObjectDefineProperty,
  ObjectPrototypeIsPrototypeOf,
  SymbolFor,
} = primordials;

const webidl = core.loadExtScript("ext:deno_webidl/00_webidl.js");
const { createFilteredInspectProxy } = core.loadExtScript(
  "ext:deno_web/01_console.js",
);

const CSSRulePrototype = CSSRule.prototype;
const CSSStyleSheetPrototype = CSSStyleSheet.prototype;

function defineCustomInspect(prototype, keys) {
  ObjectDefineProperty(
    prototype,
    SymbolFor("Deno.privateCustomInspect"),
    {
      __proto__: null,
      value: function customInspect(inspect, inspectOptions) {
        return inspect(
          createFilteredInspectProxy({
            object: this,
            evaluate: ObjectPrototypeIsPrototypeOf(prototype, this),
            keys,
          }),
          inspectOptions,
        );
      },
      enumerable: false,
      writable: true,
      configurable: true,
    },
  );
}

defineCustomInspect(CSSRulePrototype, ["cssText"]);
defineCustomInspect(CSSStyleSheetPrototype, ["cssRules"]);

webidl.configureInterface(CSSRule);
webidl.configureInterface(CSSStyleSheet);

return {
  CSSRule,
  CSSRulePrototype,
  CSSStyleSheet,
  CSSStyleSheetPrototype,
};
})());