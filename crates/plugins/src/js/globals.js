// 语言本身之外、插件常用的全局：TextEncoder / TextDecoder、atob / btoa、URL / URLSearchParams、
// structuredClone、crypto（getRandomValues、randomUUID、subtle.digest）、AbortController。
// 都是纯计算，不涉及任何能力；需要宿主帮忙的（随机数、摘要、URL 解析）走 native 上的函数。
// 另外在 native 上挂两个 base64 ↔ Uint8Array 的辅助函数，给 prelude.js 用。
(function (native) {
  "use strict";

  // ---- UTF-8 ----

  const encodeUtf8 = (text) => {
    const string = String(text);
    const out = [];
    for (let i = 0; i < string.length; i++) {
      let code = string.charCodeAt(i);
      if (code >= 0xd800 && code <= 0xdbff && i + 1 < string.length) {
        const next = string.charCodeAt(i + 1);
        if (next >= 0xdc00 && next <= 0xdfff) {
          code = 0x10000 + ((code - 0xd800) << 10) + (next - 0xdc00);
          i++;
        } else {
          code = 0xfffd;
        }
      } else if (code >= 0xd800 && code <= 0xdfff) {
        code = 0xfffd;
      }
      if (code < 0x80) out.push(code);
      else if (code < 0x800) out.push(0xc0 | (code >> 6), 0x80 | (code & 63));
      else if (code < 0x10000) out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
      else out.push(0xf0 | (code >> 18), 0x80 | ((code >> 12) & 63), 0x80 | ((code >> 6) & 63), 0x80 | (code & 63));
    }
    return new Uint8Array(out);
  };

  const decodeUtf8 = (bytes) => {
    let out = "";
    let chunk = [];
    const flush = () => {
      out += String.fromCharCode.apply(null, chunk);
      chunk = [];
    };
    for (let i = 0; i < bytes.length; ) {
      const byte = bytes[i];
      let code = 0xfffd;
      let size = 1;
      if (byte < 0x80) code = byte;
      else if (byte >= 0xc2 && byte < 0xe0 && i + 1 < bytes.length && (bytes[i + 1] & 0xc0) === 0x80) {
        code = ((byte & 31) << 6) | (bytes[i + 1] & 63);
        size = 2;
      } else if (byte >= 0xe0 && byte < 0xf0 && i + 2 < bytes.length && (bytes[i + 1] & 0xc0) === 0x80 && (bytes[i + 2] & 0xc0) === 0x80) {
        code = ((byte & 15) << 12) | ((bytes[i + 1] & 63) << 6) | (bytes[i + 2] & 63);
        size = 3;
        if (code < 0x800 || (code >= 0xd800 && code <= 0xdfff)) code = 0xfffd;
      } else if (byte >= 0xf0 && byte < 0xf5 && i + 3 < bytes.length && (bytes[i + 1] & 0xc0) === 0x80 && (bytes[i + 2] & 0xc0) === 0x80 && (bytes[i + 3] & 0xc0) === 0x80) {
        code = ((byte & 7) << 18) | ((bytes[i + 1] & 63) << 12) | ((bytes[i + 2] & 63) << 6) | (bytes[i + 3] & 63);
        size = 4;
        if (code < 0x10000 || code > 0x10ffff) code = 0xfffd;
      }
      if (code >= 0x10000) {
        code -= 0x10000;
        chunk.push(0xd800 + (code >> 10), 0xdc00 + (code & 1023));
      } else {
        chunk.push(code);
      }
      if (chunk.length > 8192) flush();
      i += size;
    }
    flush();
    return out;
  };

  const toBytes = (input) => {
    if (input instanceof Uint8Array) return input;
    if (input instanceof ArrayBuffer) return new Uint8Array(input);
    if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
    throw new TypeError("expected a Uint8Array, an ArrayBuffer or another typed array");
  };

  class TextEncoder {
    get encoding() {
      return "utf-8";
    }
    encode(text = "") {
      return encodeUtf8(text);
    }
  }

  class TextDecoder {
    constructor(label = "utf-8") {
      const name = String(label).toLowerCase();
      if (name !== "utf-8" && name !== "utf8") throw new RangeError(`TextDecoder only supports utf-8, not ${label}`);
    }
    get encoding() {
      return "utf-8";
    }
    decode(input) {
      if (input === undefined) return "";
      const bytes = toBytes(input);
      // 去掉 BOM。
      const start = bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf ? 3 : 0;
      return decodeUtf8(start ? bytes.subarray(start) : bytes);
    }
  }

  // ---- base64 ----

  const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const LOOKUP = new Map([...ALPHABET].map((char, index) => [char, index]));

  const bytesToBase64 = (input) => {
    const bytes = toBytes(input);
    let out = "";
    for (let i = 0; i < bytes.length; i += 3) {
      const [a, b, c] = [bytes[i], bytes[i + 1], bytes[i + 2]];
      out += ALPHABET[a >> 2] + ALPHABET[((a & 3) << 4) | ((b ?? 0) >> 4)];
      out += b === undefined ? "=" : ALPHABET[((b & 15) << 2) | ((c ?? 0) >> 6)];
      out += c === undefined ? "=" : ALPHABET[c & 63];
    }
    return out;
  };

  const base64ToBytes = (text) => {
    const clean = String(text).replace(/[\s=]/g, "");
    const out = new Uint8Array(Math.floor((clean.length * 3) / 4));
    let bits = 0;
    let value = 0;
    let index = 0;
    for (const char of clean) {
      const digit = LOOKUP.get(char);
      if (digit === undefined) throw new DOMException("The string to be decoded is not correctly encoded.", "InvalidCharacterError");
      value = (value << 6) | digit;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out[index++] = (value >> bits) & 255;
      }
    }
    return out.subarray(0, index);
  };

  class DOMException extends Error {
    constructor(message = "", name = "Error") {
      super(message);
      this.name = name;
    }
  }

  const btoa = (text) => {
    const string = String(text);
    const bytes = new Uint8Array(string.length);
    for (let i = 0; i < string.length; i++) {
      const code = string.charCodeAt(i);
      if (code > 255) throw new DOMException("btoa: the string contains characters outside of Latin1", "InvalidCharacterError");
      bytes[i] = code;
    }
    return bytesToBase64(bytes);
  };

  const atob = (text) => {
    const bytes = base64ToBytes(text);
    let out = "";
    for (let i = 0; i < bytes.length; i += 8192) out += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
    return out;
  };

  // ---- URL ----

  const encodeForm = (text) =>
    encodeURIComponent(text).replace(/%20/g, "+").replace(/[!'()~]/g, (char) => "%" + char.charCodeAt(0).toString(16).toUpperCase());
  const decodeForm = (text) => {
    try {
      return decodeURIComponent(text.replace(/\+/g, " "));
    } catch {
      return text;
    }
  };

  class URLSearchParams {
    #entries = [];
    #onChange = null;

    constructor(init = "") {
      if (typeof init === "string") {
        this.#parse(init);
      } else if (init instanceof URLSearchParams) {
        this.#entries = [...init];
      } else if (Array.isArray(init) || (init && typeof init[Symbol.iterator] === "function")) {
        for (const pair of init) {
          const [name, value] = [...pair];
          this.#entries.push([String(name), String(value)]);
        }
      } else if (init && typeof init === "object") {
        for (const [name, value] of Object.entries(init)) this.#entries.push([name, String(value)]);
      }
    }

    #parse(text) {
      this.#entries = [];
      const query = text.startsWith("?") ? text.slice(1) : text;
      for (const part of query.split("&")) {
        if (part === "") continue;
        const at = part.indexOf("=");
        const name = at < 0 ? part : part.slice(0, at);
        const value = at < 0 ? "" : part.slice(at + 1);
        this.#entries.push([decodeForm(name), decodeForm(value)]);
      }
    }

    // URL 用：整体替换（不触发回调）、登记改动时的回调。
    static _attach(params, text, onChange) {
      params.#parse(text);
      params.#onChange = onChange;
    }

    #changed() {
      if (this.#onChange) this.#onChange(this.toString());
    }

    append(name, value) {
      this.#entries.push([String(name), String(value)]);
      this.#changed();
    }
    delete(name, value) {
      this.#entries = this.#entries.filter(([key, item]) => key !== String(name) || (value !== undefined && item !== String(value)));
      this.#changed();
    }
    get(name) {
      const found = this.#entries.find(([key]) => key === String(name));
      return found ? found[1] : null;
    }
    getAll(name) {
      return this.#entries.filter(([key]) => key === String(name)).map(([, value]) => value);
    }
    has(name, value) {
      return this.#entries.some(([key, item]) => key === String(name) && (value === undefined || item === String(value)));
    }
    set(name, value) {
      const key = String(name);
      const index = this.#entries.findIndex(([item]) => item === key);
      if (index < 0) {
        this.#entries.push([key, String(value)]);
      } else {
        this.#entries[index] = [key, String(value)];
        this.#entries = this.#entries.filter(([item], at) => item !== key || at === index);
      }
      this.#changed();
    }
    sort() {
      this.#entries.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
      this.#changed();
    }
    forEach(callback, self) {
      for (const [name, value] of this.#entries) callback.call(self, value, name, this);
    }
    keys() {
      return this.#entries.map(([name]) => name)[Symbol.iterator]();
    }
    values() {
      return this.#entries.map(([, value]) => value)[Symbol.iterator]();
    }
    entries() {
      return this.#entries.map((pair) => [...pair])[Symbol.iterator]();
    }
    [Symbol.iterator]() {
      return this.entries();
    }
    get size() {
      return this.#entries.length;
    }
    toString() {
      return this.#entries.map(([name, value]) => `${encodeForm(name)}=${encodeForm(value)}`).join("&");
    }
  }

  class URL {
    #parts;
    #params = new URLSearchParams();

    constructor(input, base) {
      this.#set(JSON.parse(native.parseUrl(String(input), base === undefined ? undefined : String(base))));
    }

    static canParse(input, base) {
      try {
        new URL(input, base);
        return true;
      } catch {
        return false;
      }
    }

    #set(parts) {
      this.#parts = parts;
      URLSearchParams._attach(this.#params, parts.search, (query) => {
        this.#reparse({ search: query === "" ? "" : `?${query}` });
      });
    }

    // 改了某一部分：拼出新的 href 再解析一遍，规范化交给宿主的 URL 解析器。
    #reparse(change) {
      const parts = { ...this.#parts, ...change };
      const auth = parts.username ? `${parts.username}${parts.password ? `:${parts.password}` : ""}@` : "";
      const port = parts.port ? `:${parts.port}` : "";
      const search = parts.search && !parts.search.startsWith("?") ? `?${parts.search}` : parts.search;
      const hash = parts.hash && !parts.hash.startsWith("#") ? `#${parts.hash}` : parts.hash;
      const protocol = parts.protocol.endsWith(":") ? parts.protocol : `${parts.protocol}:`;
      const href = `${protocol}//${auth}${parts.hostname}${port}${parts.pathname}${search}${hash}`;
      this.#set(JSON.parse(native.parseUrl(href, undefined)));
    }

    get href() {
      return this.#parts.href;
    }
    set href(value) {
      this.#set(JSON.parse(native.parseUrl(String(value), undefined)));
    }
    get origin() {
      return this.#parts.origin;
    }
    get protocol() {
      return this.#parts.protocol;
    }
    set protocol(value) {
      this.#reparse({ protocol: String(value) });
    }
    get username() {
      return this.#parts.username;
    }
    set username(value) {
      this.#reparse({ username: encodeURIComponent(String(value)) });
    }
    get password() {
      return this.#parts.password;
    }
    set password(value) {
      this.#reparse({ password: encodeURIComponent(String(value)) });
    }
    get host() {
      return this.#parts.port ? `${this.#parts.hostname}:${this.#parts.port}` : this.#parts.hostname;
    }
    set host(value) {
      const [hostname, port = ""] = String(value).split(":");
      this.#reparse({ hostname, port });
    }
    get hostname() {
      return this.#parts.hostname;
    }
    set hostname(value) {
      this.#reparse({ hostname: String(value) });
    }
    get port() {
      return this.#parts.port;
    }
    set port(value) {
      this.#reparse({ port: String(value) });
    }
    get pathname() {
      return this.#parts.pathname;
    }
    set pathname(value) {
      const path = String(value);
      this.#reparse({ pathname: path.startsWith("/") ? path : `/${path}` });
    }
    get search() {
      return this.#parts.search;
    }
    set search(value) {
      this.#reparse({ search: String(value) });
    }
    get searchParams() {
      return this.#params;
    }
    get hash() {
      return this.#parts.hash;
    }
    set hash(value) {
      this.#reparse({ hash: String(value) });
    }
    toString() {
      return this.href;
    }
    toJSON() {
      return this.href;
    }
  }

  // ---- structuredClone ----

  const structuredClone = (value) => {
    const seen = new Map();
    const clone = (item) => {
      if (item === null || typeof item !== "object") {
        if (typeof item === "function" || typeof item === "symbol") {
          throw new DOMException(`${String(item)} could not be cloned.`, "DataCloneError");
        }
        return item;
      }
      if (seen.has(item)) return seen.get(item);
      let copy;
      if (item instanceof Date) copy = new Date(item.getTime());
      else if (item instanceof RegExp) copy = new RegExp(item.source, item.flags);
      else if (item instanceof ArrayBuffer) copy = item.slice(0);
      else if (ArrayBuffer.isView(item)) copy = new item.constructor(item);
      else if (item instanceof Map) {
        copy = new Map();
        seen.set(item, copy);
        for (const [key, entry] of item) copy.set(clone(key), clone(entry));
        return copy;
      } else if (item instanceof Set) {
        copy = new Set();
        seen.set(item, copy);
        for (const entry of item) copy.add(clone(entry));
        return copy;
      } else if (item instanceof Error) {
        copy = new item.constructor(item.message);
        if (item.stack) copy.stack = item.stack;
      } else if (Array.isArray(item)) {
        copy = [];
        seen.set(item, copy);
        for (let i = 0; i < item.length; i++) copy[i] = clone(item[i]);
        return copy;
      } else {
        copy = {};
        seen.set(item, copy);
        for (const key of Object.keys(item)) copy[key] = clone(item[key]);
        return copy;
      }
      seen.set(item, copy);
      return copy;
    };
    return clone(value);
  };

  // ---- crypto ----

  const getRandomValues = (array) => {
    if (!ArrayBuffer.isView(array) || array instanceof Float32Array || array instanceof Float64Array) {
      throw new TypeError("getRandomValues expects an integer typed array");
    }
    const bytes = base64ToBytes(native.randomBytes(array.byteLength));
    new Uint8Array(array.buffer, array.byteOffset, array.byteLength).set(bytes);
    return array;
  };

  const randomUUID = () => {
    const bytes = getRandomValues(new Uint8Array(16));
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  };

  const subtle = Object.freeze({
    async digest(algorithm, data) {
      const name = typeof algorithm === "string" ? algorithm : algorithm?.name;
      const bytes = typeof data === "string" ? encodeUtf8(data) : toBytes(data);
      return base64ToBytes(native.digest(String(name), bytesToBase64(bytes))).buffer;
    },
  });

  // ---- AbortController ----

  class AbortSignal {
    #aborted = false;
    #reason = undefined;
    #listeners = [];
    onabort = null;

    get aborted() {
      return this.#aborted;
    }
    get reason() {
      return this.#reason;
    }
    throwIfAborted() {
      if (this.#aborted) throw this.#reason;
    }
    addEventListener(type, listener) {
      if (type === "abort" && typeof listener === "function") this.#listeners.push(listener);
    }
    removeEventListener(type, listener) {
      if (type === "abort") this.#listeners = this.#listeners.filter((item) => item !== listener);
    }
    static _abort(signal, reason) {
      if (signal.#aborted) return;
      signal.#aborted = true;
      signal.#reason = reason === undefined ? new DOMException("This operation was aborted", "AbortError") : reason;
      const event = { type: "abort", target: signal };
      for (const listener of [...signal.#listeners]) listener.call(signal, event);
      if (typeof signal.onabort === "function") signal.onabort(event);
    }
    static abort(reason) {
      const signal = new AbortSignal();
      AbortSignal._abort(signal, reason);
      return signal;
    }
  }

  class AbortController {
    #signal = new AbortSignal();
    get signal() {
      return this.#signal;
    }
    abort(reason) {
      AbortSignal._abort(this.#signal, reason);
    }
  }

  const define = (name, value) =>
    Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: false });
  define("TextEncoder", TextEncoder);
  define("TextDecoder", TextDecoder);
  define("atob", atob);
  define("btoa", btoa);
  define("URL", URL);
  define("URLSearchParams", URLSearchParams);
  define("structuredClone", structuredClone);
  define("DOMException", DOMException);
  define("AbortController", AbortController);
  define("AbortSignal", AbortSignal);
  define("crypto", Object.freeze({ getRandomValues, randomUUID, subtle }));

  native.bytesToBase64 = bytesToBase64;
  native.base64ToBytes = base64ToBytes;
  native.decodeUtf8 = decodeUtf8;
})
