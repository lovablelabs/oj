// \0rolldown/runtime.js
var __defProp = Object.defineProperty;
var __exportAll = (all, flags) => {
	let target = Object.setPrototypeOf({}, null);
	for (var name in all) __defProp(target, name, {
		get: all[name],
		enumerable: true
	});
	if (!(flags & 1)) __defProp(target, Symbol.toStringTag, { value: "Module" });
	return flags & 2 ? target : Object.preventExtensions(target);
};
// \0vite/modulepreload-HASH.js
(function polyfill() {
	const relList = document.createElement("link").relList;
	if (relList && relList.supports && relList.supports("modulepreload")) return;
	for (const link of document.querySelectorAll("link[rel=\"modulepreload\"]")) processPreload(link);
	new MutationObserver((mutations) => {
		for (const mutation of mutations) {
			if (mutation.type !== "childList") continue;
			for (const node of mutation.addedNodes) if (node.tagName === "LINK" && node.rel === "modulepreload") processPreload(node);
		}
	}).observe(document, {
		childList: true,
		subtree: true
	});
	function getFetchOpts(link) {
		const fetchOpts = {};
		if (link.integrity) fetchOpts.integrity = link.integrity;
		if (link.referrerPolicy) fetchOpts.referrerPolicy = link.referrerPolicy;
		if (link.crossOrigin === "use-credentials") fetchOpts.credentials = "include";
		else if (link.crossOrigin === "anonymous") fetchOpts.credentials = "omit";
		else fetchOpts.credentials = "same-origin";
		return fetchOpts;
	}
	function processPreload(link) {
		if (link.ep) return;
		link.ep = true;
		const fetchOpts = getFetchOpts(link);
		fetch(link.href, fetchOpts);
	}
})();
// src/texts/a.txt?raw
var a_exports = /* @__PURE__ */ __exportAll({ default: () => a_default });
var a_default = "alpha\n";
// src/texts/b.txt?raw
var b_exports = /* @__PURE__ */ __exportAll({ default: () => b_default });
var b_default = "beta\n";
// \0rolldown_dynamic_import_helper.js
var _rolldown_dynamic_import_helper_default = (glob, path, segments) => {
	const query = path.lastIndexOf("?");
	const v = glob[query === -1 || query < path.lastIndexOf("/") ? path : path.slice(0, query)];
	if (v) return typeof v === "function" ? v() : Promise.resolve(v);
	return new Promise((_, reject) => {
		(typeof queueMicrotask === "function" ? queueMicrotask : setTimeout)(reject.bind(null, /* @__PURE__ */ new Error("Unknown variable dynamic import: " + path + (path.split("/").length !== segments ? ". Note that variables only represent file names one level deep." : ""))));
	});
};
// \0vite/preload-helper.js
var scriptRel = "modulepreload";
var assetsURL = function(dep) {
	return "/" + dep;
};
var seen = {};
var failedCssLinks = {};
var isCssPreloadUrl = function isCssPreloadUrl(url) {
	return url.pathname.endsWith(".css");
};
var preloadOnce = function preloadOnce(seen, href, preload) {
	if (href in seen) return seen[href];
	const promise = preload();
	if (!promise) {
		seen[href] = void 0;
		return;
	}
	const preloadPromise = promise.then(() => {
		seen[href] = void 0;
	}, (err) => {
		delete seen[href];
		throw err;
	});
	seen[href] = preloadPromise;
	return preloadPromise;
};
var __vitePreload = function preload(baseModule, deps, importerUrl) {
	let promise = Promise.resolve();
	if (deps && deps.length > 0) {
		let preloadedHrefs;
		const cspNonceMeta = document.querySelector("meta[property=csp-nonce]");
		const cspNonce = cspNonceMeta?.nonce || cspNonceMeta?.getAttribute("nonce");
		function allSettled(promises) {
			return Promise.all(promises.map((p) => Promise.resolve(p).then((value) => ({
				status: "fulfilled",
				value
			}), (reason) => ({
				status: "rejected",
				reason
			}))));
		}
		function importMetaResolve(specifier) {
			if (import.meta.resolve) return new URL(import.meta.resolve(specifier));
			return new URL(
				specifier,
				/** #__KEEP__ */
				import.meta.url
			);
		}
		promise = allSettled(deps.map((depString) => {
			depString = assetsURL(depString, importerUrl);
			const dep = importMetaResolve(depString);
			const isCss = isCssPreloadUrl(dep);
			return preloadOnce(seen, dep.href, () => {
				if (preloadedHrefs === void 0) {
					preloadedHrefs = {
						all: /* @__PURE__ */ new Set(),
						styles: /* @__PURE__ */ new Set()
					};
					const links = document.getElementsByTagName("link");
					for (let i = links.length - 1; i >= 0; i--) {
						const link = links[i];
						preloadedHrefs.all.add(link.href);
						if (link.rel === "stylesheet") preloadedHrefs.styles.add(link.href);
					}
				}
				const preloadedHrefSet = isCss ? preloadedHrefs.styles : preloadedHrefs.all;
				const failedCssLink = isCss ? failedCssLinks[dep.href] : void 0;
				if (!failedCssLink && preloadedHrefSet.has(dep.href)) return;
				const link = document.createElement("link");
				link.rel = isCss ? "stylesheet" : scriptRel;
				if (!isCss) link.as = "script";
				link.crossOrigin = "";
				link.href = dep.href;
				if (cspNonce) link.setAttribute("nonce", cspNonce);
				if (failedCssLink) delete failedCssLinks[dep.href];
				if (failedCssLink?.isConnected) failedCssLink.replaceWith(link);
				else document.head.appendChild(link);
				if (isCss) return new Promise((res, rej) => {
					const handleSettle = (e) => {
						link.removeEventListener("load", handleSettle);
						link.removeEventListener("error", handleSettle);
						if (e.type === "error") {
							failedCssLinks[dep.href] = link;
							rej(/* @__PURE__ */ new Error(`Unable to preload CSS for ${dep}`));
						} else res(e);
					};
					link.addEventListener("load", handleSettle);
					link.addEventListener("error", handleSettle);
				});
			});
		}).filter((p) => p !== void 0));
	}
	function handlePreloadError(err) {
		const e = new Event("vite:preloadError", { cancelable: true });
		e.payload = err;
		window.dispatchEvent(e);
		if (!e.defaultPrevented) throw err;
	}
	return promise.then((res) => {
		for (const item of res || []) {
			if (item.status !== "rejected") continue;
			handlePreloadError(item.reason);
		}
		return baseModule().catch(handlePreloadError);
	});
};
// src/main.js
var pages = /* #__PURE__ */ Object.assign({
	"./pages/about/index.js": () => __vitePreload(() => import("./about-HASH1.js"), []),
	"./pages/about.js": () => __vitePreload(() => import("./about-HASH2.js"), []),
	"./pages/home.js": () => __vitePreload(() => import("./home-HASH.js"), [])
});
var texts = /* #__PURE__ */ Object.assign({
	"./texts/a.txt": a_default,
	"./texts/b.txt": b_default
});
async function page(name) {
	const load = pages[`./pages/${name}.js`];
	return load ? load() : null;
}
async function locale(lang) {
	return _rolldown_dynamic_import_helper_default(/* #__PURE__ */ Object.assign({
		"./locales/en.js": () => __vitePreload(() => import("./en-HASH.js"), []),
		"./locales/fr.js": () => __vitePreload(() => import("./fr-HASH.js"), [])
	}), `./locales/${lang}.js`, 3);
}
async function note(name) {
	return _rolldown_dynamic_import_helper_default(/* #__PURE__ */ Object.assign({
		"./texts/a.txt": () => __vitePreload(() => Promise.resolve().then(() => a_exports), void 0),
		"./texts/b.txt": () => __vitePreload(() => Promise.resolve().then(() => b_exports), void 0)
	}), `./texts/${name}.txt?raw`, 3);
}
async function bare(name) {
	return __vitePreload(() => import(`./pages/${name}`), []);
}
console.log(texts, page, locale, note, bare);

//# sourceMappingURL=index-HASH.js.map