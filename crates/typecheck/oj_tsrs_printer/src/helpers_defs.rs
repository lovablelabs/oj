// Port of the helper definitions of helpers.go (text copied byte for byte). Go keeps `*EmitHelper` package vars;
// here they are statics, handed out as `SP<EmitHelper>` (`SP::from_static`), so identity comparisons match Go.

use tsrs_core::SP;

use crate::{EmitHelper, Priority};

// helpers.go:35
pub(crate) static DECORATE_HELPER: EmitHelper = EmitHelper {
    import_name: "__decorate",
    scoped: false,
    priority: Some(&Priority { value: 2 }),
    text: "var __decorate = (this && this.__decorate) || function (decorators, target, key, desc) {
    var c = arguments.length, r = c < 3 ? target : desc === null ? desc = Object.getOwnPropertyDescriptor(target, key) : desc, d;
    if (typeof Reflect === \"object\" && typeof Reflect.decorate === \"function\") r = Reflect.decorate(decorators, target, key, desc);
    else for (var i = decorators.length - 1; i >= 0; i--) if (d = decorators[i]) r = (c < 3 ? d(r) : c > 3 ? d(target, key, r) : d(target, key)) || r;
    return c > 3 && r && Object.defineProperty(target, key, r), r;
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:48
pub(crate) static METADATA_HELPER: EmitHelper = EmitHelper {
    import_name: "__metadata",
    scoped: false,
    priority: Some(&Priority { value: 3 }),
    text: "var __metadata = (this && this.__metadata) || function (k, v) {
    if (typeof Reflect === \"object\" && typeof Reflect.metadata === \"function\") return Reflect.metadata(k, v);
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:58
pub(crate) static PARAM_HELPER: EmitHelper = EmitHelper {
    import_name: "__param",
    scoped: false,
    priority: Some(&Priority { value: 4 }),
    text: "var __param = (this && this.__param) || function (paramIndex, decorator) {
    return function (target, key) { decorator(target, key, paramIndex); }
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:70
pub(crate) static ADD_DISPOSABLE_RESOURCE_HELPER: EmitHelper = EmitHelper {
    import_name: "__addDisposableResource",
    scoped: false,
    priority: None,
    text: "var __addDisposableResource = (this && this.__addDisposableResource) || function (env, value, async) {
    if (value !== null && value !== void 0) {
        if (typeof value !== \"object\" && typeof value !== \"function\") throw new TypeError(\"Object expected.\");
        var dispose, inner;
        if (async) {
            if (!Symbol.asyncDispose) throw new TypeError(\"Symbol.asyncDispose is not defined.\");
            dispose = value[Symbol.asyncDispose];
        }
        if (dispose === void 0) {
            if (!Symbol.dispose) throw new TypeError(\"Symbol.dispose is not defined.\");
            dispose = value[Symbol.dispose];
            if (async) inner = dispose;
        }
        if (typeof dispose !== \"function\") throw new TypeError(\"Object not disposable.\");
        if (inner) dispose = function() { try { inner.call(this); } catch (e) { return Promise.reject(e); } };
        env.stack.push({ value: value, dispose: dispose, async: async });
    }
    else if (async) {
        env.stack.push({ async: true });
    }
    return value;
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:98
pub(crate) static DISPOSE_RESOURCES_HELPER: EmitHelper = EmitHelper {
    import_name: "__disposeResources",
    scoped: false,
    priority: None,
    text: "var __disposeResources = (this && this.__disposeResources) || (function (SuppressedError) {
    return function (env) {
        function fail(e) {
            env.error = env.hasError ? new SuppressedError(e, env.error, \"An error was suppressed during disposal.\") : e;
            env.hasError = true;
        }
        var r, s = 0;
        function next() {
            while (r = env.stack.pop()) {
                try {
                    if (!r.async && s === 1) return s = 0, env.stack.push(r), Promise.resolve().then(next);
                    if (r.dispose) {
                        var result = r.dispose.call(r.value);
                        if (r.async) return s |= 2, Promise.resolve(result).then(next, function(e) { fail(e); return next(); });
                    }
                    else s |= 1;
                }
                catch (e) {
                    fail(e);
                }
            }
            if (s === 1) return env.hasError ? Promise.reject(env.error) : Promise.resolve();
            if (env.hasError) throw env.error;
        }
        return next();
    };
})(typeof SuppressedError === \"function\" ? SuppressedError : function (error, suppressed, message) {
    var e = new Error(message);
    return e.name = \"SuppressedError\", e.error = error, e.suppressed = suppressed, e;
});",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:184
pub(crate) static CLASS_PRIVATE_FIELD_GET_HELPER: EmitHelper = EmitHelper {
    import_name: "__classPrivateFieldGet",
    scoped: false,
    priority: None,
    text: "var __classPrivateFieldGet = (this && this.__classPrivateFieldGet) || function (receiver, state, kind, f) {
    if (kind === \"a\" && !f) throw new TypeError(\"Private accessor was defined without a getter\");
    if (typeof state === \"function\" ? receiver !== state || !f : !state.has(receiver)) throw new TypeError(\"Cannot read private member from an object whose class did not declare it\");
    return kind === \"m\" ? f : kind === \"a\" ? f.call(receiver) : f ? f.value : state.get(receiver);
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:246
pub(crate) static CLASS_PRIVATE_FIELD_SET_HELPER: EmitHelper = EmitHelper {
    import_name: "__classPrivateFieldSet",
    scoped: false,
    priority: None,
    text: "var __classPrivateFieldSet = (this && this.__classPrivateFieldSet) || function (receiver, state, value, kind, f) {
    if (kind === \"m\") throw new TypeError(\"Private method is not writable\");
    if (kind === \"a\" && !f) throw new TypeError(\"Private accessor was defined without a setter\");
    if (typeof state === \"function\" ? receiver !== state || !f : !state.has(receiver)) throw new TypeError(\"Cannot write private member to an object whose class did not declare it\");
    return (kind === \"a\" ? f.call(receiver, value) : f ? f.value = value : state.set(receiver, value)), value;
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:270
pub(crate) static CLASS_PRIVATE_FIELD_IN_HELPER: EmitHelper = EmitHelper {
    import_name: "__classPrivateFieldIn",
    scoped: false,
    priority: None,
    text: "var __classPrivateFieldIn = (this && this.__classPrivateFieldIn) || function(state, receiver) {
    if (receiver === null || (typeof receiver !== \"object\" && typeof receiver !== \"function\")) throw new TypeError(\"Cannot use 'in' operator on non-object\");
    return typeof state === \"function\" ? receiver === state : state.has(receiver);
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:282
pub(crate) static AWAIT_HELPER: EmitHelper = EmitHelper {
    import_name: "__await",
    scoped: false,
    priority: None,
    text: "var __await = (this && this.__await) || function (v) { return this instanceof __await ? (this.v = v, this) : new __await(v); }",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:289
pub(crate) static ASYNC_GENERATOR_HELPER: EmitHelper = EmitHelper {
    import_name: "__asyncGenerator",
    scoped: false,
    priority: None,
    text: "var __asyncGenerator = (this && this.__asyncGenerator) || function (thisArg, _arguments, generator) {
    if (!Symbol.asyncIterator) throw new TypeError(\"Symbol.asyncIterator is not defined.\");
    var g = generator.apply(thisArg, _arguments || []), i, q = [];
    return i = Object.create((typeof AsyncIterator === \"function\" ? AsyncIterator : Object).prototype), verb(\"next\"), verb(\"throw\"), verb(\"return\", awaitReturn), i[Symbol.asyncIterator] = function () { return this; }, i;
    function awaitReturn(f) { return function (v) { return Promise.resolve(v).then(f, reject); }; }
    function verb(n, f) { if (g[n]) { i[n] = function (v) { return new Promise(function (a, b) { q.push([n, v, a, b]) > 1 || resume(n, v); }); }; if (f) i[n] = f(i[n]); } }
    function resume(n, v) { try { step(g[n](v)); } catch (e) { settle(q[0][3], e); } }
    function step(r) { r.value instanceof __await ? Promise.resolve(r.value.v).then(fulfill, reject) : settle(q[0][2], r); }
    function fulfill(value) { resume(\"next\", value); }
    function reject(value) { resume(\"throw\", value); }
    function settle(f, v) { if (f(v), q.shift(), q.length) resume(q[0][0], q[0][1]); }
};",
    text_callback: None,
    dependencies: &[SP::from_static(&AWAIT_HELPER)],
};

// helpers.go:308
pub(crate) static ASYNC_DELEGATOR_HELPER: EmitHelper = EmitHelper {
    import_name: "__asyncDelegator",
    scoped: false,
    priority: None,
    text: "var __asyncDelegator = (this && this.__asyncDelegator) || function (o) {
    var i, p;
    return i = {}, verb(\"next\"), verb(\"throw\", function (e) { throw e; }), verb(\"return\"), i[Symbol.iterator] = function () { return this; }, i;
    function verb(n, f) { i[n] = o[n] ? function (v) { return (p = !p) ? { value: __await(o[n](v)), done: false } : f ? f(v) : v; } : f; }
};",
    text_callback: None,
    dependencies: &[SP::from_static(&AWAIT_HELPER)],
};

// helpers.go:320
pub(crate) static ASYNC_VALUES_HELPER: EmitHelper = EmitHelper {
    import_name: "__asyncValues",
    scoped: false,
    priority: None,
    text: "var __asyncValues = (this && this.__asyncValues) || function (o) {
    if (!Symbol.asyncIterator) throw new TypeError(\"Symbol.asyncIterator is not defined.\");
    var m = o[Symbol.asyncIterator], i;
    return m ? m.call(o) : (o = typeof __values === \"function\" ? __values(o) : o[Symbol.iterator](), i = {}, verb(\"next\"), verb(\"throw\"), verb(\"return\"), i[Symbol.asyncIterator] = function () { return this; }, i);
    function verb(n) { i[n] = o[n] && function (v) { return new Promise(function (resolve, reject) { v = o[n](v), settle(resolve, reject, v.done, v.value); }); }; }
    function settle(resolve, reject, d, v) { Promise.resolve(v).then(function(v) { resolve({ value: v, done: d }); }, reject); }
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:334
pub(crate) static REST_HELPER: EmitHelper = EmitHelper {
    import_name: "__rest",
    scoped: false,
    priority: None,
    text: "var __rest = (this && this.__rest) || function (s, e) {
    var t = {};
    for (var p in s) if (Object.prototype.hasOwnProperty.call(s, p) && e.indexOf(p) < 0)
        t[p] = s[p];
    if (s != null && typeof Object.getOwnPropertySymbols === \"function\")
        for (var i = 0, p = Object.getOwnPropertySymbols(s); i < p.length; i++) {
            if (e.indexOf(p[i]) < 0 && Object.prototype.propertyIsEnumerable.call(s, p[i]))
                t[p[i]] = s[p[i]];
        }
    return t;
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:351
pub(crate) static AWAITER_HELPER: EmitHelper = EmitHelper {
    import_name: "__awaiter",
    scoped: false,
    priority: Some(&Priority { value: 5 }),
    text: "var __awaiter = (this && this.__awaiter) || function (thisArg, _arguments, P, generator) {
    function adopt(value) { return value instanceof P ? value : new P(function (resolve) { resolve(value); }); }
    return new (P || (P = Promise))(function (resolve, reject) {
        function fulfilled(value) { try { step(generator.next(value)); } catch (e) { reject(e); } }
        function rejected(value) { try { step(generator[\"throw\"](value)); } catch (e) { reject(e); } }
        function step(result) { result.done ? resolve(result.value) : adopt(result.value).then(fulfilled, rejected); }
        step((generator = generator.apply(thisArg, _arguments || [])).next());
    });
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:367
pub static ASYNC_SUPER_HELPER: EmitHelper = EmitHelper {
    import_name: "",
    scoped: true,
    priority: None,
    text: "",
    text_callback: Some(async_super_helper_text_callback),
    dependencies: &[],
};

// helpers.go:375
pub static ADVANCED_ASYNC_SUPER_HELPER: EmitHelper = EmitHelper {
    import_name: "",
    scoped: true,
    priority: None,
    text: "",
    text_callback: Some(advanced_async_super_helper_text_callback),
    dependencies: &[],
};

// helpers.go:388
pub(crate) static ES_DECORATE_HELPER: EmitHelper = EmitHelper {
    import_name: "__esDecorate",
    scoped: false,
    priority: Some(&Priority { value: 2 }),
    text: "var __esDecorate = (this && this.__esDecorate) || function (ctor, descriptorIn, decorators, contextIn, initializers, extraInitializers) {
    function accept(f) { if (f !== void 0 && typeof f !== \"function\") throw new TypeError(\"Function expected\"); return f; }
    var kind = contextIn.kind, key = kind === \"getter\" ? \"get\" : kind === \"setter\" ? \"set\" : \"value\";
    var target = !descriptorIn && ctor ? contextIn[\"static\"] ? ctor : ctor.prototype : null;
    var descriptor = descriptorIn || (target ? Object.getOwnPropertyDescriptor(target, contextIn.name) : {});
    var _, done = false;
    for (var i = decorators.length - 1; i >= 0; i--) {
        var context = {};
        for (var p in contextIn) context[p] = p === \"access\" ? {} : contextIn[p];
        for (var p in contextIn.access) context.access[p] = contextIn.access[p];
        context.addInitializer = function (f) { if (done) throw new TypeError(\"Cannot add initializers after decoration has completed\"); extraInitializers.push(accept(f || null)); };
        var result = (0, decorators[i])(kind === \"accessor\" ? { get: descriptor.get, set: descriptor.set } : descriptor[key], context);
        if (kind === \"accessor\") {
            if (result === void 0) continue;
            if (result === null || typeof result !== \"object\") throw new TypeError(\"Object expected\");
            if (_ = accept(result.get)) descriptor.get = _;
            if (_ = accept(result.set)) descriptor.set = _;
            if (_ = accept(result.init)) initializers.unshift(_);
        }
        else if (_ = accept(result)) {
            if (kind === \"field\") initializers.unshift(_);
            else descriptor[key] = _;
        }
    }
    if (target) Object.defineProperty(target, contextIn.name, descriptor);
    done = true;
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:422
pub(crate) static RUN_INITIALIZERS_HELPER: EmitHelper = EmitHelper {
    import_name: "__runInitializers",
    scoped: false,
    priority: Some(&Priority { value: 2 }),
    text: "var __runInitializers = (this && this.__runInitializers) || function (thisArg, initializers, value) {
    var useValue = arguments.length > 2;
    for (var i = 0; i < initializers.length; i++) {
        value = useValue ? initializers[i].call(thisArg, value) : initializers[i].call(thisArg);
    }
    return useValue ? value : void 0;
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:438
pub(crate) static MAKE_TEMPLATE_OBJECT_HELPER: EmitHelper = EmitHelper {
    import_name: "__makeTemplateObject",
    scoped: false,
    priority: Some(&Priority { value: 0 }),
    text: "var __makeTemplateObject = (this && this.__makeTemplateObject) || function (cooked, raw) {
    if (Object.defineProperty) { Object.defineProperty(cooked, \"raw\", { value: raw }); } else { cooked.raw = raw; }
    return cooked;
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:449
pub(crate) static PROP_KEY_HELPER: EmitHelper = EmitHelper {
    import_name: "__propKey",
    scoped: false,
    priority: None,
    text: "var __propKey = (this && this.__propKey) || function (x) {
    return typeof x === \"symbol\" ? x : \"\".concat(x);
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:459
pub(crate) static SET_FUNCTION_NAME_HELPER: EmitHelper = EmitHelper {
    import_name: "__setFunctionName",
    scoped: false,
    priority: None,
    text: "var __setFunctionName = (this && this.__setFunctionName) || function (f, name, prefix) {
    if (typeof name === \"symbol\") name = name.description ? \"[\".concat(name.description, \"]\") : \"\";
    return Object.defineProperty(f, \"name\", { configurable: true, value: prefix ? \"\".concat(prefix, \" \", name) : name });
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:471
static CREATE_BINDING_HELPER: EmitHelper = EmitHelper {
    import_name: "__createBinding",
    scoped: false,
    priority: Some(&Priority { value: 1 }),
    text: "var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || (\"get\" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:489
static SET_MODULE_DEFAULT_HELPER: EmitHelper = EmitHelper {
    import_name: "__setModuleDefault",
    scoped: false,
    priority: Some(&Priority { value: 1 }),
    text: "var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, \"default\", { enumerable: true, value: v });
}) : function(o, v) {
    o[\"default\"] = v;
});",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:501
pub(crate) static IMPORT_STAR_HELPER: EmitHelper = EmitHelper {
    import_name: "__importStar",
    scoped: false,
    priority: Some(&Priority { value: 2 }),
    text: "var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== \"default\") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();",
    text_callback: None,
    dependencies: &[SP::from_static(&CREATE_BINDING_HELPER), SP::from_static(&SET_MODULE_DEFAULT_HELPER)],
};

// helpers.go:526
pub(crate) static IMPORT_DEFAULT_HELPER: EmitHelper = EmitHelper {
    import_name: "__importDefault",
    scoped: false,
    priority: None,
    text: "var __importDefault = (this && this.__importDefault) || function (mod) {
    return (mod && mod.__esModule) ? mod : { \"default\": mod };
};",
    text_callback: None,
    dependencies: &[],
};

// helpers.go:535
pub(crate) static EXPORT_STAR_HELPER: EmitHelper = EmitHelper {
    import_name: "__exportStar",
    scoped: false,
    priority: Some(&Priority { value: 2 }),
    text: "var __exportStar = (this && this.__exportStar) || function(m, exports) {
    for (var p in m) if (p !== \"default\" && !Object.prototype.hasOwnProperty.call(exports, p)) __createBinding(exports, m, p);
};",
    text_callback: None,
    dependencies: &[SP::from_static(&CREATE_BINDING_HELPER)],
};

// helpers.go:546
pub(crate) static REWRITE_RELATIVE_IMPORT_EXTENSIONS_HELPER: EmitHelper = EmitHelper {
    import_name: "__rewriteRelativeImportExtension",
    scoped: false,
    priority: None,
    text: "var __rewriteRelativeImportExtension = (this && this.__rewriteRelativeImportExtension) || function (path, preserveJsx, extraExtensions, ignoreCase) {
    if (typeof path === \"string\" && /^\\.\\.?\\//.test(path)) {
        if (extraExtensions) {
            for (var extension in extraExtensions) {
                var suffix = path.slice(-extension.length);
                if (suffix === extension || ignoreCase && suffix.toLowerCase() === extension.toLowerCase()) {
                    var output = extraExtensions[extension];
                    return output === extension ? path : path.slice(0, -extension.length) + output;
                }
            }
        }
        return path.replace(/\\.(tsx)$|((?:\\.d)?)((?:\\.[^./]+?)?)\\.([cm]?)ts$/i, function (m, tsx, d, ext, cm) {
            return tsx ? preserveJsx ? \".jsx\" : \".js\" : d && (!ext || !cm) ? m : (d + ext + \".\" + cm.toLowerCase() + \"js\");
        });
    }
    return path;
};",
    text_callback: None,
    dependencies: &[],
};
// helpers.go:370
fn async_super_helper_text_callback(make_unique_name: &mut dyn FnMut(&str) -> String) -> String {
    "\nconst ".to_string() + &make_unique_name("_superIndex") + " = name => super[name];"
}

// helpers.go:378
fn advanced_async_super_helper_text_callback(
    make_unique_name: &mut dyn FnMut(&str) -> String,
) -> String {
    "\nconst ".to_string()
        + &make_unique_name("_superIndex")
        + " = (function (geti, seti) {\n"
        + "    const cache = Object.create(null);\n"
        + "    return name => cache[name] || (cache[name] = { get value() { return geti(name); }, set value(v) { seti(name, v); } });\n"
        + "})(name => super[name], (name, value) => super[name] = value);"
}
