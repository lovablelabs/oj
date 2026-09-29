/*! Copyright (c) Meta Platforms, Inc. and affiliates. **/

const REACT_FORWARD_REF_TYPE = Symbol.for("react.forward_ref");
const REACT_MEMO_TYPE = Symbol.for("react.memo");

let allFamiliesByID = new Map();
let allFamiliesByType = new WeakMap();
let allSignaturesByType = new WeakMap();

const updatedFamiliesByType = new WeakMap();

let pendingUpdates = [];

const helpersByRendererID = new Map();

const helpersByRoot = new Map();

const mountedRoots = new Set();
const failedRoots = new Set();

let rootElements = new WeakMap();
let isPerformingRefresh = false;

function computeFullKey(signature) {
  if (signature.fullKey !== null) {
    return signature.fullKey;
  }

  let fullKey = signature.ownKey;
  let hooks;
  try {
    hooks = signature.getCustomHooks();
  } catch (err) {
    signature.forceReset = true;
    signature.fullKey = fullKey;
    return fullKey;
  }

  for (let i = 0; i < hooks.length; i++) {
    const hook = hooks[i];
    if (typeof hook !== "function") {
      signature.forceReset = true;
      signature.fullKey = fullKey;
      return fullKey;
    }
    const nestedHookSignature = allSignaturesByType.get(hook);
    if (nestedHookSignature === undefined) {
      continue;
    }
    const nestedHookKey = computeFullKey(nestedHookSignature);
    if (nestedHookSignature.forceReset) {
      signature.forceReset = true;
    }
    fullKey += "\n---\n" + nestedHookKey;
  }

  signature.fullKey = fullKey;
  return fullKey;
}

function haveEqualSignatures(prevType, nextType) {
  const prevSignature = allSignaturesByType.get(prevType);
  const nextSignature = allSignaturesByType.get(nextType);

  if (prevSignature === undefined && nextSignature === undefined) {
    return true;
  }
  if (prevSignature === undefined || nextSignature === undefined) {
    return false;
  }
  if (computeFullKey(prevSignature) !== computeFullKey(nextSignature)) {
    return false;
  }
  if (nextSignature.forceReset) {
    return false;
  }

  return true;
}

function isReactClass(type) {
  return type.prototype && type.prototype.isReactComponent;
}

function canPreserveStateBetween(prevType, nextType) {
  if (isReactClass(prevType) || isReactClass(nextType)) {
    return false;
  }
  if (haveEqualSignatures(prevType, nextType)) {
    return true;
  }
  return false;
}

function resolveFamily(type) {
  return updatedFamiliesByType.get(type);
}

function getProperty(object, property) {
  try {
    return object[property];
  } catch (err) {
    return undefined;
  }
}

function performReactRefresh() {
  if (pendingUpdates.length === 0) {
    return null;
  }
  if (isPerformingRefresh) {
    return null;
  }

  isPerformingRefresh = true;
  try {
    const staleFamilies = new Set();
    const updatedFamilies = new Set();

    const updates = pendingUpdates;
    pendingUpdates = [];
    updates.forEach(([family, nextType]) => {
      const prevType = family.current;
      updatedFamiliesByType.set(prevType, family);
      updatedFamiliesByType.set(nextType, family);
      family.current = nextType;

      if (canPreserveStateBetween(prevType, nextType)) {
        updatedFamilies.add(family);
      } else {
        staleFamilies.add(family);
      }
    });

    const update = {
      updatedFamilies,
      staleFamilies,
    };

    helpersByRendererID.forEach((helpers) => {
      helpers.setRefreshHandler(resolveFamily);
    });

    let didError = false;
    let firstError = null;

    const failedRootsSnapshot = new Set(failedRoots);
    const mountedRootsSnapshot = new Set(mountedRoots);
    const helpersByRootSnapshot = new Map(helpersByRoot);

    failedRootsSnapshot.forEach((root) => {
      const helpers = helpersByRootSnapshot.get(root);
      if (helpers === undefined) {
        throw new Error("Could not find helpers for a root. This is a bug in React Refresh.");
      }
      if (!failedRoots.has(root)) {
      }
      if (rootElements === null) {
        return;
      }
      if (!rootElements.has(root)) {
        return;
      }
      const element = rootElements.get(root);
      try {
        helpers.scheduleRoot(root, element);
      } catch (err) {
        if (!didError) {
          didError = true;
          firstError = err;
        }
      }
    });
    mountedRootsSnapshot.forEach((root) => {
      const helpers = helpersByRootSnapshot.get(root);
      if (helpers === undefined) {
        throw new Error("Could not find helpers for a root. This is a bug in React Refresh.");
      }
      if (!mountedRoots.has(root)) {
      }
      try {
        helpers.scheduleRefresh(root, update);
      } catch (err) {
        if (!didError) {
          didError = true;
          firstError = err;
        }
      }
    });
    if (didError) {
      throw firstError;
    }
    return update;
  } finally {
    isPerformingRefresh = false;
  }
}

export function register(type, id) {
  if (type === null) {
    return;
  }
  if (typeof type !== "function" && typeof type !== "object") {
    return;
  }

  if (allFamiliesByType.has(type)) {
    return;
  }
  let family = allFamiliesByID.get(id);
  if (family === undefined) {
    family = { current: type };
    allFamiliesByID.set(id, family);
  } else {
    pendingUpdates.push([family, type]);
  }
  allFamiliesByType.set(type, family);

  if (typeof type === "object" && type !== null) {
    switch (getProperty(type, "$$typeof")) {
      case REACT_FORWARD_REF_TYPE:
        register(type.render, id + "$render");
        break;
      case REACT_MEMO_TYPE:
        register(type.type, id + "$type");
        break;
    }
  }
}

function setSignature(type, key, forceReset, getCustomHooks) {
  if (!allSignaturesByType.has(type)) {
    allSignaturesByType.set(type, {
      forceReset,
      ownKey: key,
      fullKey: null,
      getCustomHooks: getCustomHooks || (() => []),
    });
  }
  if (typeof type === "object" && type !== null) {
    switch (getProperty(type, "$$typeof")) {
      case REACT_FORWARD_REF_TYPE:
        setSignature(type.render, key, forceReset, getCustomHooks);
        break;
      case REACT_MEMO_TYPE:
        setSignature(type.type, key, forceReset, getCustomHooks);
        break;
    }
  }
}

function collectCustomHooksForSignature(type) {
  const signature = allSignaturesByType.get(type);
  if (signature !== undefined) {
    computeFullKey(signature);
  }
}

export function injectIntoGlobalHook(globalObject) {
  // Mark refresh as installed for oj's compiled boundary modules. oj's own
  // refresh-preamble.js sets this, but a TanStack Start app installs refresh via
  // its dev client entry (which calls this), where oj's preamble is not injected.
  globalObject.__oj_refresh_installed__ = true;

  let hook = globalObject.__REACT_DEVTOOLS_GLOBAL_HOOK__;
  if (hook === undefined) {
    let nextID = 0;
    globalObject.__REACT_DEVTOOLS_GLOBAL_HOOK__ = hook = {
      renderers: new Map(),
      supportsFiber: true,
      inject: (injected) => nextID++,
      onScheduleFiberRoot: (id, root, children) => {},
      onCommitFiberRoot: (id, root, maybePriorityLevel, didError) => {},
      onCommitFiberUnmount() {},
    };
  }

  if (hook.isDisabled) {
    console["warn"](
      "Something has shimmed the React DevTools global hook (__REACT_DEVTOOLS_GLOBAL_HOOK__). " +
        "Fast Refresh is not compatible with this shim and will be disabled.",
    );
    return;
  }

  const oldInject = hook.inject;
  hook.inject = function (injected) {
    const id = oldInject.apply(this, arguments);
    if (typeof injected.scheduleRefresh === "function" && typeof injected.setRefreshHandler === "function") {
      helpersByRendererID.set(id, injected);
    }
    return id;
  };

  hook.renderers.forEach((injected, id) => {
    if (typeof injected.scheduleRefresh === "function" && typeof injected.setRefreshHandler === "function") {
      helpersByRendererID.set(id, injected);
    }
  });

  const oldOnCommitFiberRoot = hook.onCommitFiberRoot;
  const oldOnScheduleFiberRoot = hook.onScheduleFiberRoot || (() => {});
  hook.onScheduleFiberRoot = function (id, root, children) {
    if (!isPerformingRefresh) {
      failedRoots.delete(root);
      if (rootElements !== null) {
        rootElements.set(root, children);
      }
    }
    return oldOnScheduleFiberRoot.apply(this, arguments);
  };
  hook.onCommitFiberRoot = function (id, root, maybePriorityLevel, didError) {
    const helpers = helpersByRendererID.get(id);
    if (helpers !== undefined) {
      helpersByRoot.set(root, helpers);

      const current = root.current;
      const alternate = current.alternate;

      if (alternate !== null) {
        const wasMounted =
          alternate.memoizedState != null && alternate.memoizedState.element != null && mountedRoots.has(root);

        const isMounted = current.memoizedState != null && current.memoizedState.element != null;

        if (!wasMounted && isMounted) {
          mountedRoots.add(root);
          failedRoots.delete(root);
        } else if (wasMounted && isMounted) {
        } else if (wasMounted && !isMounted) {
          mountedRoots.delete(root);
          if (didError) {
            failedRoots.add(root);
          } else {
            helpersByRoot.delete(root);
          }
        } else if (!wasMounted && !isMounted) {
          if (didError) {
            failedRoots.add(root);
          }
        }
      } else {
        mountedRoots.add(root);
      }
    }

    return oldOnCommitFiberRoot.apply(this, arguments);
  };
}

export function createSignatureFunctionForTransform() {
  let savedType;
  let hasCustomHooks;
  let didCollectHooks = false;
  return function (type, key, forceReset, getCustomHooks) {
    if (typeof key === "string") {
      if (!savedType) {
        savedType = type;
        hasCustomHooks = typeof getCustomHooks === "function";
      }
      if (type != null && (typeof type === "function" || typeof type === "object")) {
        setSignature(type, key, forceReset, getCustomHooks);
      }
      return type;
    } else {
      if (!didCollectHooks && hasCustomHooks) {
        didCollectHooks = true;
        collectCustomHooksForSignature(savedType);
      }
    }
  };
}

function isLikelyComponentType(type) {
  switch (typeof type) {
    case "function": {
      if (type.prototype != null) {
        if (type.prototype.isReactComponent) {
          return true;
        }
        const ownNames = Object.getOwnPropertyNames(type.prototype);
        if (ownNames.length > 1 || ownNames[0] !== "constructor") {
          return false;
        }

        if (type.prototype.__proto__ !== Object.prototype) {
          return false;
        }
      }
      const name = type.name || type.displayName;
      return typeof name === "string" && /^[A-Z]/.test(name);
    }
    case "object": {
      if (type != null) {
        switch (getProperty(type, "$$typeof")) {
          case REACT_FORWARD_REF_TYPE:
          case REACT_MEMO_TYPE:
            return true;
          default:
            return false;
        }
      }
      return false;
    }
    default: {
      return false;
    }
  }
}

function isCompoundComponent(type) {
  if (!isPlainObject(type)) return false;
  for (const key in type) {
    if (!isLikelyComponentType(type[key])) return false;
  }
  return true;
}

function isPlainObject(obj) {
  return (
    Object.prototype.toString.call(obj) === "[object Object]" &&
    (obj.constructor === Object || obj.constructor === undefined)
  );
}

export function registerExportsForReactRefresh(filename, moduleExports) {
  for (const key in moduleExports) {
    if (key === "__esModule") continue;
    const exportValue = moduleExports[key];
    if (isLikelyComponentType(exportValue)) {
      register(exportValue, filename + " export " + key);
    } else if (isCompoundComponent(exportValue)) {
      for (const subKey in exportValue) {
        register(exportValue[subKey], filename + " export " + key + "-" + subKey);
      }
    }
  }
}

function debounce(fn, delay) {
  let handle;
  return () => {
    clearTimeout(handle);
    handle = setTimeout(fn, delay);
  };
}

const hooks = [];
window.__registerBeforePerformReactRefresh = (cb) => {
  hooks.push(cb);
};
export const enqueueUpdate = debounce(async () => {
  if (hooks.length) await Promise.all(hooks.map((cb) => cb()));
  performReactRefresh();
}, 16);

export function validateRefreshBoundaryAndEnqueueUpdate(id, prevExports, nextExports) {
  const ignoredExports = window.__getReactRefreshIgnoredExports?.({ id }) ?? [];
  if (predicateOnExport(ignoredExports, prevExports, (key) => key in nextExports) !== true) {
    return "Could not Fast Refresh (export removed)";
  }
  if (predicateOnExport(ignoredExports, nextExports, (key) => key in prevExports) !== true) {
    return "Could not Fast Refresh (new export)";
  }

  let hasExports = false;
  const allExportsAreComponentsOrUnchanged = predicateOnExport(ignoredExports, nextExports, (key, value) => {
    hasExports = true;
    if (isLikelyComponentType(value)) return true;
    if (isCompoundComponent(value)) return true;
    return prevExports[key] === nextExports[key];
  });
  if (hasExports && allExportsAreComponentsOrUnchanged === true) {
    enqueueUpdate();
  } else {
    return `Could not Fast Refresh ("${allExportsAreComponentsOrUnchanged}" export is incompatible). Learn more at __README_URL__#consistent-components-exports`;
  }
}

function predicateOnExport(ignoredExports, moduleExports, predicate) {
  for (const key in moduleExports) {
    if (ignoredExports.includes(key)) continue;
    if (!predicate(key, moduleExports[key])) return key;
  }
  return true;
}

export const __hmr_import = (module) => import(module);

export default { injectIntoGlobalHook };
