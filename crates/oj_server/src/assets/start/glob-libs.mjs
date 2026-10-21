/*! picomatch 4.0.5, tinyglobby 0.2.17 and fdir 6.5.0 (Vite 8's glob libraries),
 * bundled unmodified into one module for oj: vite-globs.mjs uses it when the
 * app has no Vite of its own.
 *
 * --- picomatch ---
 * The MIT License (MIT)
 *
 * Copyright (c) 2017-present, Jon Schlinkert.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 *
 * --- tinyglobby ---
 * MIT License
 *
 * Copyright (c) 2024 Madeline Gurriarán
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 * --- fdir ---
 * Copyright 2023 Abdullah Atta
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

import * as nativeFs from "fs";
import { readdir, readdirSync, realpath, realpathSync, stat, statSync } from "fs";
import { basename, dirname, isAbsolute, normalize, posix, relative, resolve, sep } from "path";
import { fileURLToPath } from "url";
import { createRequire } from "module";

//#region \0rolldown/runtime.js
var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __commonJSMin = (cb, mod) => () => (mod || (cb((mod = { exports: {} }).exports, mod), cb = null), mod.exports);
var __exportAll = (all, no_symbols) => {
	let target = {};
	for (var name in all) {
		__defProp(target, name, {
			get: all[name],
			enumerable: true
		});
	}
	if (!no_symbols) {
		__defProp(target, Symbol.toStringTag, { value: "Module" });
	}
	return target;
};
var __copyProps = (to, from, except, desc) => {
	if (from && typeof from === "object" || typeof from === "function") {
		for (var keys = __getOwnPropNames(from), i = 0, n = keys.length, key; i < n; i++) {
			key = keys[i];
			if (!__hasOwnProp.call(to, key) && key !== except) {
				__defProp(to, key, {
					get: ((k) => from[k]).bind(null, key),
					enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable
				});
			}
		}
	}
	return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(isNodeMode || !mod || !mod.__esModule || !__hasOwnProp.call(mod, "default") ? __defProp(target, "default", {
	value: mod,
	enumerable: true
}) : target, mod));

//#endregion
//#region node_modules/picomatch/lib/constants.js
var require_constants = /* @__PURE__ */ __commonJSMin(((exports, module) => {
	const WIN_SLASH = "\\\\/";
	const WIN_NO_SLASH = `[^${WIN_SLASH}]`;
	const DEFAULT_MAX_EXTGLOB_RECURSION = 0;
	/**
	* Posix glob regex
	*/
	const DOT_LITERAL = "\\.";
	const PLUS_LITERAL = "\\+";
	const QMARK_LITERAL = "\\?";
	const SLASH_LITERAL = "\\/";
	const ONE_CHAR = "(?=.)";
	const QMARK = "[^/]";
	const END_ANCHOR = `(?:${SLASH_LITERAL}|$)`;
	const START_ANCHOR = `(?:^|${SLASH_LITERAL})`;
	const DOTS_SLASH = `${DOT_LITERAL}{1,2}${END_ANCHOR}`;
	const POSIX_CHARS = {
		DOT_LITERAL,
		PLUS_LITERAL,
		QMARK_LITERAL,
		SLASH_LITERAL,
		ONE_CHAR,
		QMARK,
		END_ANCHOR,
		DOTS_SLASH,
		NO_DOT: `(?!${DOT_LITERAL})`,
		NO_DOTS: `(?!${START_ANCHOR}${DOTS_SLASH})`,
		NO_DOT_SLASH: `(?!${DOT_LITERAL}{0,1}${END_ANCHOR})`,
		NO_DOTS_SLASH: `(?!${DOTS_SLASH})`,
		QMARK_NO_DOT: `[^.${SLASH_LITERAL}]`,
		STAR: `${QMARK}*?`,
		START_ANCHOR,
		SEP: "/"
	};
	/**
	* Windows glob regex
	*/
	const WINDOWS_CHARS = {
		...POSIX_CHARS,
		SLASH_LITERAL: `[${WIN_SLASH}]`,
		QMARK: WIN_NO_SLASH,
		STAR: `${WIN_NO_SLASH}*?`,
		DOTS_SLASH: `${DOT_LITERAL}{1,2}(?:[${WIN_SLASH}]|$)`,
		NO_DOT: `(?!${DOT_LITERAL})`,
		NO_DOTS: `(?!(?:^|[${WIN_SLASH}])${DOT_LITERAL}{1,2}(?:[${WIN_SLASH}]|$))`,
		NO_DOT_SLASH: `(?!${DOT_LITERAL}{0,1}(?:[${WIN_SLASH}]|$))`,
		NO_DOTS_SLASH: `(?!${DOT_LITERAL}{1,2}(?:[${WIN_SLASH}]|$))`,
		QMARK_NO_DOT: `[^.${WIN_SLASH}]`,
		START_ANCHOR: `(?:^|[${WIN_SLASH}])`,
		END_ANCHOR: `(?:[${WIN_SLASH}]|$)`,
		SEP: "\\"
	};
	/**
	* POSIX Bracket Regex
	*/
	const POSIX_REGEX_SOURCE = {
		__proto__: null,
		alnum: "a-zA-Z0-9",
		alpha: "a-zA-Z",
		ascii: "\\x00-\\x7F",
		blank: " \\t",
		cntrl: "\\x00-\\x1F\\x7F",
		digit: "0-9",
		graph: "\\x21-\\x7E",
		lower: "a-z",
		print: "\\x20-\\x7E ",
		punct: "\\-!\"#$%&'()\\*+,./:;<=>?@[\\]^_`{|}~",
		space: " \\t\\r\\n\\v\\f",
		upper: "A-Z",
		word: "A-Za-z0-9_",
		xdigit: "A-Fa-f0-9"
	};
	module.exports = {
		DEFAULT_MAX_EXTGLOB_RECURSION,
		MAX_LENGTH: 65536,
		POSIX_REGEX_SOURCE,
		REGEX_BACKSLASH: /\\(?![*+?^${}(|)[\]])/g,
		REGEX_NON_SPECIAL_CHARS: /^[^@![\].,$*+?^{}()|\\/]+/,
		REGEX_SPECIAL_CHARS: /[-*+?.^${}(|)[\]]/,
		REGEX_SPECIAL_CHARS_BACKREF: /(\\?)((\W)(\3*))/g,
		REGEX_SPECIAL_CHARS_GLOBAL: /([-*+?.^${}(|)[\]])/g,
		REGEX_REMOVE_BACKSLASH: /(?:\[.*?[^\\]\]|\\(?=.))/g,
		REPLACEMENTS: {
			__proto__: null,
			"***": "*",
			"**/**": "**",
			"**/**/**": "**"
		},
		CHAR_0: 48,
		CHAR_9: 57,
		CHAR_UPPERCASE_A: 65,
		CHAR_LOWERCASE_A: 97,
		CHAR_UPPERCASE_Z: 90,
		CHAR_LOWERCASE_Z: 122,
		CHAR_LEFT_PARENTHESES: 40,
		CHAR_RIGHT_PARENTHESES: 41,
		CHAR_ASTERISK: 42,
		CHAR_AMPERSAND: 38,
		CHAR_AT: 64,
		CHAR_BACKWARD_SLASH: 92,
		CHAR_CARRIAGE_RETURN: 13,
		CHAR_CIRCUMFLEX_ACCENT: 94,
		CHAR_COLON: 58,
		CHAR_COMMA: 44,
		CHAR_DOT: 46,
		CHAR_DOUBLE_QUOTE: 34,
		CHAR_EQUAL: 61,
		CHAR_EXCLAMATION_MARK: 33,
		CHAR_FORM_FEED: 12,
		CHAR_FORWARD_SLASH: 47,
		CHAR_GRAVE_ACCENT: 96,
		CHAR_HASH: 35,
		CHAR_HYPHEN_MINUS: 45,
		CHAR_LEFT_ANGLE_BRACKET: 60,
		CHAR_LEFT_CURLY_BRACE: 123,
		CHAR_LEFT_SQUARE_BRACKET: 91,
		CHAR_LINE_FEED: 10,
		CHAR_NO_BREAK_SPACE: 160,
		CHAR_PERCENT: 37,
		CHAR_PLUS: 43,
		CHAR_QUESTION_MARK: 63,
		CHAR_RIGHT_ANGLE_BRACKET: 62,
		CHAR_RIGHT_CURLY_BRACE: 125,
		CHAR_RIGHT_SQUARE_BRACKET: 93,
		CHAR_SEMICOLON: 59,
		CHAR_SINGLE_QUOTE: 39,
		CHAR_SPACE: 32,
		CHAR_TAB: 9,
		CHAR_UNDERSCORE: 95,
		CHAR_VERTICAL_LINE: 124,
		CHAR_ZERO_WIDTH_NOBREAK_SPACE: 65279,
		/**
		* Create EXTGLOB_CHARS
		*/
		extglobChars(chars) {
			return {
				"!": {
					type: "negate",
					open: "(?:(?!(?:",
					close: `))${chars.STAR})`
				},
				"?": {
					type: "qmark",
					open: "(?:",
					close: ")?"
				},
				"+": {
					type: "plus",
					open: "(?:",
					close: ")+"
				},
				"*": {
					type: "star",
					open: "(?:",
					close: ")*"
				},
				"@": {
					type: "at",
					open: "(?:",
					close: ")"
				}
			};
		},
		/**
		* Create GLOB_CHARS
		*/
		globChars(win32) {
			return win32 === true ? WINDOWS_CHARS : POSIX_CHARS;
		}
	};
}));

//#endregion
//#region node_modules/picomatch/lib/utils.js
var require_utils = /* @__PURE__ */ __commonJSMin(((exports) => {
	const { REGEX_BACKSLASH, REGEX_REMOVE_BACKSLASH, REGEX_SPECIAL_CHARS, REGEX_SPECIAL_CHARS_GLOBAL } = require_constants();
	exports.isObject = (val) => val !== null && typeof val === "object" && !Array.isArray(val);
	exports.hasRegexChars = (str) => REGEX_SPECIAL_CHARS.test(str);
	exports.isRegexChar = (str) => str.length === 1 && exports.hasRegexChars(str);
	exports.escapeRegex = (str) => str.replace(REGEX_SPECIAL_CHARS_GLOBAL, "\\$1");
	exports.toPosixSlashes = (str) => str.replace(REGEX_BACKSLASH, "/");
	exports.isWindows = () => {
		if (typeof navigator !== "undefined" && navigator.platform) {
			const platform = navigator.platform.toLowerCase();
			return platform === "win32" || platform === "windows";
		}
		if (typeof process !== "undefined" && process.platform) return process.platform === "win32";
		return false;
	};
	exports.removeBackslashes = (str) => {
		return str.replace(REGEX_REMOVE_BACKSLASH, (match) => {
			return match === "\\" ? "" : match;
		});
	};
	exports.escapeLast = (input, char, lastIdx) => {
		const idx = input.lastIndexOf(char, lastIdx);
		if (idx === -1) return input;
		if (input[idx - 1] === "\\") return exports.escapeLast(input, char, idx - 1);
		return `${input.slice(0, idx)}\\${input.slice(idx)}`;
	};
	exports.removePrefix = (input, state = {}) => {
		let output = input;
		if (output.startsWith("./")) {
			output = output.slice(2);
			state.prefix = "./";
		}
		return output;
	};
	exports.wrapOutput = (input, state = {}, options = {}) => {
		let output = `${options.contains ? "" : "^"}(?:${input})${options.contains ? "" : "$"}`;
		if (state.negated === true) output = `(?:^(?!${output}).*$)`;
		return output;
	};
	exports.basename = (path, { windows } = {}) => {
		const segs = path.split(windows ? /[\\/]/ : "/");
		const last = segs[segs.length - 1];
		if (last === "") return segs[segs.length - 2];
		return last;
	};
}));

//#endregion
//#region node_modules/picomatch/lib/scan.js
var require_scan = /* @__PURE__ */ __commonJSMin(((exports, module) => {
	const utils = require_utils();
	const { CHAR_ASTERISK, CHAR_AT, CHAR_BACKWARD_SLASH, CHAR_COMMA, CHAR_DOT, CHAR_EXCLAMATION_MARK, CHAR_FORWARD_SLASH, CHAR_LEFT_CURLY_BRACE, CHAR_LEFT_PARENTHESES, CHAR_LEFT_SQUARE_BRACKET, CHAR_PLUS, CHAR_QUESTION_MARK, CHAR_RIGHT_CURLY_BRACE, CHAR_RIGHT_PARENTHESES, CHAR_RIGHT_SQUARE_BRACKET } = require_constants();
	const isPathSeparator = (code) => {
		return code === CHAR_FORWARD_SLASH || code === CHAR_BACKWARD_SLASH;
	};
	const depth = (token) => {
		if (token.isPrefix !== true) token.depth = token.isGlobstar ? Infinity : 1;
	};
	/**
	* Quickly scans a glob pattern and returns an object with a handful of
	* useful properties, like `isGlob`, `path` (the leading non-glob, if it exists),
	* `glob` (the actual pattern), `negated` (true if the path starts with `!` but not
	* with `!(`) and `negatedExtglob` (true if the path starts with `!(`).
	*
	* ```js
	* const pm = require('picomatch');
	* console.log(pm.scan('foo/bar/*.js'));
	* { isGlob: true, input: 'foo/bar/*.js', base: 'foo/bar', glob: '*.js' }
	* ```
	* @param {String} `str`
	* @param {Object} `options`
	* @return {Object} Returns an object with tokens and regex source string.
	* @api public
	*/
	const scan = (input, options) => {
		const opts = options || {};
		const length = input.length - 1;
		const scanToEnd = opts.parts === true || opts.scanToEnd === true;
		const slashes = [];
		const tokens = [];
		const parts = [];
		let str = input;
		let index = -1;
		let start = 0;
		let lastIndex = 0;
		let isBrace = false;
		let isBracket = false;
		let isGlob = false;
		let isExtglob = false;
		let isGlobstar = false;
		let braceEscaped = false;
		let backslashes = false;
		let negated = false;
		let negatedExtglob = false;
		let finished = false;
		let braces = 0;
		let prev;
		let code;
		let token = {
			value: "",
			depth: 0,
			isGlob: false
		};
		const eos = () => index >= length;
		const peek = () => str.charCodeAt(index + 1);
		const advance = () => {
			prev = code;
			return str.charCodeAt(++index);
		};
		while (index < length) {
			code = advance();
			let next;
			if (code === CHAR_BACKWARD_SLASH) {
				backslashes = token.backslashes = true;
				code = advance();
				if (code === CHAR_LEFT_CURLY_BRACE) braceEscaped = true;
				continue;
			}
			if (braceEscaped === true || code === CHAR_LEFT_CURLY_BRACE) {
				braces++;
				while (eos() !== true && (code = advance())) {
					if (code === CHAR_BACKWARD_SLASH) {
						backslashes = token.backslashes = true;
						advance();
						continue;
					}
					if (code === CHAR_LEFT_CURLY_BRACE) {
						braces++;
						continue;
					}
					if (braceEscaped !== true && code === CHAR_DOT && (code = advance()) === CHAR_DOT) {
						isBrace = token.isBrace = true;
						isGlob = token.isGlob = true;
						finished = true;
						if (scanToEnd === true) continue;
						break;
					}
					if (braceEscaped !== true && code === CHAR_COMMA) {
						isBrace = token.isBrace = true;
						isGlob = token.isGlob = true;
						finished = true;
						if (scanToEnd === true) continue;
						break;
					}
					if (code === CHAR_RIGHT_CURLY_BRACE) {
						braces--;
						if (braces === 0) {
							braceEscaped = false;
							isBrace = token.isBrace = true;
							finished = true;
							break;
						}
					}
				}
				if (scanToEnd === true) continue;
				break;
			}
			if (code === CHAR_FORWARD_SLASH) {
				slashes.push(index);
				tokens.push(token);
				token = {
					value: "",
					depth: 0,
					isGlob: false
				};
				if (finished === true) continue;
				if (prev === CHAR_DOT && index === start + 1) {
					start += 2;
					continue;
				}
				lastIndex = index + 1;
				continue;
			}
			if (opts.noext !== true) {
				if ((code === CHAR_PLUS || code === CHAR_AT || code === CHAR_ASTERISK || code === CHAR_QUESTION_MARK || code === CHAR_EXCLAMATION_MARK) === true && peek() === CHAR_LEFT_PARENTHESES) {
					isGlob = token.isGlob = true;
					isExtglob = token.isExtglob = true;
					finished = true;
					if (code === CHAR_EXCLAMATION_MARK && index === start) negatedExtglob = true;
					if (scanToEnd === true) {
						while (eos() !== true && (code = advance())) {
							if (code === CHAR_BACKWARD_SLASH) {
								backslashes = token.backslashes = true;
								code = advance();
								continue;
							}
							if (code === CHAR_RIGHT_PARENTHESES) {
								isGlob = token.isGlob = true;
								finished = true;
								break;
							}
						}
						continue;
					}
					break;
				}
			}
			if (code === CHAR_ASTERISK) {
				if (prev === CHAR_ASTERISK) isGlobstar = token.isGlobstar = true;
				isGlob = token.isGlob = true;
				finished = true;
				if (scanToEnd === true) continue;
				break;
			}
			if (code === CHAR_QUESTION_MARK) {
				isGlob = token.isGlob = true;
				finished = true;
				if (scanToEnd === true) continue;
				break;
			}
			if (code === CHAR_LEFT_SQUARE_BRACKET) {
				while (eos() !== true && (next = advance())) {
					if (next === CHAR_BACKWARD_SLASH) {
						backslashes = token.backslashes = true;
						advance();
						continue;
					}
					if (next === CHAR_RIGHT_SQUARE_BRACKET) {
						isBracket = token.isBracket = true;
						isGlob = token.isGlob = true;
						finished = true;
						break;
					}
				}
				if (scanToEnd === true) continue;
				break;
			}
			if (opts.nonegate !== true && code === CHAR_EXCLAMATION_MARK && index === start) {
				negated = token.negated = true;
				start++;
				continue;
			}
			if (opts.noparen !== true && code === CHAR_LEFT_PARENTHESES) {
				isGlob = token.isGlob = true;
				if (scanToEnd === true) {
					while (eos() !== true && (code = advance())) {
						if (code === CHAR_LEFT_PARENTHESES) {
							backslashes = token.backslashes = true;
							code = advance();
							continue;
						}
						if (code === CHAR_RIGHT_PARENTHESES) {
							finished = true;
							break;
						}
					}
					continue;
				}
				break;
			}
			if (isGlob === true) {
				finished = true;
				if (scanToEnd === true) continue;
				break;
			}
		}
		if (opts.noext === true) {
			isExtglob = false;
			isGlob = false;
		}
		let base = str;
		let prefix = "";
		let glob = "";
		if (start > 0) {
			prefix = str.slice(0, start);
			str = str.slice(start);
			lastIndex -= start;
		}
		if (base && isGlob === true && lastIndex > 0) {
			base = str.slice(0, lastIndex);
			glob = str.slice(lastIndex);
		} else if (isGlob === true) {
			base = "";
			glob = str;
		} else base = str;
		if (base && base !== "" && base !== "/" && base !== str) {
			if (isPathSeparator(base.charCodeAt(base.length - 1))) base = base.slice(0, -1);
		}
		if (opts.unescape === true) {
			if (glob) glob = utils.removeBackslashes(glob);
			if (base && backslashes === true) base = utils.removeBackslashes(base);
		}
		const state = {
			prefix,
			input,
			start,
			base,
			glob,
			isBrace,
			isBracket,
			isGlob,
			isExtglob,
			isGlobstar,
			negated,
			negatedExtglob
		};
		if (opts.tokens === true) {
			state.maxDepth = 0;
			if (!isPathSeparator(code)) tokens.push(token);
			state.tokens = tokens;
		}
		if (opts.parts === true || opts.tokens === true) {
			let prevIndex;
			for (let idx = 0; idx < slashes.length; idx++) {
				const n = prevIndex ? prevIndex + 1 : start;
				const i = slashes[idx];
				const value = input.slice(n, i);
				if (opts.tokens) {
					if (idx === 0 && start !== 0) {
						tokens[idx].isPrefix = true;
						tokens[idx].value = prefix;
					} else tokens[idx].value = value;
					depth(tokens[idx]);
					state.maxDepth += tokens[idx].depth;
				}
				if (idx !== 0 || value !== "") parts.push(value);
				prevIndex = i;
			}
			if (prevIndex && prevIndex + 1 < input.length) {
				const value = input.slice(prevIndex + 1);
				parts.push(value);
				if (opts.tokens) {
					tokens[tokens.length - 1].value = value;
					depth(tokens[tokens.length - 1]);
					state.maxDepth += tokens[tokens.length - 1].depth;
				}
			}
			state.slashes = slashes;
			state.parts = parts;
		}
		return state;
	};
	module.exports = scan;
}));

//#endregion
//#region node_modules/picomatch/lib/parse.js
var require_parse = /* @__PURE__ */ __commonJSMin(((exports, module) => {
	const constants = require_constants();
	const utils = require_utils();
	/**
	* Constants
	*/
	const { MAX_LENGTH, POSIX_REGEX_SOURCE, REGEX_NON_SPECIAL_CHARS, REGEX_SPECIAL_CHARS_BACKREF, REPLACEMENTS } = constants;
	/**
	* Helpers
	*/
	const expandRange = (args, options) => {
		if (typeof options.expandRange === "function") return options.expandRange(...args, options);
		args.sort();
		const value = `[${args.join("-")}]`;
		try {
			new RegExp(value);
		} catch (ex) {
			return args.map((v) => utils.escapeRegex(v)).join("..");
		}
		return value;
	};
	/**
	* Create the message for a syntax error
	*/
	const syntaxError = (type, char) => {
		return `Missing ${type}: "${char}" - use "\\\\${char}" to match literal characters`;
	};
	const splitTopLevel = (input) => {
		const parts = [];
		let bracket = 0;
		let paren = 0;
		let quote = 0;
		let value = "";
		let escaped = false;
		for (const ch of input) {
			if (escaped === true) {
				value += ch;
				escaped = false;
				continue;
			}
			if (ch === "\\") {
				value += ch;
				escaped = true;
				continue;
			}
			if (ch === "\"") {
				quote = quote === 1 ? 0 : 1;
				value += ch;
				continue;
			}
			if (quote === 0) {
				if (ch === "[") bracket++;
				else if (ch === "]" && bracket > 0) bracket--;
				else if (bracket === 0) {
					if (ch === "(") paren++;
					else if (ch === ")" && paren > 0) paren--;
					else if (ch === "|" && paren === 0) {
						parts.push(value);
						value = "";
						continue;
					}
				}
			}
			value += ch;
		}
		parts.push(value);
		return parts;
	};
	const isPlainBranch = (branch) => {
		let escaped = false;
		for (const ch of branch) {
			if (escaped === true) {
				escaped = false;
				continue;
			}
			if (ch === "\\") {
				escaped = true;
				continue;
			}
			if (/[?*+@!()[\]{}]/.test(ch)) return false;
		}
		return true;
	};
	const normalizeSimpleBranch = (branch) => {
		let value = branch.trim();
		let changed = true;
		while (changed === true) {
			changed = false;
			if (/^@\([^\\()[\]{}|]+\)$/.test(value)) {
				value = value.slice(2, -1);
				changed = true;
			}
		}
		if (!isPlainBranch(value)) return;
		return value.replace(/\\(.)/g, "$1");
	};
	const hasRepeatedCharPrefixOverlap = (branches) => {
		const values = branches.map(normalizeSimpleBranch).filter(Boolean);
		for (let i = 0; i < values.length; i++) for (let j = i + 1; j < values.length; j++) {
			const a = values[i];
			const b = values[j];
			const char = a[0];
			if (!char || a !== char.repeat(a.length) || b !== char.repeat(b.length)) continue;
			if (a === b || a.startsWith(b) || b.startsWith(a)) return true;
		}
		return false;
	};
	const parseRepeatedExtglob = (pattern, requireEnd = true) => {
		if (pattern[0] !== "+" && pattern[0] !== "*" || pattern[1] !== "(") return;
		let bracket = 0;
		let paren = 0;
		let quote = 0;
		let escaped = false;
		for (let i = 1; i < pattern.length; i++) {
			const ch = pattern[i];
			if (escaped === true) {
				escaped = false;
				continue;
			}
			if (ch === "\\") {
				escaped = true;
				continue;
			}
			if (ch === "\"") {
				quote = quote === 1 ? 0 : 1;
				continue;
			}
			if (quote === 1) continue;
			if (ch === "[") {
				bracket++;
				continue;
			}
			if (ch === "]" && bracket > 0) {
				bracket--;
				continue;
			}
			if (bracket > 0) continue;
			if (ch === "(") {
				paren++;
				continue;
			}
			if (ch === ")") {
				paren--;
				if (paren === 0) {
					if (requireEnd === true && i !== pattern.length - 1) return;
					return {
						type: pattern[0],
						body: pattern.slice(2, i),
						end: i
					};
				}
			}
		}
	};
	const buildCharClassStar = (chars) => {
		return `${chars.length === 1 ? utils.escapeRegex(chars[0]) : `[${chars.map((ch) => utils.escapeRegex(ch)).join("")}]`}*`;
	};
	const getStarExtglobSequenceChars = (pattern) => {
		let index = 0;
		const chars = [];
		while (index < pattern.length) {
			const match = parseRepeatedExtglob(pattern.slice(index), false);
			if (!match || match.type !== "*") return;
			const branches = splitTopLevel(match.body).map((branch) => branch.trim());
			if (branches.length !== 1) return;
			const branch = normalizeSimpleBranch(branches[0]);
			if (!branch || branch.length !== 1) return;
			chars.push(branch);
			index += match.end + 1;
		}
		if (chars.length < 1) return;
		return chars;
	};
	const repeatedExtglobRecursion = (pattern) => {
		let depth = 0;
		let value = pattern.trim();
		let match = parseRepeatedExtglob(value);
		while (match) {
			depth++;
			value = match.body.trim();
			match = parseRepeatedExtglob(value);
		}
		return depth;
	};
	const analyzeRepeatedExtglob = (body, options) => {
		if (options.maxExtglobRecursion === false) return { risky: false };
		const max = typeof options.maxExtglobRecursion === "number" ? options.maxExtglobRecursion : constants.DEFAULT_MAX_EXTGLOB_RECURSION;
		const branches = splitTopLevel(body).map((branch) => branch.trim());
		if (branches.length > 1) {
			if (branches.some((branch) => branch === "") || branches.some((branch) => /^[*?]+$/.test(branch)) || hasRepeatedCharPrefixOverlap(branches)) return { risky: true };
		}
		const safeChars = [];
		let sawStarSequence = false;
		let combinable = true;
		for (const branch of branches) {
			const chars = getStarExtglobSequenceChars(branch);
			if (chars) {
				sawStarSequence = true;
				safeChars.push(...chars);
				continue;
			}
			const literal = normalizeSimpleBranch(branch);
			if (literal && literal.length === 1) {
				safeChars.push(literal);
				continue;
			}
			combinable = false;
			if (repeatedExtglobRecursion(branch) > max) return { risky: true };
		}
		if (sawStarSequence) return combinable ? {
			risky: true,
			safeOutput: buildCharClassStar([...new Set(safeChars)])
		} : { risky: true };
		return { risky: false };
	};
	/**
	* Parse the given input string.
	* @param {String} input
	* @param {Object} options
	* @return {Object}
	*/
	const parse = (input, options) => {
		if (typeof input !== "string") throw new TypeError("Expected a string");
		input = REPLACEMENTS[input] || input;
		const opts = { ...options };
		const max = typeof opts.maxLength === "number" ? Math.min(MAX_LENGTH, opts.maxLength) : MAX_LENGTH;
		let len = input.length;
		if (len > max) throw new SyntaxError(`Input length: ${len}, exceeds maximum allowed length: ${max}`);
		const bos = {
			type: "bos",
			value: "",
			output: opts.prepend || ""
		};
		const tokens = [bos];
		const capture = opts.capture ? "" : "?:";
		const PLATFORM_CHARS = constants.globChars(opts.windows);
		const EXTGLOB_CHARS = constants.extglobChars(PLATFORM_CHARS);
		const { DOT_LITERAL, PLUS_LITERAL, SLASH_LITERAL, ONE_CHAR, DOTS_SLASH, NO_DOT, NO_DOT_SLASH, NO_DOTS_SLASH, QMARK, QMARK_NO_DOT, STAR, START_ANCHOR } = PLATFORM_CHARS;
		const globstar = (opts) => {
			return `(${capture}(?:(?!${START_ANCHOR}${opts.dot ? DOTS_SLASH : DOT_LITERAL}).)*?)`;
		};
		const nodot = opts.dot ? "" : NO_DOT;
		const qmarkNoDot = opts.dot ? QMARK : QMARK_NO_DOT;
		let star = opts.bash === true ? globstar(opts) : STAR;
		if (opts.capture) star = `(${star})`;
		if (typeof opts.noext === "boolean") opts.noextglob = opts.noext;
		const state = {
			input,
			index: -1,
			start: 0,
			dot: opts.dot === true,
			consumed: "",
			output: "",
			prefix: "",
			backtrack: false,
			negated: false,
			brackets: 0,
			braces: 0,
			parens: 0,
			quotes: 0,
			globstar: false,
			tokens
		};
		input = utils.removePrefix(input, state);
		len = input.length;
		const extglobs = [];
		const braces = [];
		const stack = [];
		let prev = bos;
		let value;
		/**
		* Tokenizing helpers
		*/
		const eos = () => state.index === len - 1;
		const peek = state.peek = (n = 1) => input[state.index + n];
		const advance = state.advance = () => input[++state.index] || "";
		const remaining = () => input.slice(state.index + 1);
		const consume = (value = "", num = 0) => {
			state.consumed += value;
			state.index += num;
		};
		const append = (token) => {
			state.output += token.output != null ? token.output : token.value;
			consume(token.value);
		};
		const negate = () => {
			let count = 1;
			while (peek() === "!" && (peek(2) !== "(" || peek(3) === "?")) {
				advance();
				state.start++;
				count++;
			}
			if (count % 2 === 0) return false;
			state.negated = true;
			state.start++;
			return true;
		};
		const increment = (type) => {
			state[type]++;
			stack.push(type);
		};
		const decrement = (type) => {
			state[type]--;
			stack.pop();
		};
		/**
		* Push tokens onto the tokens array. This helper speeds up
		* tokenizing by 1) helping us avoid backtracking as much as possible,
		* and 2) helping us avoid creating extra tokens when consecutive
		* characters are plain text. This improves performance and simplifies
		* lookbehinds.
		*/
		const push = (tok) => {
			if (prev.type === "globstar") {
				const isBrace = state.braces > 0 && (tok.type === "comma" || tok.type === "brace");
				const isExtglob = tok.extglob === true || extglobs.length && (tok.type === "pipe" || tok.type === "paren");
				if (tok.type !== "slash" && tok.type !== "paren" && !isBrace && !isExtglob) {
					state.output = state.output.slice(0, -prev.output.length);
					prev.type = "star";
					prev.value = "*";
					prev.output = star;
					state.output += prev.output;
				}
			}
			if (extglobs.length && tok.type !== "paren") extglobs[extglobs.length - 1].inner += tok.value;
			if (tok.value || tok.output) append(tok);
			if (prev && prev.type === "text" && tok.type === "text") {
				prev.output = (prev.output || prev.value) + tok.value;
				prev.value += tok.value;
				return;
			}
			tok.prev = prev;
			tokens.push(tok);
			prev = tok;
		};
		const extglobOpen = (type, value) => {
			const token = {
				...EXTGLOB_CHARS[value],
				conditions: 1,
				inner: ""
			};
			token.prev = prev;
			token.parens = state.parens;
			token.output = state.output;
			token.startIndex = state.index;
			token.tokensIndex = tokens.length;
			const output = (opts.capture ? "(" : "") + token.open;
			increment("parens");
			push({
				type,
				value,
				output: state.output ? "" : ONE_CHAR
			});
			push({
				type: "paren",
				extglob: true,
				value: advance(),
				output
			});
			extglobs.push(token);
		};
		const extglobClose = (token) => {
			const literal = input.slice(token.startIndex, state.index + 1);
			const body = input.slice(token.startIndex + 2, state.index);
			const analysis = analyzeRepeatedExtglob(body, opts);
			if ((token.type === "plus" || token.type === "star") && analysis.risky) {
				const safeOutput = analysis.safeOutput ? (token.output ? "" : ONE_CHAR) + (opts.capture ? `(${analysis.safeOutput})` : analysis.safeOutput) : void 0;
				const open = tokens[token.tokensIndex];
				open.type = "text";
				open.value = literal;
				open.output = safeOutput || utils.escapeRegex(literal);
				for (let i = token.tokensIndex + 1; i < tokens.length; i++) {
					tokens[i].value = "";
					tokens[i].output = "";
					delete tokens[i].suffix;
				}
				state.output = token.output + open.output;
				state.backtrack = true;
				push({
					type: "paren",
					extglob: true,
					value,
					output: ""
				});
				decrement("parens");
				return;
			}
			let output = token.close + (opts.capture ? ")" : "");
			let rest;
			if (token.type === "negate") {
				let extglobStar = star;
				if (token.inner && token.inner.length > 1 && token.inner.includes("/")) extglobStar = globstar(opts);
				if (extglobStar !== star || eos() || /^\)+$/.test(remaining())) output = token.close = `)$))${extglobStar}`;
				if (token.inner.includes("*") && (rest = remaining()) && /^\.[^\\/.]+$/.test(rest)) output = token.close = `)${parse(rest, {
					...options,
					fastpaths: false
				}).output})${extglobStar})`;
				if (token.prev.type === "bos") state.negatedExtglob = true;
			}
			push({
				type: "paren",
				extglob: true,
				value,
				output
			});
			decrement("parens");
		};
		/**
		* Fast paths
		*/
		if (opts.fastpaths !== false && !/(^[*!]|[/()[\]{}"])/.test(input)) {
			let backslashes = false;
			let output = input.replace(REGEX_SPECIAL_CHARS_BACKREF, (m, esc, chars, first, rest, index) => {
				if (first === "\\") {
					backslashes = true;
					return m;
				}
				if (first === "?") {
					if (esc) return esc + first + (rest ? QMARK.repeat(rest.length) : "");
					if (index === 0) return qmarkNoDot + (rest ? QMARK.repeat(rest.length) : "");
					return QMARK.repeat(chars.length);
				}
				if (first === ".") return DOT_LITERAL.repeat(chars.length);
				if (first === "*") {
					if (esc) return esc + first + (rest ? star : "");
					return star;
				}
				return esc ? m : `\\${m}`;
			});
			if (backslashes === true) {
				if (opts.unescape === true) output = output.replace(/\\/g, "");
				else output = output.replace(/\\+/g, (m) => {
					return m.length % 2 === 0 ? "\\\\" : m ? "\\" : "";
				});
			}
			if (output === input && opts.contains === true) {
				state.output = input;
				return state;
			}
			state.output = utils.wrapOutput(output, state, options);
			return state;
		}
		/**
		* Tokenize input until we reach end-of-string
		*/
		while (!eos()) {
			value = advance();
			if (value === "\0") continue;
			/**
			* Escaped characters
			*/
			if (value === "\\") {
				const next = peek();
				if (next === "/" && opts.bash !== true) continue;
				if (next === "." || next === ";") continue;
				if (!next) {
					value += "\\";
					push({
						type: "text",
						value
					});
					continue;
				}
				const match = /^\\+/.exec(remaining());
				let slashes = 0;
				if (match && match[0].length > 2) {
					slashes = match[0].length;
					state.index += slashes;
					if (slashes % 2 !== 0) value += "\\";
				}
				if (opts.unescape === true) value = advance();
				else value += advance();
				if (state.brackets === 0) {
					push({
						type: "text",
						value
					});
					continue;
				}
			}
			/**
			* If we're inside a regex character class, continue
			* until we reach the closing bracket.
			*/
			if (state.brackets > 0 && (value !== "]" || prev.value === "[" || prev.value === "[^")) {
				if (opts.posix !== false && value === ":") {
					const inner = prev.value.slice(1);
					if (inner.includes("[")) {
						prev.posix = true;
						if (inner.includes(":")) {
							const idx = prev.value.lastIndexOf("[");
							const pre = prev.value.slice(0, idx);
							const rest = prev.value.slice(idx + 2);
							const posix = POSIX_REGEX_SOURCE[rest];
							if (posix) {
								prev.value = pre + posix;
								state.backtrack = true;
								advance();
								if (!bos.output && tokens.indexOf(prev) === 1) bos.output = ONE_CHAR;
								continue;
							}
						}
					}
				}
				if (value === "[" && peek() !== ":" || value === "-" && peek() === "]") value = `\\${value}`;
				if (value === "]" && (prev.value === "[" || prev.value === "[^")) value = `\\${value}`;
				if (opts.posix === true && value === "!" && prev.value === "[") value = "^";
				prev.value += value;
				append({ value });
				continue;
			}
			/**
			* If we're inside a quoted string, continue
			* until we reach the closing double quote.
			*/
			if (state.quotes === 1 && value !== "\"") {
				value = utils.escapeRegex(value);
				prev.value += value;
				append({ value });
				continue;
			}
			/**
			* Double quotes
			*/
			if (value === "\"") {
				state.quotes = state.quotes === 1 ? 0 : 1;
				if (opts.keepQuotes === true) push({
					type: "text",
					value
				});
				continue;
			}
			/**
			* Parentheses
			*/
			if (value === "(") {
				increment("parens");
				push({
					type: "paren",
					value
				});
				continue;
			}
			if (value === ")") {
				if (state.parens === 0 && opts.strictBrackets === true) throw new SyntaxError(syntaxError("opening", "("));
				const extglob = extglobs[extglobs.length - 1];
				if (extglob && state.parens === extglob.parens + 1) {
					extglobClose(extglobs.pop());
					continue;
				}
				push({
					type: "paren",
					value,
					output: state.parens ? ")" : "\\)"
				});
				decrement("parens");
				continue;
			}
			/**
			* Square brackets
			*/
			if (value === "[") {
				if (opts.nobracket === true || !remaining().includes("]")) {
					if (opts.nobracket !== true && opts.strictBrackets === true) throw new SyntaxError(syntaxError("closing", "]"));
					value = `\\${value}`;
				} else increment("brackets");
				push({
					type: "bracket",
					value
				});
				continue;
			}
			if (value === "]") {
				if (opts.nobracket === true || prev && prev.type === "bracket" && prev.value.length === 1) {
					push({
						type: "text",
						value,
						output: `\\${value}`
					});
					continue;
				}
				if (state.brackets === 0) {
					if (opts.strictBrackets === true) throw new SyntaxError(syntaxError("opening", "["));
					push({
						type: "text",
						value,
						output: `\\${value}`
					});
					continue;
				}
				decrement("brackets");
				const prevValue = prev.value.slice(1);
				if (prev.posix !== true && prevValue[0] === "^" && !prevValue.includes("/")) value = `/${value}`;
				prev.value += value;
				append({ value });
				if (opts.literalBrackets === false || utils.hasRegexChars(prevValue)) continue;
				const escaped = utils.escapeRegex(prev.value);
				state.output = state.output.slice(0, -prev.value.length);
				if (opts.literalBrackets === true) {
					state.output += escaped;
					prev.value = escaped;
					continue;
				}
				prev.value = `(${capture}${escaped}|${prev.value})`;
				state.output += prev.value;
				continue;
			}
			/**
			* Braces
			*/
			if (value === "{" && opts.nobrace !== true) {
				increment("braces");
				const open = {
					type: "brace",
					value,
					output: "(",
					outputIndex: state.output.length,
					tokensIndex: state.tokens.length
				};
				braces.push(open);
				push(open);
				continue;
			}
			if (value === "}") {
				const brace = braces[braces.length - 1];
				if (opts.nobrace === true || !brace) {
					push({
						type: "text",
						value,
						output: value
					});
					continue;
				}
				let output = ")";
				if (brace.dots === true) {
					const arr = tokens.slice();
					const range = [];
					for (let i = arr.length - 1; i >= 0; i--) {
						tokens.pop();
						if (arr[i].type === "brace") break;
						if (arr[i].type !== "dots") range.unshift(arr[i].value);
					}
					output = expandRange(range, opts);
					state.backtrack = true;
				}
				if (brace.comma !== true && brace.dots !== true) {
					const out = state.output.slice(0, brace.outputIndex);
					const toks = state.tokens.slice(brace.tokensIndex);
					brace.value = brace.output = "\\{";
					value = output = "\\}";
					state.output = out;
					for (const t of toks) state.output += t.output || t.value;
				}
				push({
					type: "brace",
					value,
					output
				});
				decrement("braces");
				braces.pop();
				continue;
			}
			/**
			* Pipes
			*/
			if (value === "|") {
				if (extglobs.length > 0) extglobs[extglobs.length - 1].conditions++;
				push({
					type: "text",
					value
				});
				continue;
			}
			/**
			* Commas
			*/
			if (value === ",") {
				let output = value;
				const brace = braces[braces.length - 1];
				if (brace && stack[stack.length - 1] === "braces") {
					brace.comma = true;
					output = "|";
				}
				push({
					type: "comma",
					value,
					output
				});
				continue;
			}
			/**
			* Slashes
			*/
			if (value === "/") {
				if (prev.type === "dot" && state.index === state.start + 1) {
					state.start = state.index + 1;
					state.consumed = "";
					state.output = "";
					tokens.pop();
					prev = bos;
					continue;
				}
				push({
					type: "slash",
					value,
					output: SLASH_LITERAL
				});
				continue;
			}
			/**
			* Dots
			*/
			if (value === ".") {
				if (state.braces > 0 && prev.type === "dot") {
					if (prev.value === ".") prev.output = DOT_LITERAL;
					const brace = braces[braces.length - 1];
					prev.type = "dots";
					prev.output += value;
					prev.value += value;
					brace.dots = true;
					continue;
				}
				if (state.braces + state.parens === 0 && prev.type !== "bos" && prev.type !== "slash") {
					push({
						type: "text",
						value,
						output: DOT_LITERAL
					});
					continue;
				}
				push({
					type: "dot",
					value,
					output: DOT_LITERAL
				});
				continue;
			}
			/**
			* Question marks
			*/
			if (value === "?") {
				if (!(prev && prev.value === "(") && opts.noextglob !== true && peek() === "(" && peek(2) !== "?") {
					extglobOpen("qmark", value);
					continue;
				}
				if (prev && prev.type === "paren") {
					const next = peek();
					let output = value;
					if (prev.value === "(" && !/[!=<:]/.test(next) || next === "<" && !/<([!=]|\w+>)/.test(remaining())) output = `\\${value}`;
					push({
						type: "text",
						value,
						output
					});
					continue;
				}
				if (opts.dot !== true && (prev.type === "slash" || prev.type === "bos")) {
					push({
						type: "qmark",
						value,
						output: QMARK_NO_DOT
					});
					continue;
				}
				push({
					type: "qmark",
					value,
					output: QMARK
				});
				continue;
			}
			/**
			* Exclamation
			*/
			if (value === "!") {
				if (opts.noextglob !== true && peek() === "(") {
					if (peek(2) !== "?" || !/[!=<:]/.test(peek(3))) {
						extglobOpen("negate", value);
						continue;
					}
				}
				if (opts.nonegate !== true && state.index === 0) {
					negate();
					continue;
				}
			}
			/**
			* Plus
			*/
			if (value === "+") {
				if (opts.noextglob !== true && peek() === "(" && peek(2) !== "?") {
					extglobOpen("plus", value);
					continue;
				}
				if (prev && prev.value === "(" || opts.regex === false) {
					push({
						type: "plus",
						value,
						output: PLUS_LITERAL
					});
					continue;
				}
				if (prev && (prev.type === "bracket" || prev.type === "paren" || prev.type === "brace") || state.parens > 0) {
					push({
						type: "plus",
						value
					});
					continue;
				}
				push({
					type: "plus",
					value: PLUS_LITERAL
				});
				continue;
			}
			/**
			* Plain text
			*/
			if (value === "@") {
				if (opts.noextglob !== true && peek() === "(" && peek(2) !== "?") {
					push({
						type: "at",
						extglob: true,
						value,
						output: ""
					});
					continue;
				}
				push({
					type: "text",
					value
				});
				continue;
			}
			/**
			* Plain text
			*/
			if (value !== "*") {
				if (value === "$" || value === "^") value = `\\${value}`;
				const match = REGEX_NON_SPECIAL_CHARS.exec(remaining());
				if (match) {
					value += match[0];
					state.index += match[0].length;
				}
				push({
					type: "text",
					value
				});
				continue;
			}
			/**
			* Stars
			*/
			if (prev && (prev.type === "globstar" || prev.star === true)) {
				prev.type = "star";
				prev.star = true;
				prev.value += value;
				prev.output = star;
				state.backtrack = true;
				state.globstar = true;
				consume(value);
				continue;
			}
			let rest = remaining();
			if (opts.noextglob !== true && /^\([^?]/.test(rest)) {
				extglobOpen("star", value);
				continue;
			}
			if (prev.type === "star") {
				if (opts.noglobstar === true) {
					consume(value);
					continue;
				}
				const prior = prev.prev;
				const before = prior.prev;
				const isStart = prior.type === "slash" || prior.type === "bos";
				const afterStar = before && (before.type === "star" || before.type === "globstar");
				if (opts.bash === true && (!isStart || rest[0] && rest[0] !== "/")) {
					push({
						type: "star",
						value,
						output: ""
					});
					continue;
				}
				const isBrace = state.braces > 0 && (prior.type === "comma" || prior.type === "brace");
				const isExtglob = extglobs.length && (prior.type === "pipe" || prior.type === "paren");
				if (!isStart && prior.type !== "paren" && !isBrace && !isExtglob) {
					push({
						type: "star",
						value,
						output: ""
					});
					continue;
				}
				while (rest.slice(0, 3) === "/**") {
					const after = input[state.index + 4];
					if (after && after !== "/") break;
					rest = rest.slice(3);
					consume("/**", 3);
				}
				if (prior.type === "bos" && eos()) {
					prev.type = "globstar";
					prev.value += value;
					prev.output = globstar(opts);
					state.output = prev.output;
					state.globstar = true;
					consume(value);
					continue;
				}
				if (prior.type === "slash" && prior.prev.type !== "bos" && !afterStar && eos()) {
					state.output = state.output.slice(0, -(prior.output + prev.output).length);
					prior.output = `(?:${prior.output}`;
					prev.type = "globstar";
					prev.output = globstar(opts) + (opts.strictSlashes ? ")" : "|$)");
					prev.value += value;
					state.globstar = true;
					state.output += prior.output + prev.output;
					consume(value);
					continue;
				}
				if (prior.type === "slash" && prior.prev.type !== "bos" && rest[0] === "/") {
					const end = rest[1] !== void 0 ? "|$" : "";
					state.output = state.output.slice(0, -(prior.output + prev.output).length);
					prior.output = `(?:${prior.output}`;
					prev.type = "globstar";
					prev.output = `${globstar(opts)}${SLASH_LITERAL}|${SLASH_LITERAL}${end})`;
					prev.value += value;
					state.output += prior.output + prev.output;
					state.globstar = true;
					consume(value + advance());
					push({
						type: "slash",
						value: "/",
						output: ""
					});
					continue;
				}
				if (prior.type === "bos" && rest[0] === "/") {
					prev.type = "globstar";
					prev.value += value;
					prev.output = `(?:^|${SLASH_LITERAL}|${globstar(opts)}${SLASH_LITERAL})`;
					state.output = prev.output;
					state.globstar = true;
					consume(value + advance());
					push({
						type: "slash",
						value: "/",
						output: ""
					});
					continue;
				}
				state.output = state.output.slice(0, -prev.output.length);
				prev.type = "globstar";
				prev.output = globstar(opts);
				prev.value += value;
				state.output += prev.output;
				state.globstar = true;
				consume(value);
				continue;
			}
			const token = {
				type: "star",
				value,
				output: star
			};
			if (opts.bash === true) {
				token.output = ".*?";
				if (prev.type === "bos" || prev.type === "slash") token.output = nodot + token.output;
				push(token);
				continue;
			}
			if (prev && (prev.type === "bracket" || prev.type === "paren") && opts.regex === true) {
				token.output = value;
				push(token);
				continue;
			}
			if (state.index === state.start || prev.type === "slash" || prev.type === "dot") {
				if (prev.type === "dot") {
					state.output += NO_DOT_SLASH;
					prev.output += NO_DOT_SLASH;
				} else if (opts.dot === true) {
					state.output += NO_DOTS_SLASH;
					prev.output += NO_DOTS_SLASH;
				} else {
					state.output += nodot;
					prev.output += nodot;
				}
				if (peek() !== "*") {
					state.output += ONE_CHAR;
					prev.output += ONE_CHAR;
				}
			}
			push(token);
		}
		while (state.brackets > 0) {
			if (opts.strictBrackets === true) throw new SyntaxError(syntaxError("closing", "]"));
			state.output = utils.escapeLast(state.output, "[");
			decrement("brackets");
		}
		while (state.parens > 0) {
			if (opts.strictBrackets === true) throw new SyntaxError(syntaxError("closing", ")"));
			state.output = utils.escapeLast(state.output, "(");
			decrement("parens");
		}
		while (state.braces > 0) {
			if (opts.strictBrackets === true) throw new SyntaxError(syntaxError("closing", "}"));
			state.output = utils.escapeLast(state.output, "{");
			decrement("braces");
		}
		if (opts.strictSlashes !== true && (prev.type === "star" || prev.type === "bracket")) push({
			type: "maybe_slash",
			value: "",
			output: `${SLASH_LITERAL}?`
		});
		if (state.backtrack === true) {
			state.output = "";
			for (const token of state.tokens) {
				state.output += token.output != null ? token.output : token.value;
				if (token.suffix) state.output += token.suffix;
			}
		}
		return state;
	};
	/**
	* Fast paths for creating regular expressions for common glob patterns.
	* This can significantly speed up processing and has very little downside
	* impact when none of the fast paths match.
	*/
	parse.fastpaths = (input, options) => {
		const opts = { ...options };
		const max = typeof opts.maxLength === "number" ? Math.min(MAX_LENGTH, opts.maxLength) : MAX_LENGTH;
		const len = input.length;
		if (len > max) throw new SyntaxError(`Input length: ${len}, exceeds maximum allowed length: ${max}`);
		input = REPLACEMENTS[input] || input;
		const { DOT_LITERAL, SLASH_LITERAL, ONE_CHAR, DOTS_SLASH, NO_DOT, NO_DOTS, NO_DOTS_SLASH, STAR, START_ANCHOR } = constants.globChars(opts.windows);
		const nodot = opts.dot ? NO_DOTS : NO_DOT;
		const slashDot = opts.dot ? NO_DOTS_SLASH : NO_DOT;
		const capture = opts.capture ? "" : "?:";
		const state = {
			negated: false,
			prefix: ""
		};
		let star = opts.bash === true ? ".*?" : STAR;
		if (opts.capture) star = `(${star})`;
		const globstar = (opts) => {
			if (opts.noglobstar === true) return star;
			return `(${capture}(?:(?!${START_ANCHOR}${opts.dot ? DOTS_SLASH : DOT_LITERAL}).)*?)`;
		};
		const create = (str) => {
			switch (str) {
				case "*": return `${nodot}${ONE_CHAR}${star}`;
				case ".*": return `${DOT_LITERAL}${ONE_CHAR}${star}`;
				case "*.*": return `${nodot}${star}${DOT_LITERAL}${ONE_CHAR}${star}`;
				case "*/*": return `${nodot}${star}${SLASH_LITERAL}${ONE_CHAR}${slashDot}${star}`;
				case "**": return nodot + globstar(opts);
				case "**/*": return `(?:${nodot}${globstar(opts)}${SLASH_LITERAL})?${slashDot}${ONE_CHAR}${star}`;
				case "**/*.*": return `(?:${nodot}${globstar(opts)}${SLASH_LITERAL})?${slashDot}${star}${DOT_LITERAL}${ONE_CHAR}${star}`;
				case "**/.*": return `(?:${nodot}${globstar(opts)}${SLASH_LITERAL})?${DOT_LITERAL}${ONE_CHAR}${star}`;
				default: {
					const match = /^(.*?)\.(\w+)$/.exec(str);
					if (!match) return;
					const source = create(match[1]);
					if (!source) return;
					return source + DOT_LITERAL + match[2];
				}
			}
		};
		let source = create(utils.removePrefix(input, state));
		if (source && opts.strictSlashes !== true) source += `${SLASH_LITERAL}?`;
		return source;
	};
	module.exports = parse;
}));

//#endregion
//#region node_modules/picomatch/lib/picomatch.js
var require_picomatch$1 = /* @__PURE__ */ __commonJSMin(((exports, module) => {
	const scan = require_scan();
	const parse = require_parse();
	const utils = require_utils();
	const constants = require_constants();
	const isObject = (val) => val && typeof val === "object" && !Array.isArray(val);
	/**
	* Creates a matcher function from one or more glob patterns. The
	* returned function takes a string to match as its first argument,
	* and returns true if the string is a match. The returned matcher
	* function also takes a boolean as the second argument that, when true,
	* returns an object with additional information.
	*
	* ```js
	* const picomatch = require('picomatch');
	* // picomatch(glob[, options]);
	*
	* const isMatch = picomatch('*.!(*a)');
	* console.log(isMatch('a.a')); //=> false
	* console.log(isMatch('a.b')); //=> true
	*
	* // For environments without `node.js`, `picomatch/posix` provides you a dependency-free matcher, without automatic OS detection.
	* const picomatch = require('picomatch/posix');
	* // the same API, defaulting to posix paths
	* const isMatch = picomatch('a/*');
	* console.log(isMatch('a\\b')); //=> false
	* console.log(isMatch('a/b')); //=> true
	*
	* // you can still configure the matcher function to accept windows paths
	* const isMatch = picomatch('a/*', { options: windows });
	* console.log(isMatch('a\\b')); //=> true
	* console.log(isMatch('a/b')); //=> true
	* ```
	* @name picomatch
	* @param {String|Array} `globs` One or more glob patterns.
	* @param {Object=} `options`
	* @return {Function=} Returns a matcher function.
	* @api public
	*/
	const picomatch = (glob, options, returnState = false) => {
		if (Array.isArray(glob)) {
			const fns = glob.map((input) => picomatch(input, options, returnState));
			const arrayMatcher = (str) => {
				for (const isMatch of fns) {
					const state = isMatch(str);
					if (state) return state;
				}
				return false;
			};
			return arrayMatcher;
		}
		const isState = isObject(glob) && glob.tokens && glob.input;
		if (glob === "" || typeof glob !== "string" && !isState) throw new TypeError("Expected pattern to be a non-empty string");
		const opts = options || {};
		const posix = opts.windows;
		const regex = isState ? picomatch.compileRe(glob, options) : picomatch.makeRe(glob, options, false, true);
		const state = regex.state;
		delete regex.state;
		let isIgnored = () => false;
		if (opts.ignore) {
			const ignoreOpts = {
				...options,
				ignore: null,
				onMatch: null,
				onResult: null
			};
			isIgnored = picomatch(opts.ignore, ignoreOpts, returnState);
		}
		const matcher = (input, returnObject = false) => {
			const { isMatch, match, output } = picomatch.test(input, regex, options, {
				glob,
				posix
			});
			const result = {
				glob,
				state,
				regex,
				posix,
				input,
				output,
				match,
				isMatch
			};
			if (typeof opts.onResult === "function") opts.onResult(result);
			if (isMatch === false) {
				result.isMatch = false;
				return returnObject ? result : false;
			}
			if (isIgnored(input)) {
				if (typeof opts.onIgnore === "function") opts.onIgnore(result);
				result.isMatch = false;
				return returnObject ? result : false;
			}
			if (typeof opts.onMatch === "function") opts.onMatch(result);
			return returnObject ? result : true;
		};
		if (returnState) matcher.state = state;
		return matcher;
	};
	/**
	* Test `input` with the given `regex`. This is used by the main
	* `picomatch()` function to test the input string.
	*
	* ```js
	* const picomatch = require('picomatch');
	* // picomatch.test(input, regex[, options]);
	*
	* console.log(picomatch.test('foo/bar', /^(?:([^/]*?)\/([^/]*?))$/));
	* // { isMatch: true, match: [ 'foo/', 'foo', 'bar' ], output: 'foo/bar' }
	* ```
	* @param {String} `input` String to test.
	* @param {RegExp} `regex`
	* @return {Object} Returns an object with matching info.
	* @api public
	*/
	picomatch.test = (input, regex, options, { glob, posix } = {}) => {
		if (typeof input !== "string") throw new TypeError("Expected input to be a string");
		if (input === "") return {
			isMatch: false,
			output: ""
		};
		const opts = options || {};
		const format = opts.format || (posix ? utils.toPosixSlashes : null);
		let match = input === glob;
		let output = match && format ? format(input) : input;
		if (match === false) {
			output = format ? format(input) : input;
			match = output === glob;
		}
		if (match === false || opts.capture === true) {
			if (opts.matchBase === true || opts.basename === true) match = picomatch.matchBase(input, regex, options, posix);
			else match = regex.exec(output);
		}
		return {
			isMatch: Boolean(match),
			match,
			output
		};
	};
	/**
	* Match the basename of a filepath.
	*
	* ```js
	* const picomatch = require('picomatch');
	* // picomatch.matchBase(input, glob[, options]);
	* console.log(picomatch.matchBase('foo/bar.js', '*.js'); // true
	* ```
	* @param {String} `input` String to test.
	* @param {RegExp|String} `glob` Glob pattern or regex created by [.makeRe](#makeRe).
	* @return {Boolean}
	* @api public
	*/
	picomatch.matchBase = (input, glob, options, posix = options && options.windows) => {
		return (glob instanceof RegExp ? glob : picomatch.makeRe(glob, options)).test(utils.basename(input, { windows: posix }));
	};
	/**
	* Returns true if **any** of the given glob `patterns` match the specified `string`.
	*
	* ```js
	* const picomatch = require('picomatch');
	* // picomatch.isMatch(string, patterns[, options]);
	*
	* console.log(picomatch.isMatch('a.a', ['b.*', '*.a'])); //=> true
	* console.log(picomatch.isMatch('a.a', 'b.*')); //=> false
	* ```
	* @param {String|Array} str The string to test.
	* @param {String|Array} patterns One or more glob patterns to use for matching.
	* @param {Object} [options] See available [options](#options).
	* @return {Boolean} Returns true if any patterns match `str`
	* @api public
	*/
	picomatch.isMatch = (str, patterns, options) => picomatch(patterns, options)(str);
	/**
	* Parse a glob pattern to create the source string for a regular
	* expression.
	*
	* ```js
	* const picomatch = require('picomatch');
	* const result = picomatch.parse(pattern[, options]);
	* ```
	* @param {String} `pattern`
	* @param {Object} `options`
	* @return {Object} Returns an object with useful properties and output to be used as a regex source string.
	* @api public
	*/
	picomatch.parse = (pattern, options) => {
		if (Array.isArray(pattern)) return pattern.map((p) => picomatch.parse(p, options));
		return parse(pattern, {
			...options,
			fastpaths: false
		});
	};
	/**
	* Scan a glob pattern to separate the pattern into segments.
	*
	* ```js
	* const picomatch = require('picomatch');
	* // picomatch.scan(input[, options]);
	*
	* const result = picomatch.scan('!./foo/*.js');
	* console.log(result);
	* { prefix: '!./',
	*   input: '!./foo/*.js',
	*   start: 3,
	*   base: 'foo',
	*   glob: '*.js',
	*   isBrace: false,
	*   isBracket: false,
	*   isGlob: true,
	*   isExtglob: false,
	*   isGlobstar: false,
	*   negated: true }
	* ```
	* @param {String} `input` Glob pattern to scan.
	* @param {Object} `options`
	* @return {Object} Returns an object with
	* @api public
	*/
	picomatch.scan = (input, options) => scan(input, options);
	/**
	* Compile a regular expression from the `state` object returned by the
	* [parse()](#parse) method.
	*
	* ```js
	* const picomatch = require('picomatch');
	* const state = picomatch.parse('*.js');
	* // picomatch.compileRe(state[, options]);
	*
	* console.log(picomatch.compileRe(state));
	* //=> /^(?:(?!\.)(?=.)[^/]*?\.js)$/
	* ```
	* @param {Object} `state`
	* @param {Object} `options`
	* @param {Boolean} `returnOutput` Intended for implementors, this argument allows you to return the raw output from the parser.
	* @param {Boolean} `returnState` Adds the state to a `state` property on the returned regex. Useful for implementors and debugging.
	* @return {RegExp}
	* @api public
	*/
	picomatch.compileRe = (state, options, returnOutput = false, returnState = false) => {
		if (returnOutput === true) return state.output;
		const opts = options || {};
		const prepend = opts.contains ? "" : "^";
		const append = opts.contains ? "" : "$";
		let source = `${prepend}(?:${state.output})${append}`;
		if (state && state.negated === true) source = `^(?!${source}).*$`;
		const regex = picomatch.toRegex(source, options);
		if (returnState === true) regex.state = state;
		return regex;
	};
	/**
	* Create a regular expression from a parsed glob pattern.
	*
	* ```js
	* const picomatch = require('picomatch');
	* // picomatch.makeRe(state[, options]);
	*
	* const result = picomatch.makeRe('*.js');
	* console.log(result);
	* //=> /^(?:(?!\.)(?=.)[^/]*?\.js)$/
	* ```
	* @param {String} `state` The object returned from the `.parse` method.
	* @param {Object} `options`
	* @param {Boolean} `returnOutput` Implementors may use this argument to return the compiled output, instead of a regular expression. This is not exposed on the options to prevent end-users from mutating the result.
	* @param {Boolean} `returnState` Implementors may use this argument to return the state from the parsed glob with the returned regular expression.
	* @return {RegExp} Returns a regex created from the given pattern.
	* @api public
	*/
	picomatch.makeRe = (input, options = {}, returnOutput = false, returnState = false) => {
		if (!input || typeof input !== "string") throw new TypeError("Expected a non-empty string");
		let parsed = {
			negated: false,
			fastpaths: true
		};
		if (options.fastpaths !== false && (input[0] === "." || input[0] === "*")) parsed.output = parse.fastpaths(input, options);
		if (!parsed.output) parsed = parse(input, options);
		return picomatch.compileRe(parsed, options, returnOutput, returnState);
	};
	/**
	* Create a regular expression from the given regex source string.
	*
	* ```js
	* const picomatch = require('picomatch');
	* // picomatch.toRegex(source[, options]);
	*
	* const { output } = picomatch.parse('*.js');
	* console.log(picomatch.toRegex(output));
	* //=> /^(?:(?!\.)(?=.)[^/]*?\.js)$/
	* ```
	* @param {String} `source` Regular expression source string.
	* @param {Object} `options`
	* @return {RegExp}
	* @api public
	*/
	picomatch.toRegex = (source, options) => {
		try {
			const opts = options || {};
			return new RegExp(source, opts.flags || (opts.nocase ? "i" : ""));
		} catch (err) {
			if (options && options.debug === true) throw err;
			return /$^/;
		}
	};
	/**
	* Picomatch constants.
	* @return {Object}
	*/
	picomatch.constants = constants;
	/**
	* Expose "picomatch"
	*/
	module.exports = picomatch;
}));

//#endregion
//#region node_modules/picomatch/index.js
var require_picomatch = /* @__PURE__ */ __commonJSMin(((exports, module) => {
	const pico = require_picomatch$1();
	const utils = require_utils();
	function picomatch(glob, options, returnState = false) {
		if (options && (options.windows === null || options.windows === void 0)) options = {
			...options,
			windows: utils.isWindows()
		};
		return pico(glob, options, returnState);
	}
	Object.assign(picomatch, pico);
	module.exports = picomatch;
}));

//#endregion
//#region node_modules/fdir/dist/index.mjs
var import_picomatch = /* @__PURE__ */ __toESM(require_picomatch(), 1);
var __require = /* @__PURE__ */ createRequire(import.meta.url);
function cleanPath(path) {
	let normalized = normalize(path);
	if (normalized.length > 1 && normalized[normalized.length - 1] === sep) normalized = normalized.substring(0, normalized.length - 1);
	return normalized;
}
const SLASHES_REGEX = /[\\/]/g;
function convertSlashes(path, separator) {
	return path.replace(SLASHES_REGEX, separator);
}
const WINDOWS_ROOT_DIR_REGEX = /^[a-z]:[\\/]$/i;
function isRootDirectory(path) {
	return path === "/" || WINDOWS_ROOT_DIR_REGEX.test(path);
}
function normalizePath(path, options) {
	const { resolvePaths, normalizePath: normalizePath$1, pathSeparator } = options;
	const pathNeedsCleaning = process.platform === "win32" && path.includes("/") || path.startsWith(".");
	if (resolvePaths) path = resolve(path);
	if (normalizePath$1 || pathNeedsCleaning) path = cleanPath(path);
	if (path === ".") return "";
	return convertSlashes(path[path.length - 1] !== pathSeparator ? path + pathSeparator : path, pathSeparator);
}
function joinPathWithBasePath(filename, directoryPath) {
	return directoryPath + filename;
}
function joinPathWithRelativePath(root, options) {
	return function(filename, directoryPath) {
		if (directoryPath.startsWith(root)) return directoryPath.slice(root.length) + filename;
		else return convertSlashes(relative(root, directoryPath), options.pathSeparator) + options.pathSeparator + filename;
	};
}
function joinPath(filename) {
	return filename;
}
function joinDirectoryPath(filename, directoryPath, separator) {
	return directoryPath + filename + separator;
}
function build$7(root, options) {
	const { relativePaths, includeBasePath } = options;
	return relativePaths && root ? joinPathWithRelativePath(root, options) : includeBasePath ? joinPathWithBasePath : joinPath;
}
function pushDirectoryWithRelativePath(root) {
	return function(directoryPath, paths) {
		paths.push(directoryPath.substring(root.length) || ".");
	};
}
function pushDirectoryFilterWithRelativePath(root) {
	return function(directoryPath, paths, filters) {
		const relativePath = directoryPath.substring(root.length) || ".";
		if (filters.every((filter) => filter(relativePath, true))) paths.push(relativePath);
	};
}
const pushDirectory = (directoryPath, paths) => {
	paths.push(directoryPath || ".");
};
const pushDirectoryFilter = (directoryPath, paths, filters) => {
	const path = directoryPath || ".";
	if (filters.every((filter) => filter(path, true))) paths.push(path);
};
const empty$2 = () => {};
function build$6(root, options) {
	const { includeDirs, filters, relativePaths } = options;
	if (!includeDirs) return empty$2;
	if (relativePaths) return filters && filters.length ? pushDirectoryFilterWithRelativePath(root) : pushDirectoryWithRelativePath(root);
	return filters && filters.length ? pushDirectoryFilter : pushDirectory;
}
const pushFileFilterAndCount = (filename, _paths, counts, filters) => {
	if (filters.every((filter) => filter(filename, false))) counts.files++;
};
const pushFileFilter = (filename, paths, _counts, filters) => {
	if (filters.every((filter) => filter(filename, false))) paths.push(filename);
};
const pushFileCount = (_filename, _paths, counts, _filters) => {
	counts.files++;
};
const pushFile = (filename, paths) => {
	paths.push(filename);
};
const empty$1 = () => {};
function build$5(options) {
	const { excludeFiles, filters, onlyCounts } = options;
	if (excludeFiles) return empty$1;
	if (filters && filters.length) return onlyCounts ? pushFileFilterAndCount : pushFileFilter;
	else if (onlyCounts) return pushFileCount;
	else return pushFile;
}
const getArray = (paths) => {
	return paths;
};
const getArrayGroup = () => {
	return [""].slice(0, 0);
};
function build$4(options) {
	return options.group ? getArrayGroup : getArray;
}
const groupFiles = (groups, directory, files) => {
	groups.push({
		directory,
		files,
		dir: directory
	});
};
const empty = () => {};
function build$3(options) {
	return options.group ? groupFiles : empty;
}
const resolveSymlinksAsync = function(path, state, callback$1) {
	const { queue, fs, options: { suppressErrors } } = state;
	queue.enqueue();
	fs.realpath(path, (error, resolvedPath) => {
		if (error) return queue.dequeue(suppressErrors ? null : error, state);
		fs.stat(resolvedPath, (error$1, stat) => {
			if (error$1) return queue.dequeue(suppressErrors ? null : error$1, state);
			if (stat.isDirectory() && isRecursive(path, resolvedPath, state)) return queue.dequeue(null, state);
			callback$1(stat, resolvedPath);
			queue.dequeue(null, state);
		});
	});
};
const resolveSymlinks = function(path, state, callback$1) {
	const { queue, fs, options: { suppressErrors } } = state;
	queue.enqueue();
	try {
		const resolvedPath = fs.realpathSync(path);
		const stat = fs.statSync(resolvedPath);
		if (stat.isDirectory() && isRecursive(path, resolvedPath, state)) return;
		callback$1(stat, resolvedPath);
	} catch (e) {
		if (!suppressErrors) throw e;
	}
};
function build$2(options, isSynchronous) {
	if (!options.resolveSymlinks || options.excludeSymlinks) return null;
	return isSynchronous ? resolveSymlinks : resolveSymlinksAsync;
}
function isRecursive(path, resolved, state) {
	if (state.options.useRealPaths) return isRecursiveUsingRealPaths(resolved, state);
	let parent = dirname(path);
	let depth = 1;
	while (parent !== state.root && depth < 2) {
		const resolvedPath = state.symlinks.get(parent);
		if (!!resolvedPath && (resolvedPath === resolved || resolvedPath.startsWith(resolved) || resolved.startsWith(resolvedPath))) depth++;
		else parent = dirname(parent);
	}
	state.symlinks.set(path, resolved);
	return depth > 1;
}
function isRecursiveUsingRealPaths(resolved, state) {
	return state.visited.includes(resolved + state.options.pathSeparator);
}
const onlyCountsSync = (state) => {
	return state.counts;
};
const groupsSync = (state) => {
	return state.groups;
};
const defaultSync = (state) => {
	return state.paths;
};
const limitFilesSync = (state) => {
	return state.paths.slice(0, state.options.maxFiles);
};
const onlyCountsAsync = (state, error, callback$1) => {
	report(error, callback$1, state.counts, state.options.suppressErrors);
	return null;
};
const defaultAsync = (state, error, callback$1) => {
	report(error, callback$1, state.paths, state.options.suppressErrors);
	return null;
};
const limitFilesAsync = (state, error, callback$1) => {
	report(error, callback$1, state.paths.slice(0, state.options.maxFiles), state.options.suppressErrors);
	return null;
};
const groupsAsync = (state, error, callback$1) => {
	report(error, callback$1, state.groups, state.options.suppressErrors);
	return null;
};
function report(error, callback$1, output, suppressErrors) {
	if (error && !suppressErrors) callback$1(error, output);
	else callback$1(null, output);
}
function build$1(options, isSynchronous) {
	const { onlyCounts, group, maxFiles } = options;
	if (onlyCounts) return isSynchronous ? onlyCountsSync : onlyCountsAsync;
	else if (group) return isSynchronous ? groupsSync : groupsAsync;
	else if (maxFiles) return isSynchronous ? limitFilesSync : limitFilesAsync;
	else return isSynchronous ? defaultSync : defaultAsync;
}
const readdirOpts = { withFileTypes: true };
const walkAsync = (state, crawlPath, directoryPath, currentDepth, callback$1) => {
	state.queue.enqueue();
	if (currentDepth < 0) return state.queue.dequeue(null, state);
	const { fs } = state;
	state.visited.push(crawlPath);
	state.counts.directories++;
	fs.readdir(crawlPath || ".", readdirOpts, (error, entries = []) => {
		callback$1(entries, directoryPath, currentDepth);
		state.queue.dequeue(state.options.suppressErrors ? null : error, state);
	});
};
const walkSync = (state, crawlPath, directoryPath, currentDepth, callback$1) => {
	const { fs } = state;
	if (currentDepth < 0) return;
	state.visited.push(crawlPath);
	state.counts.directories++;
	let entries = [];
	try {
		entries = fs.readdirSync(crawlPath || ".", readdirOpts);
	} catch (e) {
		if (!state.options.suppressErrors) throw e;
	}
	callback$1(entries, directoryPath, currentDepth);
};
function build(isSynchronous) {
	return isSynchronous ? walkSync : walkAsync;
}
/**
* This is a custom stateless queue to track concurrent async fs calls.
* It increments a counter whenever a call is queued and decrements it
* as soon as it completes. When the counter hits 0, it calls onQueueEmpty.
*/
var Queue = class {
	count = 0;
	constructor(onQueueEmpty) {
		this.onQueueEmpty = onQueueEmpty;
	}
	enqueue() {
		this.count++;
		return this.count;
	}
	dequeue(error, output) {
		if (this.onQueueEmpty && (--this.count <= 0 || error)) {
			this.onQueueEmpty(error, output);
			if (error) {
				output.controller.abort();
				this.onQueueEmpty = void 0;
			}
		}
	}
};
var Counter = class {
	_files = 0;
	_directories = 0;
	set files(num) {
		this._files = num;
	}
	get files() {
		return this._files;
	}
	set directories(num) {
		this._directories = num;
	}
	get directories() {
		return this._directories;
	}
	/**
	* @deprecated use `directories` instead
	*/
	/* c8 ignore next 3 */
	get dirs() {
		return this._directories;
	}
};
/**
* AbortController is not supported on Node 14 so we use this until we can drop
* support for Node 14.
*/
var Aborter = class {
	aborted = false;
	abort() {
		this.aborted = true;
	}
};
var Walker = class {
	root;
	isSynchronous;
	state;
	joinPath;
	pushDirectory;
	pushFile;
	getArray;
	groupFiles;
	resolveSymlink;
	walkDirectory;
	callbackInvoker;
	constructor(root, options, callback$1) {
		this.isSynchronous = !callback$1;
		this.callbackInvoker = build$1(options, this.isSynchronous);
		this.root = normalizePath(root, options);
		this.state = {
			root: isRootDirectory(this.root) ? this.root : this.root.slice(0, -1),
			paths: [""].slice(0, 0),
			groups: [],
			counts: new Counter(),
			options,
			queue: new Queue((error, state) => this.callbackInvoker(state, error, callback$1)),
			symlinks: /* @__PURE__ */ new Map(),
			visited: [""].slice(0, 0),
			controller: new Aborter(),
			fs: options.fs || nativeFs
		};
		this.joinPath = build$7(this.root, options);
		this.pushDirectory = build$6(this.root, options);
		this.pushFile = build$5(options);
		this.getArray = build$4(options);
		this.groupFiles = build$3(options);
		this.resolveSymlink = build$2(options, this.isSynchronous);
		this.walkDirectory = build(this.isSynchronous);
	}
	start() {
		this.pushDirectory(this.root, this.state.paths, this.state.options.filters);
		this.walkDirectory(this.state, this.root, this.root, this.state.options.maxDepth, this.walk);
		return this.isSynchronous ? this.callbackInvoker(this.state, null) : null;
	}
	walk = (entries, directoryPath, depth) => {
		const { paths, options: { filters, resolveSymlinks: resolveSymlinks$1, excludeSymlinks, exclude, maxFiles, signal, useRealPaths, pathSeparator }, controller } = this.state;
		if (controller.aborted || signal && signal.aborted || maxFiles && paths.length > maxFiles) return;
		const files = this.getArray(this.state.paths);
		for (let i = 0; i < entries.length; ++i) {
			const entry = entries[i];
			if (entry.isFile() || entry.isSymbolicLink() && !resolveSymlinks$1 && !excludeSymlinks) {
				const filename = this.joinPath(entry.name, directoryPath);
				this.pushFile(filename, files, this.state.counts, filters);
			} else if (entry.isDirectory()) {
				let path = joinDirectoryPath(entry.name, directoryPath, this.state.options.pathSeparator);
				if (exclude && exclude(entry.name, path)) continue;
				this.pushDirectory(path, paths, filters);
				this.walkDirectory(this.state, path, path, depth - 1, this.walk);
			} else if (this.resolveSymlink && entry.isSymbolicLink()) {
				let path = joinPathWithBasePath(entry.name, directoryPath);
				this.resolveSymlink(path, this.state, (stat, resolvedPath) => {
					if (stat.isDirectory()) {
						resolvedPath = normalizePath(resolvedPath, this.state.options);
						if (exclude && exclude(entry.name, useRealPaths ? resolvedPath : path + pathSeparator)) return;
						this.walkDirectory(this.state, resolvedPath, useRealPaths ? resolvedPath : path + pathSeparator, depth - 1, this.walk);
					} else {
						resolvedPath = useRealPaths ? resolvedPath : path;
						const filename = basename(resolvedPath);
						const directoryPath$1 = normalizePath(dirname(resolvedPath), this.state.options);
						resolvedPath = this.joinPath(filename, directoryPath$1);
						this.pushFile(resolvedPath, files, this.state.counts, filters);
					}
				});
			}
		}
		this.groupFiles(this.state.groups, directoryPath, files);
	};
};
function promise(root, options) {
	return new Promise((resolve$1, reject) => {
		callback(root, options, (err, output) => {
			if (err) return reject(err);
			resolve$1(output);
		});
	});
}
function callback(root, options, callback$1) {
	new Walker(root, options, callback$1).start();
}
function sync(root, options) {
	return new Walker(root, options).start();
}
var APIBuilder = class {
	constructor(root, options) {
		this.root = root;
		this.options = options;
	}
	withPromise() {
		return promise(this.root, this.options);
	}
	withCallback(cb) {
		callback(this.root, this.options, cb);
	}
	sync() {
		return sync(this.root, this.options);
	}
};
let pm = null;
/* c8 ignore next 6 */
try {
	__require.resolve("picomatch");
	pm = __require("picomatch");
} catch {}
var Builder = class {
	globCache = {};
	options = {
		maxDepth: Infinity,
		suppressErrors: true,
		pathSeparator: sep,
		filters: []
	};
	globFunction;
	constructor(options) {
		this.options = {
			...this.options,
			...options
		};
		this.globFunction = this.options.globFunction;
	}
	group() {
		this.options.group = true;
		return this;
	}
	withPathSeparator(separator) {
		this.options.pathSeparator = separator;
		return this;
	}
	withBasePath() {
		this.options.includeBasePath = true;
		return this;
	}
	withRelativePaths() {
		this.options.relativePaths = true;
		return this;
	}
	withDirs() {
		this.options.includeDirs = true;
		return this;
	}
	withMaxDepth(depth) {
		this.options.maxDepth = depth;
		return this;
	}
	withMaxFiles(limit) {
		this.options.maxFiles = limit;
		return this;
	}
	withFullPaths() {
		this.options.resolvePaths = true;
		this.options.includeBasePath = true;
		return this;
	}
	withErrors() {
		this.options.suppressErrors = false;
		return this;
	}
	withSymlinks({ resolvePaths = true } = {}) {
		this.options.resolveSymlinks = true;
		this.options.useRealPaths = resolvePaths;
		return this.withFullPaths();
	}
	withAbortSignal(signal) {
		this.options.signal = signal;
		return this;
	}
	normalize() {
		this.options.normalizePath = true;
		return this;
	}
	filter(predicate) {
		this.options.filters.push(predicate);
		return this;
	}
	onlyDirs() {
		this.options.excludeFiles = true;
		this.options.includeDirs = true;
		return this;
	}
	exclude(predicate) {
		this.options.exclude = predicate;
		return this;
	}
	onlyCounts() {
		this.options.onlyCounts = true;
		return this;
	}
	crawl(root) {
		return new APIBuilder(root || ".", this.options);
	}
	withGlobFunction(fn) {
		this.globFunction = fn;
		return this;
	}
	/**
	* @deprecated Pass options using the constructor instead:
	* ```ts
	* new fdir(options).crawl("/path/to/root");
	* ```
	* This method will be removed in v7.0
	*/
	/* c8 ignore next 4 */
	crawlWithOptions(root, options) {
		this.options = {
			...this.options,
			...options
		};
		return new APIBuilder(root || ".", this.options);
	}
	glob(...patterns) {
		if (this.globFunction) return this.globWithOptions(patterns);
		return this.globWithOptions(patterns, ...[{ dot: true }]);
	}
	globWithOptions(patterns, ...options) {
		const globFn = this.globFunction || pm;
		/* c8 ignore next 5 */
		if (!globFn) throw new Error("Please specify a glob function to use glob matching.");
		var isMatch = this.globCache[patterns.join("\0")];
		if (!isMatch) {
			isMatch = globFn(patterns, ...options);
			this.globCache[patterns.join("\0")] = isMatch;
		}
		this.options.filters.push((path) => isMatch(path));
		return this;
	}
};

//#endregion
//#region node_modules/tinyglobby/dist/index.mjs
var dist_exports = /* @__PURE__ */ __exportAll({
	convertPathToPattern: () => convertPathToPattern,
	escapePath: () => escapePath,
	glob: () => glob,
	globSync: () => globSync,
	isDynamicPattern: () => isDynamicPattern
});
const isReadonlyArray = Array.isArray;
const BACKSLASHES = /\\/g;
const DRIVE_RELATIVE_PATH = /^[A-Za-z]:$/;
const isWin = process.platform === "win32";
const ONLY_PARENT_DIRECTORIES = /^(\/?\.\.)+$/;
function getPartialMatcher(patterns, options = {}) {
	const patternsCount = patterns.length;
	const patternsParts = Array(patternsCount);
	const matchers = Array(patternsCount);
	let i, j;
	for (i = 0; i < patternsCount; i++) {
		const parts = splitPattern(patterns[i]);
		patternsParts[i] = parts;
		const partsCount = parts.length;
		const partMatchers = Array(partsCount);
		for (j = 0; j < partsCount; j++) partMatchers[j] = (0, import_picomatch.default)(parts[j], options);
		matchers[i] = partMatchers;
	}
	return (input) => {
		const inputParts = input.split("/");
		if (inputParts[0] === ".." && ONLY_PARENT_DIRECTORIES.test(input)) return true;
		for (i = 0; i < patternsCount; i++) {
			const patternParts = patternsParts[i];
			const matcher = matchers[i];
			const inputPatternCount = inputParts.length;
			const minParts = Math.min(inputPatternCount, patternParts.length);
			j = 0;
			while (j < minParts) {
				const part = patternParts[j];
				if (part.includes("/")) return true;
				if (!matcher[j](inputParts[j])) break;
				if (!options.noglobstar && part === "**") return true;
				j++;
			}
			if (j === inputPatternCount) return true;
		}
		return false;
	};
}
/* node:coverage ignore next 2 */
const WIN32_ROOT_DIR = /^[A-Z]:\/$/i;
const isRoot = isWin ? (p) => WIN32_ROOT_DIR.test(p) : (p) => p === "/";
function buildFormat(cwd, root, absolute) {
	if (cwd === root || root.startsWith(`${cwd}/`)) {
		if (absolute) {
			const start = cwd.length + +!isRoot(cwd);
			return (p, isDir) => p.slice(start, isDir ? -1 : void 0) || ".";
		}
		const prefix = root.slice(cwd.length + 1);
		if (prefix) return (p, isDir) => {
			if (p === ".") return prefix;
			const result = `${prefix}/${p}`;
			return isDir ? result.slice(0, -1) : result;
		};
		return (p, isDir) => isDir && p !== "." ? p.slice(0, -1) : p;
	}
	if (absolute) return (p) => posix.relative(cwd, p) || ".";
	return (p) => posix.relative(cwd, `${root}/${p}`) || ".";
}
function buildRelative(cwd, root) {
	if (root.startsWith(`${cwd}/`)) {
		const prefix = root.slice(cwd.length + 1);
		return (p) => `${prefix}/${p}`;
	}
	return (p) => {
		const result = posix.relative(cwd, `${root}/${p}`);
		return p[p.length - 1] === "/" && result !== "" ? `${result}/` : result || ".";
	};
}
function ensureNonDriveRelativePath(path) {
	return path.replace(DRIVE_RELATIVE_PATH, (match) => `${match}/`);
}
const splitPatternOptions = { parts: true };
function splitPattern(path) {
	var _result$parts;
	const result = import_picomatch.default.scan(path, splitPatternOptions);
	return ((_result$parts = result.parts) === null || _result$parts === void 0 ? void 0 : _result$parts.length) ? result.parts : [path];
}
const ESCAPED_WIN32_BACKSLASHES = /\\(?![()[\]{}!+@])/g;
function convertPosixPathToPattern(path) {
	return escapePosixPath(path);
}
function convertWin32PathToPattern(path) {
	return escapeWin32Path(path).replace(ESCAPED_WIN32_BACKSLASHES, "/");
}
/**
* Converts a path to a pattern depending on the platform.
* Identical to {@link escapePath} on POSIX systems.
* @see {@link https://superchupu.dev/tinyglobby/documentation#convertPathToPattern}
*/
/* node:coverage ignore next 3 */
const convertPathToPattern = isWin ? convertWin32PathToPattern : convertPosixPathToPattern;
const POSIX_UNESCAPED_GLOB_SYMBOLS = /(?<!\\)([()[\]{}*?|]|^!|[!+@](?=\()|\\(?![()[\]{}!*+?@|]))/g;
const WIN32_UNESCAPED_GLOB_SYMBOLS = /(?<!\\)([()[\]{}]|^!|[!+@](?=\())/g;
const escapePosixPath = (path) => path.replace(POSIX_UNESCAPED_GLOB_SYMBOLS, "\\$&");
const escapeWin32Path = (path) => path.replace(WIN32_UNESCAPED_GLOB_SYMBOLS, "\\$&");
/**
* Escapes a path's special characters depending on the platform.
* @see {@link https://superchupu.dev/tinyglobby/documentation#escapePath}
*/
/* node:coverage ignore next */
const escapePath = isWin ? escapeWin32Path : escapePosixPath;
/**
* Checks if a pattern has dynamic parts.
*
* Has a few minor differences with [`fast-glob`](https://github.com/mrmlnc/fast-glob) for better accuracy:
*
* - Doesn't necessarily return `false` on patterns that include `\`.
* - Returns `true` if the pattern includes parentheses, regardless of them representing one single pattern or not.
* - Returns `true` for unfinished glob extensions i.e. `(h`, `+(h`.
* - Returns `true` for unfinished brace expansions as long as they include `,` or `..`.
*
* @see {@link https://superchupu.dev/tinyglobby/documentation#isDynamicPattern}
*/
function isDynamicPattern(pattern, options) {
	if ((options === null || options === void 0 ? void 0 : options.caseSensitiveMatch) === false) return true;
	const scan = import_picomatch.default.scan(pattern);
	return scan.isGlob || scan.negated;
}
function log(...tasks) {
	console.log(`[tinyglobby ${(/* @__PURE__ */ new Date()).toLocaleTimeString("es")}]`, ...tasks);
}
function ensureStringArray(value) {
	return typeof value === "string" ? [value] : value !== null && value !== void 0 ? value : [];
}
const PARENT_DIRECTORY = /^(\/?\.\.)+/;
const ESCAPING_BACKSLASHES = /\\(?=[()[\]{}!*+?@|])/g;
function normalizePattern(pattern, opts, props, isIgnore) {
	var _PARENT_DIRECTORY$exe;
	const cwd = opts.cwd;
	let result = pattern;
	if (pattern[pattern.length - 1] === "/") result = pattern.slice(0, -1);
	if (result[result.length - 1] !== "*" && opts.expandDirectories) result += "/**";
	const escapedCwd = escapePath(cwd);
	result = isAbsolute(result.replace(ESCAPING_BACKSLASHES, "")) ? posix.relative(escapedCwd, result) : posix.normalize(result);
	const parentDir = (_PARENT_DIRECTORY$exe = PARENT_DIRECTORY.exec(result)) === null || _PARENT_DIRECTORY$exe === void 0 ? void 0 : _PARENT_DIRECTORY$exe[0];
	const parts = splitPattern(result);
	if (parentDir) {
		const n = (parentDir.length + 1) / 3;
		let i = 0;
		const cwdParts = escapedCwd.split("/");
		while (i < n && parts[i + n] === cwdParts[cwdParts.length + i - n]) {
			result = result.slice(0, (n - i - 1) * 3) + result.slice((n - i) * 3 + parts[i + n].length + 1) || ".";
			i++;
		}
		const potentialRoot = posix.join(cwd, parentDir.slice(i * 3));
		if (potentialRoot[0] !== "." && props.root.length > potentialRoot.length) {
			props.root = ensureNonDriveRelativePath(potentialRoot);
			props.depthOffset = -n + i;
		}
	}
	if (!isIgnore && props.depthOffset >= 0) {
		var _props$commonPath;
		(_props$commonPath = props.commonPath) !== null && _props$commonPath !== void 0 || (props.commonPath = parts);
		const newCommonPath = [];
		const length = Math.min(props.commonPath.length, parts.length);
		for (let i = 0; i < length; i++) {
			const part = parts[i];
			if (part === "**" && !parts[i + 1]) {
				newCommonPath.pop();
				break;
			}
			if (i === parts.length - 1 || part !== props.commonPath[i] || isDynamicPattern(part)) break;
			newCommonPath.push(part);
		}
		props.depthOffset = newCommonPath.length;
		props.commonPath = newCommonPath;
		props.root = ensureNonDriveRelativePath(newCommonPath.length > 0 ? posix.join(cwd, ...newCommonPath) : cwd);
	}
	return result;
}
function processPatterns(options, patterns, props) {
	const matchPatterns = [];
	const ignorePatterns = [];
	for (const pattern of options.ignore) {
		if (!pattern) continue;
		if (pattern[0] !== "!" || pattern[1] === "(") ignorePatterns.push(normalizePattern(pattern, options, props, true));
	}
	for (const pattern of patterns) {
		if (!pattern) continue;
		if (pattern[0] !== "!" || pattern[1] === "(") matchPatterns.push(normalizePattern(pattern, options, props, false));
		else if (pattern[1] !== "!" || pattern[2] === "(") ignorePatterns.push(normalizePattern(pattern.slice(1), options, props, true));
	}
	return {
		match: matchPatterns,
		ignore: ignorePatterns
	};
}
function buildCrawler(options, patterns) {
	const cwd = options.cwd;
	const props = {
		root: cwd,
		depthOffset: 0
	};
	const processed = processPatterns(options, patterns, props);
	if (options.debug) log("internal processing patterns:", processed);
	const { absolute, caseSensitiveMatch, debug, dot, followSymbolicLinks, onlyDirectories } = options;
	const root = props.root.replace(BACKSLASHES, "");
	const matchOptions = {
		dot,
		nobrace: options.braceExpansion === false,
		nocase: !caseSensitiveMatch,
		noextglob: options.extglob === false,
		noglobstar: options.globstar === false,
		posix: true
	};
	const matcher = (0, import_picomatch.default)(processed.match, matchOptions);
	const ignore = (0, import_picomatch.default)(processed.ignore, matchOptions);
	const partialMatcher = getPartialMatcher(processed.match, matchOptions);
	const format = buildFormat(cwd, root, absolute);
	const excludeFormatter = absolute ? format : buildFormat(cwd, root, true);
	const excludePredicate = (_, p) => {
		const relativePath = excludeFormatter(p, true);
		return relativePath !== "." && !partialMatcher(relativePath) || ignore(relativePath);
	};
	let maxDepth;
	if (options.deep !== void 0) maxDepth = Math.round(options.deep - props.depthOffset);
	const crawler = new Builder({
		filters: [debug ? (p, isDirectory) => {
			const path = format(p, isDirectory);
			const matches = matcher(path) && !ignore(path);
			if (matches) log(`matched ${path}`);
			return matches;
		} : (p, isDirectory) => {
			const path = format(p, isDirectory);
			return matcher(path) && !ignore(path);
		}],
		exclude: debug ? (_, p) => {
			const skipped = excludePredicate(_, p);
			log(`${skipped ? "skipped" : "crawling"} ${p}`);
			return skipped;
		} : excludePredicate,
		fs: options.fs,
		pathSeparator: "/",
		relativePaths: !absolute,
		resolvePaths: absolute,
		includeBasePath: absolute,
		resolveSymlinks: followSymbolicLinks,
		excludeSymlinks: !followSymbolicLinks,
		excludeFiles: onlyDirectories,
		includeDirs: onlyDirectories || !options.onlyFiles,
		maxDepth,
		signal: options.signal
	}).crawl(root);
	if (options.debug) log("internal properties:", {
		...props,
		root
	});
	return [crawler, cwd !== root && !absolute && buildRelative(cwd, root)];
}
function formatPaths(paths, mapper) {
	if (mapper) for (let i = paths.length - 1; i >= 0; i--) paths[i] = mapper(paths[i]);
	return paths;
}
const defaultOptions = {
	caseSensitiveMatch: true,
	debug: !!process.env.TINYGLOBBY_DEBUG,
	expandDirectories: true,
	followSymbolicLinks: true,
	onlyFiles: true
};
function getOptions(options) {
	const opts = Object.assign({}, options);
	for (const key in defaultOptions) if (opts[key] === void 0) Object.assign(opts, { [key]: defaultOptions[key] });
	opts.cwd = (opts.cwd instanceof URL ? fileURLToPath(opts.cwd) : resolve(opts.cwd || process.cwd())).replace(BACKSLASHES, "/");
	opts.ignore = ensureStringArray(opts.ignore);
	opts.fs && (opts.fs = {
		readdir: opts.fs.readdir || readdir,
		readdirSync: opts.fs.readdirSync || readdirSync,
		realpath: opts.fs.realpath || realpath,
		realpathSync: opts.fs.realpathSync || realpathSync,
		stat: opts.fs.stat || stat,
		statSync: opts.fs.statSync || statSync
	});
	if (opts.debug) log("globbing with options:", opts);
	return opts;
}
function getCrawler(globInput, inputOptions = {}) {
	var _ref;
	if (globInput && (inputOptions === null || inputOptions === void 0 ? void 0 : inputOptions.patterns)) throw new Error("Cannot pass patterns as both an argument and an option");
	const isModern = isReadonlyArray(globInput) || typeof globInput === "string";
	const patterns = ensureStringArray((_ref = isModern ? globInput : globInput.patterns) !== null && _ref !== void 0 ? _ref : "**/*");
	const options = getOptions(isModern ? inputOptions : globInput);
	return patterns.length > 0 ? buildCrawler(options, patterns) : [];
}
async function glob(globInput, options) {
	const [crawler, relative] = getCrawler(globInput, options);
	return crawler ? formatPaths(await crawler.withPromise(), relative) : [];
}
function globSync(globInput, options) {
	const [crawler, relative] = getCrawler(globInput, options);
	return crawler ? formatPaths(crawler.sync(), relative) : [];
}

//#endregion
var picomatch = import_picomatch.default;
export { picomatch, dist_exports as tinyglobby };