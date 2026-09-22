"use strict"; return ((function() {
  const { primordials } = __bootstrap;
  const { MathFloor, Number, NumberPrototypeToFixed, ObjectCreate, ObjectDefineProperty, ReflectApply, RegExpPrototypeExec, SafeArrayIterator, SafeRegExp, String, StringPrototypePadStart, StringPrototypeReplace, StringPrototypeReplaceAll, StringPrototypeSplit, StringPrototypeToLowerCase, StringPrototypeToUpperCase } = primordials;
  let debugImpls = ObjectCreate(null);
  let testEnabled = ()=>false;
  // `debugEnv` is initial value of process.env.NODE_DEBUG
  function initializeDebugEnv(debugEnv) {
    debugImpls = ObjectCreate(null);
    if (debugEnv) {
      debugEnv = StringPrototypeReplaceAll(StringPrototypeReplaceAll(StringPrototypeReplace(debugEnv, new SafeRegExp(/[|\\{}()[\]^$+?.]/g), "\\$&"), "*", ".*"), ",", "$|^");
      const debugEnvRegex = new SafeRegExp(`^${debugEnv}$`, "i");
      testEnabled = (str)=>RegExpPrototypeExec(debugEnvRegex, str) !== null;
    } else {
      testEnabled = ()=>false;
    }
  }
  // Emits warning when user sets
  // NODE_DEBUG=http or NODE_DEBUG=http2.
  function emitWarningIfNeeded(set) {
    if ("HTTP" === set || "HTTP2" === set) {
      // deno-lint-ignore no-console
      console.warn("Setting the NODE_DEBUG environment variable " + "to '" + StringPrototypeToLowerCase(set) + "' can expose sensitive " + "data (such as passwords, tokens and authentication headers) " + "in the resulting log.");
    }
  }
  const noop = ()=>{};
  function debuglogImpl(enabled, set) {
    if (debugImpls[set] === undefined) {
      if (enabled) {
        emitWarningIfNeeded(set);
        debugImpls[set] = function debug(msg, ...args) {
          // deno-lint-ignore no-console
          console.error("%s %s: " + msg, set, String(Deno.pid), ...new SafeArrayIterator(args));
        };
      } else {
        debugImpls[set] = noop;
      }
    }
    return debugImpls[set];
  }
  // debuglogImpl depends on process.pid and process.env.NODE_DEBUG,
  // so it needs to be called lazily in top scopes of internal modules
  // that may be loaded before these run time states are allowed to
  // be accessed.
  function debuglog(set, cb) {
    function init() {
      set = StringPrototypeToUpperCase(set);
      enabled = testEnabled(set);
    }
    let debug = (...args)=>{
      init();
      // Only invokes debuglogImpl() when the debug function is
      // called for the first time.
      debug = debuglogImpl(enabled, set);
      if (typeof cb === "function") {
        cb(debug);
      }
      return ReflectApply(debug, undefined, args);
    };
    let enabled;
    let test = ()=>{
      init();
      test = ()=>enabled;
      return enabled;
    };
    const logger = (...args)=>ReflectApply(debug, undefined, args);
    ObjectDefineProperty(logger, "enabled", {
      __proto__: null,
      get () {
        return test();
      },
      configurable: true,
      enumerable: true
    });
    return logger;
  }
  // One second in milliseconds.
  const kSecond = 1000;
  const kMinute = 60 * kSecond;
  const kHour = 60 * kMinute;
  function pad(value) {
    return StringPrototypePadStart(`${value}`, 2, "0");
  }
  function formatTime(ms) {
    let hours = 0;
    let minutes = 0;
    let seconds = 0;
    if (ms >= kSecond) {
      if (ms >= kMinute) {
        if (ms >= kHour) {
          hours = MathFloor(ms / kHour);
          ms = ms % kHour;
        }
        minutes = MathFloor(ms / kMinute);
        ms = ms % kMinute;
      }
      seconds = ms / kSecond;
    }
    if (hours !== 0 || minutes !== 0) {
      const fixed = StringPrototypeSplit(NumberPrototypeToFixed(seconds, 3), ".");
      const secondsStr = fixed[0];
      const msStr = fixed[1];
      const res = hours !== 0 ? `${hours}:${pad(minutes)}` : minutes;
      return `${res}:${pad(secondsStr)}.${msStr} (${hours !== 0 ? "h:m" : ""}m:ss.mmm)`;
    }
    if (seconds !== 0) {
      return `${NumberPrototypeToFixed(seconds, 3)}s`;
    }
    return `${Number(NumberPrototypeToFixed(ms, 3))}ms`;
  }
  const _defaultExport = {
    debuglog,
    formatTime
  };
  return {
    initializeDebugEnv,
    debuglog,
    formatTime,
    default: _defaultExport
  };
})());