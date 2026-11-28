// 每个插件的 QuickJS 运行时启动时先执行这一段（在 globals.js 之后）。它拿到宿主的原生函数（native），
// 装好 console 和定时器，并返回宿主调用的入口：register（登记模块的导出）、call（执行一次工具调用）、
// prompt、project、render、act、dispatch（钩子、卡片、生命周期、后台任务……），以及事件循环用的
// tick（触发到期的定时器）、settle（慢操作完成）、reset（一次调用结束）。
// 原生函数不挂在 globalThis 上，插件代码只能通过第二个参数 host 用到它们；
// 能不能用由宿主按插件声明的能力检查，这里不做判断。
(function (native) {
  "use strict";

  const format = (args) =>
    args
      .map((arg) => {
        if (typeof arg === "string") return arg;
        if (arg instanceof Error) return arg.stack ? `${arg.message}\n${arg.stack}` : String(arg);
        try {
          const json = JSON.stringify(arg);
          return json === undefined ? String(arg) : json;
        } catch {
          return String(arg);
        }
      })
      .join(" ");
  const logger = (level) => (...args) => native.log(level, format(args));

  globalThis.console = Object.freeze({
    log: logger("info"),
    info: logger("info"),
    debug: logger("debug"),
    warn: logger("warn"),
    error: logger("error"),
  });

  // ---- 定时器：只在一次调用的生命周期内有效，调用结束时没触发的都清掉 ----

  const timers = new Map();
  let timerSeq = 0;
  const addTimer = (callback, delay, args, repeat) => {
    if (typeof callback !== "function") throw new TypeError("the timer callback must be a function");
    const wait = Math.max(0, Number(delay) || 0);
    const id = ++timerSeq;
    timers.set(id, { callback, args, due: Date.now() + wait, interval: repeat ? Math.max(1, wait) : null });
    return id;
  };
  const define = (name, value) =>
    Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: false });
  define("setTimeout", (callback, delay, ...args) => addTimer(callback, delay, args, false));
  define("setInterval", (callback, delay, ...args) => addTimer(callback, delay, args, true));
  define("clearTimeout", (id) => void timers.delete(id));
  define("clearInterval", (id) => void timers.delete(id));
  if (typeof globalThis.queueMicrotask !== "function") {
    define("queueMicrotask", (callback) => void Promise.resolve().then(callback));
  }

  // ---- 慢操作（fetch、exec、llm）：宿主返回编号，完成后经 settle 交回来 ----

  const pending = new Map();
  const operation = (id, signal) =>
    new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      if (signal) {
        if (signal.aborted) native.abort(id);
        else signal.addEventListener("abort", () => native.abort(id));
      }
    });
  const rethrowAbort = (signal) => (error) => {
    if (signal?.aborted) throw signal.reason;
    throw error;
  };

  // 流式 fetch：正文一块一块读（body 是异步迭代器），events() 按 SSE 解析。
  const streamingFetch = async (url, request, signal) => {
    const op = native.fetchStream(url, JSON.stringify(request));
    const head = JSON.parse(await operation(op, signal).catch(rethrowAbort(signal)));
    const stream = native.claimStream(op);
    let used = false;
    const nextChunk = async () => {
      const chunk = JSON.parse(await operation(native.readChunk(stream), signal).catch(rethrowAbort(signal)));
      return chunk === null ? null : native.base64ToBytes(chunk);
    };
    const take = () => {
      if (used) throw new TypeError("the response body has already been read");
      used = true;
    };
    const body = {
      [Symbol.asyncIterator]() {
        take();
        return { next: async () => { const chunk = await nextChunk(); return chunk === null ? { done: true, value: undefined } : { done: false, value: chunk }; } };
      },
    };
    const readAll = async () => {
      take();
      const parts = [];
      for (let chunk = await nextChunk(); chunk !== null; chunk = await nextChunk()) parts.push(chunk);
      const size = parts.reduce((total, part) => total + part.length, 0);
      const all = new Uint8Array(size);
      let offset = 0;
      for (const part of parts) {
        all.set(part, offset);
        offset += part.length;
      }
      return all;
    };
    // Server-Sent Events：按空行分事件，event / data / id 三种字段。
    async function* events() {
      take();
      let buffer = "";
      const decoder = new TextDecoder();
      let pending = new Uint8Array(0);
      const flush = function* (final) {
        const blocks = buffer.split(/\r?\n\r?\n/);
        buffer = final ? "" : blocks.pop();
        for (const block of blocks) {
          const event = { event: "message", data: "", id: undefined };
          const data = [];
          for (const line of block.split(/\r?\n/)) {
            if (line === "" || line.startsWith(":")) continue;
            const at = line.indexOf(":");
            const field = at < 0 ? line : line.slice(0, at);
            const value = at < 0 ? "" : line.slice(at + 1).replace(/^ /, "");
            if (field === "data") data.push(value);
            else if (field === "event") event.event = value;
            else if (field === "id") event.id = value;
          }
          if (data.length === 0 && event.id === undefined) continue;
          event.data = data.join("\n");
          yield event;
        }
      };
      for (let chunk = await nextChunk(); chunk !== null; chunk = await nextChunk()) {
        // 一块的末尾可能截断了一个多字节字符：留到下一块一起解码。
        const joined = new Uint8Array(pending.length + chunk.length);
        joined.set(pending);
        joined.set(chunk, pending.length);
        let cut = joined.length;
        for (let back = 1; back <= Math.min(3, joined.length); back++) {
          const byte = joined[joined.length - back];
          if ((byte & 0xc0) === 0x80) continue;
          const need = byte >= 0xf0 ? 4 : byte >= 0xe0 ? 3 : byte >= 0xc0 ? 2 : 1;
          if (need > back) cut = joined.length - back;
          break;
        }
        buffer += decoder.decode(joined.subarray(0, cut));
        pending = joined.slice(cut);
        yield* flush(false);
      }
      buffer += decoder.decode(pending);
      if (buffer.trim() !== "") {
        buffer += "\n\n";
        yield* flush(true);
      }
    }
    return Object.freeze({
      status: head.status,
      ok: head.status >= 200 && head.status < 300,
      url: head.url,
      headers: Object.freeze(head.headers),
      body,
      events,
      bytes: readAll,
      arrayBuffer: async () => (await readAll()).buffer,
      text: async () => new TextDecoder().decode(await readAll()),
      json: async () => JSON.parse(new TextDecoder().decode(await readAll())),
    });
  };

  const tools = new Map();
  const projectors = new Map();
  const labels = new Map();
  const renders = new Map();
  const hooks = new Map();
  // permissions：插件提供的权限模式的 decide。
  const permissionModes = new Map();
  const cards = new Map();
  const schedules = new Map();
  const commands = new Map();
  const slashes = new Map();
  const services = new Map();
  const providers = new Map();
  // ui.pages：页面名 → { render(host), actions: { 名字: (payload, host) => … } }。
  const pages = new Map();
  let prompt = null;
  let config = {};
  // 插件自带的文案（locales/<语言>.json，已拍平成 key → 文案）和默认语言。
  let i18nData = { messages: {}, defaultLocale: "en" };
  // 从“语言代码 → 值”的对象里按回退顺序挑一个。
  const pickLocale = (table) => {
    if (table === null || typeof table !== "object") return undefined;
    const order = JSON.parse(native.localeCandidates(native.locale(), JSON.stringify(Object.keys(table)), i18nData.defaultLocale));
    for (const code of order) if (table[code] !== undefined) return table[code];
    return undefined;
  };
  const formatText = (template, vars) =>
    vars && typeof vars === "object"
      ? String(template).replace(/\{([^{}]+)\}/g, (whole, name) => (Object.prototype.hasOwnProperty.call(vars, name) ? String(vars[name]) : whole))
      : String(template);
  const i18n = Object.freeze({
    // 界面当前语言（如 "zh-CN"、"en"），每次读都是最新的。
    get locale() {
      return native.locale();
    },
    // 插件自带哪些语言。
    get locales() {
      return Object.keys(i18nData.messages);
    },
    // 语言文件里的文案，按语言回退；{name} 换成 vars 里的值。找不到时返回 key 本身。
    t(key, vars) {
      key = String(key);
      const order = JSON.parse(native.localeCandidates(native.locale(), JSON.stringify(Object.keys(i18nData.messages)), i18nData.defaultLocale));
      for (const code of order) {
        const text = i18nData.messages[code]?.[key];
        if (text !== undefined) return formatText(text, vars);
      }
      return key;
    },
    // 从内联的多语言对象里挑当前语言的一条：pick({ "zh-CN": "数量", en: "Count" })。
    pick(table, vars) {
      const value = pickLocale(table);
      return typeof value === "string" ? formatText(value, vars) : value;
    },
  });
  let lifecycle = { activate: null, deactivate: null, state: null };

  const HOOKS = ["beforeTool", "afterTool", "beforeStep", "beforeRequest", "turnEnded", "reviewTool", "transformRequest", "turnStarted", "beforeWrite", "approve"];
  // on：会话事件类型（`tool/result`）、一个命名空间（`todo/*`）或全部（`*`）→ 处理函数。
  const listeners = new Map();
  // api：给别的插件调用的方法。
  const apiMethods = new Map();
  // host.tools.set 设的动态工具。
  const dynamicTools = new Map();
  // mentions：输入框 @ 提及的来源。
  const mentionSources = new Map();
  // watch：工作区文件变化。
  const watchers = new Map();
  const bytesOf = (data) => {
    if (data instanceof Uint8Array) return data;
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    throw new TypeError("expected a Uint8Array or an ArrayBuffer");
  };

  // SQLite：参数是数组（? 占位）或对象（:name 占位）；Uint8Array 存成 BLOB，读出来也是 Uint8Array。
  const sqlParams = (params) => {
    if (params === undefined || params === null) return "null";
    const convert = (value) => {
      if (value === undefined) return null;
      if (value instanceof Uint8Array || value instanceof ArrayBuffer || ArrayBuffer.isView(value)) {
        return { $blob: native.bytesToBase64(bytesOf(value)) };
      }
      if (value instanceof Date) return value.toISOString();
      if (typeof value === "bigint") return Number(value);
      return value;
    };
    if (Array.isArray(params)) return JSON.stringify(params.map(convert));
    if (typeof params === "object") return JSON.stringify(Object.fromEntries(Object.entries(params).map(([key, value]) => [key, convert(value)])));
    throw new TypeError("query parameters must be an array or an object");
  };
  const sqlRows = (json) =>
    JSON.parse(json).map((row) => {
      for (const [key, value] of Object.entries(row)) {
        if (value !== null && typeof value === "object" && typeof value.$blob === "string") row[key] = native.base64ToBytes(value.$blob);
      }
      return row;
    });
  function database(name) {
    const exec = async (sql, params) => JSON.parse(native.dbExec(name, String(sql), sqlParams(params)));
    const query = async (sql, params) => sqlRows(native.dbQuery(name, String(sql), sqlParams(params)));
    return Object.freeze({
      exec,
      query,
      get: async (sql, params) => (await query(sql, params))[0] ?? null,
      // 在一个事务里执行 fn：正常返回时提交，抛异常时回滚。调用结束时还没提交的事务也会回滚。
      transaction: async (fn) => {
        native.dbExec(name, "BEGIN IMMEDIATE", "null");
        try {
          const result = await fn();
          native.dbExec(name, "COMMIT", "null");
          return result;
        } catch (error) {
          try { native.dbExec(name, "ROLLBACK", "null"); } catch {}
          throw error;
        }
      },
      close: async () => native.dbClose(name),
    });
  }

  // 文件函数在宿主那边是同步执行的，这里包成 async：以后换成真正的异步实现时插件不用改。
  // `host.session.model()` 等要找的会话：给了 id 就按 id；否则是这一轮的会话，不在一轮里时是界面上当前的会话。
  const findSession = async (session) => {
    const sessions = JSON.parse(native.sessionList());
    const id = session ?? JSON.parse(native.identity()).session;
    return (id === null || id === undefined ? undefined : sessions.find((item) => item.id === String(id)))
      ?? (session === undefined || session === null ? sessions.find((item) => item.current) : undefined);
  };

  const host = Object.freeze({
    get workspace() {
      return native.workspace();
    },
    get config() {
      return config;
    },
    i18n,
    log: logger("info"),
    output: (text) => native.output(String(text)),
    emitEvent: (kind, data) => native.emitEvent(String(kind), JSON.stringify(data === undefined ? null : data)),

    // 工作区里的文件（fs:workspace:read / fs:workspace:write）。
    readFile: async (path) => native.readFile(String(path)),
    readFileBytes: async (path) => native.base64ToBytes(native.readFileBytes(String(path))),
    listDir: async (path) => native.listDir(path === undefined ? "." : String(path)),
    stat: async (path) => JSON.parse(native.stat(String(path))),
    exists: async (path) => JSON.parse(native.stat(String(path))) !== null,
    glob: async (pattern) => JSON.parse(await operation(native.glob(String(pattern)))),
    grep: async (pattern, options = {}) => JSON.parse(await operation(native.grep(String(pattern), JSON.stringify(options ?? {})))),
    writeFile: async (path, text) => native.writeFile(String(path), String(text)),
    writeFileBytes: async (path, data) => native.writeFileBytes(String(path), native.bytesToBase64(bytesOf(data))),
    appendFile: async (path, text) => native.appendFile(String(path), String(text)),
    mkdir: async (path) => native.mkdir(String(path)),
    remove: async (path, options = {}) => native.remove(String(path), options?.recursive === true),
    rename: async (from, to) => native.rename(String(from), String(to)),

    // 插件自己的数据目录：不需要能力，工具、页面、提示词里都能用；没有这个文件时得到 null。
    readData: async (path) => native.readData(String(path)) ?? null,
    writeData: async (path, text) => native.writeData(String(path), String(text)),
    appendData: async (path, text) => native.appendData(String(path), String(text)),
    removeData: async (path) => native.removeData(String(path)),
    listData: async () => native.listData(),

    // SQLite 数据库，文件在数据目录里（不需要能力）：host.db 是 db.sqlite，host.database(name) 是别的文件。
    db: database("db.sqlite"),
    database: (name) => database(String(name ?? "db.sqlite")),

    // zip 打包、解包（纯计算，不需要能力）。
    zip: async (entries) => {
      if (!Array.isArray(entries)) throw new TypeError("host.zip takes an array of { path, data } entries");
      const list = entries.map((entry) => {
        if (entry === null || typeof entry !== "object") throw new TypeError("each zip entry must be an object");
        const item = { path: String(entry.path ?? "") };
        if (entry.method !== undefined) item.method = String(entry.method);
        if (entry.dir === true) return item;
        const data = entry.data ?? "";
        item.data = typeof data === "string" ? native.bytesToBase64(new TextEncoder().encode(data)) : native.bytesToBase64(bytesOf(data));
        return item;
      });
      return native.base64ToBytes(native.zip(JSON.stringify(list)));
    },
    unzip: async (data) =>
      JSON.parse(native.unzip(native.bytesToBase64(bytesOf(data)))).map((entry) =>
        entry.dir ? Object.freeze(entry) : Object.freeze({
          path: entry.path,
          data: native.base64ToBytes(entry.data),
          text() { return new TextDecoder().decode(this.data); },
        })),

    // 不是完整的 Fetch API：响应是一个普通对象，正文最多 2 MB。
    fetch: async (url, options = {}) => {
      const { method, headers, body, signal, secretHeaders } = options ?? {};
      const request = { method, headers: {}, secretHeaders: {} };
      if (body !== undefined && body !== null) {
        if (typeof body === "string") request.body = body;
        else if (body instanceof URLSearchParams) request.body = body.toString();
        else request.bodyBase64 = native.bytesToBase64(bytesOf(body));
      }
      for (const [name, value] of Object.entries(headers ?? {})) request.headers[name] = String(value);
      for (const [name, secret] of Object.entries(secretHeaders ?? {})) request.secretHeaders[name] = String(secret);
      signal?.throwIfAborted?.();
      if (options?.stream === true) return streamingFetch(String(url), request, signal);
      const id = native.fetch(String(url), JSON.stringify(request));
      const response = JSON.parse(await operation(id, signal).catch(rethrowAbort(signal)));
      const bytes = () => native.base64ToBytes(response.bodyBase64);
      return Object.freeze({
        status: response.status,
        ok: response.status >= 200 && response.status < 300,
        url: response.url,
        headers: Object.freeze(response.headers),
        body: response.body,
        text: async () => response.body,
        json: async () => JSON.parse(response.body),
        bytes: async () => bytes(),
        arrayBuffer: async () => bytes().slice().buffer,
      });
    },

    // WebSocket 客户端（net:<主机>）：连接属于这次调用，调用结束时关掉。
    websocket: async (url, options = {}) => {
      const { signal, ...rest } = options ?? {};
      const op = native.websocket(String(url), JSON.stringify(rest));
      const info = JSON.parse(await operation(op, signal).catch(rethrowAbort(signal)));
      const socket = native.claimSocket(op);
      const next = async () => {
        const message = JSON.parse(await operation(native.wsNext(socket)));
        if (message === null) return null;
        return message.type === "binary" ? { type: "binary", data: native.base64ToBytes(message.data) } : message;
      };
      return Object.freeze({
        protocol: info.protocol,
        send: async (data) => {
          const id = typeof data === "string"
            ? native.wsSend(socket, data, false)
            : native.wsSend(socket, native.bytesToBase64(bytesOf(data)), true);
          await operation(id);
        },
        next,
        close: async () => void (await operation(native.wsClose(socket))),
        [Symbol.asyncIterator]() {
          return { next: async () => { const message = await next(); return message === null ? { done: true, value: undefined } : { done: false, value: message }; } };
        },
      });
    },

    // 用浏览器登录第三方账号（oauth）：token 存进清单里声明的一个密钥。
    oauth: Object.freeze({
      authorize: async (request) => JSON.parse(await operation(native.oauthAuthorize(JSON.stringify(request ?? {})))),
      token: async (secret) => JSON.parse(await operation(native.oauthToken(String(secret)))),
    }),

    // 启动一个程序（exec:<程序>），不经 shell；参数原样传过去。
    exec: async (program, args = [], options = {}) => {
      if (!Array.isArray(args)) throw new TypeError("exec arguments must be an array");
      const { signal, ...rest } = options ?? {};
      signal?.throwIfAborted?.();
      const id = native.exec(String(program), JSON.stringify(args.map(String)), JSON.stringify(rest));
      const result = JSON.parse(await operation(id, signal).catch(rethrowAbort(signal)));
      return Object.freeze({ ...result, ok: result.code === 0 });
    },

    // 启动一个程序、边跑边读它的输出（exec:<程序>）：进程属于这次调用，调用结束时被结束。
    spawn: (program, args = [], options = {}) => {
      if (!Array.isArray(args)) throw new TypeError("spawn arguments must be an array");
      const id = native.spawn(String(program), JSON.stringify(args.map(String)), JSON.stringify(options ?? {}));
      // 下一行输出：{ stream: "stdout" | "stderr", line }，两路都读完时是 null。
      const next = async () => JSON.parse(await operation(native.procRead(id)));
      return Object.freeze({
        next,
        async *lines() {
          for (;;) {
            const item = await next();
            if (item === null) return;
            yield item.line;
          }
        },
        [Symbol.asyncIterator]() {
          return this.lines();
        },
        write: async (text) => {
          await operation(native.procWrite(id, String(text)));
        },
        closeStdin: async () => {
          await operation(native.procWrite(id, null));
        },
        kill: () => native.procKill(id),
        wait: async () => {
          const result = JSON.parse(await operation(native.procWait(id)));
          return Object.freeze({ ...result, ok: result.code === 0 });
        },
      });
    },

    // 发一次请求（llm）：默认用当前会话的模型；options.model（"provider/model" 或模型名）、options.effort 可以换。
    llm: Object.freeze({
      complete: async (request, options = {}) => {
        const body = { ...(typeof request === "string" ? { prompt: request } : request ?? {}) };
        for (const key of ["model", "effort", "maxTokens"]) {
          if (options?.[key] !== undefined && options[key] !== null) body[key] = options[key];
        }
        const id = native.llm(JSON.stringify(body));
        return JSON.parse(await operation(id, options?.signal).catch(rethrowAbort(options?.signal))).text;
      },
      // 流式请求：for await (const { type, text } of host.llm.stream(…)) 逐块读（type 是 "text" 或 "thinking"）；
      // 或者 await stream.text() 读完返回全文。读完后 stream.usage 是用量。
      stream: (request, options = {}) => {
        const body = { ...(typeof request === "string" ? { prompt: request } : request ?? {}) };
        for (const key of ["model", "effort", "maxTokens"]) {
          if (options?.[key] !== undefined && options[key] !== null) body[key] = options[key];
        }
        const signal = options?.signal;
        // 参数、能力不对时在这里就抛出；连接在第一次读的时候等。
        const op = native.llmStream(JSON.stringify(body));
        let stream;
        const connect = async () => {
          if (stream === undefined) {
            await operation(op, signal).catch(rethrowAbort(signal));
            stream = native.claimStream(op);
          }
          return stream;
        };
        const decoder = new TextDecoder();
        let used = false;
        let done = false;
        const result = { usage: null, text: null };
        const next = async () => {
          if (done) return null;
          const id = await connect();
          const chunk = JSON.parse(await operation(native.readChunk(id), signal).catch(rethrowAbort(signal)));
          if (chunk === null) {
            done = true;
            return null;
          }
          const piece = JSON.parse(decoder.decode(native.base64ToBytes(chunk)));
          if (piece.type === "done") {
            done = true;
            result.usage = piece.usage ?? null;
            result.text = piece.text ?? "";
            return null;
          }
          return piece;
        };
        const take = () => {
          if (used) throw new TypeError("the stream has already been read");
          used = true;
        };
        return {
          [Symbol.asyncIterator]() {
            take();
            return { next: async () => { const piece = await next(); return piece === null ? { done: true, value: undefined } : { done: false, value: piece }; } };
          },
          text: async () => {
            take();
            let text = "";
            for (let piece = await next(); piece !== null; piece = await next()) if (piece.type === "text") text += piece.text;
            return result.text ?? text;
          },
          get usage() {
            return result.usage;
          },
        };
      },
    }),

    // 当前会话（session:read）；替用户发消息、停止、列出打开的会话（session:control）。
    session: Object.freeze({
      events: async (options = {}) => JSON.parse(native.sessionEvents(JSON.stringify(options ?? {}))).events,
      id: async () => JSON.parse(native.sessionEvents(JSON.stringify({ last: 0 }))).id,
      // send(text, { session, workspace, images: [{ data, mediaType, name }] })；也可以写 send({ text, images, ... })。
      send: async (text, options = {}) => {
        const request = text !== null && typeof text === "object" ? { ...text } : { ...(options ?? {}), text };
        request.text = request.text === undefined || request.text === null ? "" : String(request.text);
        if (request.images !== undefined) {
          if (!Array.isArray(request.images)) throw new TypeError("images must be an array");
          request.images = request.images.map((image) => ({
            media_type: String(image?.mediaType ?? image?.media_type ?? "image/png"),
            data: typeof image?.data === "string" ? image.data : native.bytesToBase64(bytesOf(image?.data)),
            ...(image?.name === undefined ? {} : { name: String(image.name) }),
          }));
        }
        return native.sessionSend(JSON.stringify(request));
      },
      continue: async (options = {}) => native.sessionSend(JSON.stringify({ ...(options ?? {}), text: "continue" })),
      stop: async (session) => native.sessionStop(session === undefined || session === null ? undefined : String(session)),
      list: async () => JSON.parse(native.sessionList()),
      // 会话下一轮用的模型和预设（session:read 或 session:control）；改它们要 session:control。
      // 不指定会话时：在一轮里调用的是这一轮的会话，其余场合是界面上当前的会话。
      model: async (session) => {
        const found = await findSession(session);
        return found === undefined ? null : { model: found.model, effort: found.effort ?? null };
      },
      preset: async (session) => (await findSession(session))?.preset ?? null,
      permission: async (session) => (await findSession(session))?.permission ?? null,
      models: async () => JSON.parse(native.sessionModels()),
      setModel: async (model, options = {}) => {
        const request = { session: options?.session };
        if (model !== undefined && model !== null) request.model = String(model);
        if (options !== null && typeof options === "object" && "effort" in options) request.effort = options.effort ?? null;
        native.sessionSet(JSON.stringify(request));
      },
      setPreset: async (preset, options = {}) => native.sessionSet(JSON.stringify({ session: options?.session, preset: String(preset) })),
      // 这一步的工具调用执行完后结束本轮（工具、afterTool 里可用）。
      endTurn: async () => native.endTurn(),
      // 当前的会话和代理：{ session, agent }（agent 是 "main" 或子代理名）；不在会话里时都是 null。
      current: async () => JSON.parse(native.identity()),
    }),

    // 子代理（agent:run）：得到子代理的报告。
    agent: Object.freeze({
      run: async (request, options = {}) => {
        const body = typeof request === "string" ? { task: request } : request;
        const id = native.agentRun(JSON.stringify(body ?? {}));
        return JSON.parse(await operation(id, options?.signal).catch(rethrowAbort(options?.signal))).report;
      },
    }),

    // 桌面通知（notify）：返回是否发出（太频繁时被丢弃）。
    notify: async (notification) =>
      native.notify(JSON.stringify(typeof notification === "string" ? { title: notification } : notification ?? {})),

    // 输入框：只在页面动作、命令、斜杠命令里可用。
    composer: Object.freeze({
      set: async (text) => native.composerSet(String(text)),
      get: async () => native.composerGet() ?? null,
      insert: async (text) => native.composerSet(`${native.composerGet() ?? ""}${String(text)}`),
    }),

    // 宿主自己的数据（dshr:config:*、dshr:skills、dshr:sessions:*、dshr:mcp）。
    dshr: (() => {
      const call = (method) => async (...args) => JSON.parse(native.dshr(method, JSON.stringify(args)));
      return Object.freeze({
        config: Object.freeze({ get: call("config.get"), patch: call("config.patch") }),
        skills: Object.freeze({ list: call("skills.list"), read: call("skills.read"), write: call("skills.write"), remove: call("skills.remove") }),
        sessions: Object.freeze({
          list: call("sessions.list"),
          read: call("sessions.read"),
          rename: call("sessions.rename"),
          remove: call("sessions.remove"),
          fork: call("sessions.fork"),
          import: call("sessions.import"),
        }),
        mcp: Object.freeze({ list: call("mcp.list"), add: call("mcp.add"), remove: call("mcp.remove"), setEnabled: call("mcp.setEnabled") }),
      });
    })(),

    // 插件管理（plugins:install）：安装每次都要用户确认，装好的插件能力仍要另外允许。
    plugins: Object.freeze({
      list: async () => JSON.parse(native.plugins("list", "[]")),
      install: async (request) => {
        const body = typeof request === "string" ? { url: request } : request;
        return JSON.parse(await operation(native.pluginsInstall(JSON.stringify(body ?? {})))).id;
      },
      uninstall: async (id) => JSON.parse(native.plugins("uninstall", JSON.stringify([String(id)]))),
      setEnabled: async (id, enabled) => JSON.parse(native.plugins("setEnabled", JSON.stringify([String(id), enabled !== false]))),
      // 调用别的插件导出的 api（清单里 uses = ["<id>"]）。
      call: async (id, method, args) =>
        JSON.parse(await operation(native.pluginsApi(String(id), String(method), JSON.stringify(args === undefined ? null : args)))),
    }),

    // 运行时增减的工具：整组替换，下一步起模型就能调用。
    tools: Object.freeze({
      set: (list) => {
        if (!Array.isArray(list)) throw new TypeError("host.tools.set takes an array of tools");
        const specs = [];
        const handlers = new Map();
        for (const tool of list) {
          if (tool === null || typeof tool !== "object") throw new TypeError("each tool must be an object");
          if (typeof tool.name !== "string") throw new TypeError("a tool is missing its `name`");
          if (typeof tool.execute !== "function") throw new TypeError(`tool ${tool.name} has no execute function`);
          if (tools.has(tool.name)) throw new TypeError(`tool ${tool.name} is already exported statically`);
          handlers.set(tool.name, tool);
          specs.push({
            name: tool.name,
            description: tool.description === undefined ? "" : String(tool.description),
            inputSchema: tool.inputSchema ?? { type: "object", properties: {} },
            readOnly: tool.readOnly === true,
          });
        }
        native.toolsSet(JSON.stringify(specs));
        dynamicTools.clear();
        for (const [name, tool] of handlers) dynamicTools.set(name, tool);
      },
      list: () => [...dynamicTools.keys()],
    }),

    // 清单里声明的密钥。
    secrets: Object.freeze({
      get: async (name) => native.secret(String(name)) ?? null,
      has: async (name) => (native.secret(String(name)) ?? null) !== null,
    }),

    ui: Object.freeze({
      // 让已经打开的页面重画（工具、钩子改了数据之后）。
      refresh: (page) => native.uiRefresh(String(page)),
      // 运行时藏起、重新显示入口：自己的入口（sidebar:<id>、settings:<id>、panel:<id>、status:<id>）或可藏的内置入口。
      hide: async (target) => native.uiVisibility(String(target), true),
      show: async (target) => native.uiVisibility(String(target), false),
      hidden: async () => JSON.parse(native.uiHidden()),
    }),
  });

  const fn = (value, what) => {
    if (typeof value !== "function") throw new TypeError(`${what} must be a function`);
    return value;
  };
  // "30s"、"10m"、"2h" 或毫秒数。
  const parseInterval = (every, id) => {
    if (typeof every === "number" && Number.isFinite(every)) return every;
    const match = /^(\d+(?:\.\d+)?)\s*(ms|s|m|h)$/.exec(String(every ?? "").trim());
    if (!match) throw new TypeError(`schedule ${id}: every must be milliseconds or a string like "30s", "10m", "2h"`);
    return Number(match[1]) * { ms: 1, s: 1000, m: 60000, h: 3600000 }[match[2]];
  };
  const asJson = (value) => {
    const json = JSON.stringify(value === undefined ? null : value);
    return json === undefined ? "null" : json;
  };

  return {
    register(namespace, configJson, i18nJson) {
      config = Object.freeze(JSON.parse(configJson));
      if (i18nJson) i18nData = JSON.parse(i18nJson);
      const exported = (name) => namespace[name] ?? namespace.default?.[name];

      // prompt(context, host)：这一轮追加到系统提示词后面的文本。
      const promptExport = exported("prompt");
      if (promptExport !== undefined) prompt = fn(promptExport, "the `prompt` export");

      // events：{ "<插件 id>/<名字>": (data) => 文本 }，或者值是带 project / label / render 函数的对象：
      // project 决定事件怎么进模型上下文，label 决定它在对话区显示成哪一行字，render 画成一棵节点树。
      const events = exported("events");
      if (events !== undefined) {
        if (events === null || typeof events !== "object" || Array.isArray(events)) {
          throw new TypeError("the `events` export must be an object keyed by event kind");
        }
        for (const [kind, entry] of Object.entries(events)) {
          const project = typeof entry === "function" ? entry : entry?.project;
          const label = typeof entry === "function" ? undefined : entry?.label;
          const render = typeof entry === "function" ? undefined : entry?.render;
          if (project !== undefined && typeof project !== "function") throw new TypeError(`event ${kind}: project must be a function`);
          if (label !== undefined && typeof label !== "function") throw new TypeError(`event ${kind}: label must be a function`);
          if (render !== undefined && typeof render !== "function") throw new TypeError(`event ${kind}: render must be a function`);
          if (project === undefined && label === undefined && render === undefined) {
            throw new TypeError(`event ${kind} has neither a project, a label nor a render function`);
          }
          if (project !== undefined) projectors.set(kind, project);
          if (label !== undefined) labels.set(kind, label);
          if (render !== undefined) renders.set(kind, render);
        }
      }

      // hooks：挂在 agent 循环上的函数。
      const hooksExport = exported("hooks");
      if (hooksExport !== undefined) {
        if (hooksExport === null || typeof hooksExport !== "object") throw new TypeError("the `hooks` export must be an object");
        for (const [name, handler] of Object.entries(hooksExport)) {
          if (!HOOKS.includes(name)) throw new TypeError(`unknown hook ${name} (available: ${HOOKS.join(", ")})`);
          hooks.set(name, fn(handler, `hook ${name}`));
        }
      }

      // api：给别的插件调用的方法（参数和结果都是 JSON）。
      const apiExport = exported("api");
      if (apiExport !== undefined) {
        if (apiExport === null || typeof apiExport !== "object") throw new TypeError("the `api` export must be an object of functions");
        for (const [name, handler] of Object.entries(apiExport)) apiMethods.set(name, fn(handler, `api ${name}`));
      }

      // mentions：输入框 @ 提及的来源。
      const mentionsExport = exported("mentions") ?? [];
      if (!Array.isArray(mentionsExport)) throw new TypeError("the `mentions` export must be an array");
      const mentionDecls = [];
      for (const source of mentionsExport) {
        if (source === null || typeof source !== "object") throw new TypeError("each mention source must be an object");
        const { id, label, icon } = source;
        if (typeof id !== "string" || !/^[a-z0-9_-]+$/.test(id)) throw new TypeError("a mention source needs an id of lowercase letters, digits, - and _");
        if (typeof label !== "string") throw new TypeError(`mention source ${id} needs a label`);
        if (mentionSources.has(id)) throw new TypeError(`mention source ${id} is defined twice`);
        mentionSources.set(id, { search: fn(source.search, `mention ${id}: search`), resolve: fn(source.resolve, `mention ${id}: resolve`) });
        mentionDecls.push({ id, label, icon: typeof icon === "string" ? icon : null });
      }

      // watch：工作区里的文件变了就调用。
      const watchExport = exported("watch") ?? [];
      if (!Array.isArray(watchExport)) throw new TypeError("the `watch` export must be an array");
      const watchDecls = [];
      for (const entry of watchExport) {
        const id = entry?.id;
        if (typeof id !== "string" || id === "") throw new TypeError("each watch entry needs a string id");
        if (watchers.has(id)) throw new TypeError(`watch ${id} is defined twice`);
        const globs = entry.glob === undefined ? ["**"] : Array.isArray(entry.glob) ? entry.glob.map(String) : [String(entry.glob)];
        watchers.set(id, fn(entry.run, `watch ${id}: run`));
        watchDecls.push({ id, globs });
      }

      // on：订阅会话事件。
      const onExport = exported("on");
      if (onExport !== undefined) {
        if (onExport === null || typeof onExport !== "object") throw new TypeError("the `on` export must be an object of event type → function");
        for (const [pattern, handler] of Object.entries(onExport)) {
          const valid = pattern === "*" || /^[a-z0-9_.-]+\/(\*|[a-z0-9_.\/-]+)$/.test(pattern);
          if (!valid) throw new TypeError(`on: \`${pattern}\` should be an event type (tool/result), a namespace (todo/*) or *`);
          listeners.set(pattern, fn(handler, `on ${pattern}`));
        }
      }

      // 生命周期。
      const activate = exported("activate");
      const deactivate = exported("deactivate");
      const state = exported("state");
      lifecycle = {
        activate: activate === undefined ? null : fn(activate, "the `activate` export"),
        deactivate: deactivate === undefined ? null : fn(deactivate, "the `deactivate` export"),
        state: null,
      };
      if (state !== undefined) {
        if (state === null || typeof state !== "object") throw new TypeError("the `state` export must be an object with snapshot and restore");
        lifecycle.state = { snapshot: fn(state.snapshot, "state.snapshot"), restore: fn(state.restore, "state.restore") };
      }

      // schedules：后台定时任务。
      const schedulesExport = exported("schedules") ?? [];
      if (!Array.isArray(schedulesExport)) throw new TypeError("the `schedules` export must be an array");
      const scheduleDecls = [];
      for (const entry of schedulesExport) {
        const id = entry?.id;
        if (typeof id !== "string" || id === "") throw new TypeError("each schedule needs a string id");
        if (schedules.has(id)) throw new TypeError(`schedule ${id} is defined twice`);
        const everyMs = parseInterval(entry.every, id);
        if (everyMs < 10000) throw new TypeError(`schedule ${id} runs too often (at least every 10 seconds)`);
        schedules.set(id, fn(entry.run, `schedule ${id}: run`));
        scheduleDecls.push({ id, everyMs: Math.round(everyMs) });
      }

      // presets：输入卡片预设菜单里的预设（只是提示词，不需要能力）。
      const presetsExport = exported("presets") ?? [];
      if (!Array.isArray(presetsExport)) throw new TypeError("the `presets` export must be an array");
      const presetDecls = presetsExport.map((preset) => {
        if (preset === null || typeof preset !== "object") throw new TypeError("each preset must be an object");
        const { id, label, description = "", instructions = "", readOnly = false, tools } = preset;
        if (typeof id !== "string" || typeof label !== "string") throw new TypeError("a preset needs a string id and label");
        if (tools !== undefined && (!Array.isArray(tools) || tools.some((tool) => typeof tool !== "string"))) {
          throw new TypeError(`preset ${id}: tools must be an array of tool names`);
        }
        return { id, label, description: String(description), instructions: String(instructions), readOnly: readOnly === true, tools: tools ?? null };
      });

      // permissions：权限菜单里的权限模式（要 permissions:mode）。选中时每次工具调用由 decide 下结论。
      const permissionsExport = exported("permissions") ?? [];
      if (!Array.isArray(permissionsExport)) throw new TypeError("the `permissions` export must be an array");
      const permissionDecls = permissionsExport.map((mode) => {
        if (mode === null || typeof mode !== "object") throw new TypeError("each permission mode must be an object");
        const { id, label, description = "", icon } = mode;
        if (typeof id !== "string" || typeof label !== "string") throw new TypeError("a permission mode needs a string id and label");
        if (permissionModes.has(id)) throw new TypeError(`permission mode ${id} is defined twice`);
        permissionModes.set(id, fn(mode.decide, `permission mode ${id}: decide`));
        return { id, label, description: String(description), icon: typeof icon === "string" ? icon : null };
      });

      // commands：命令面板里的命令；slash：输入框里的 /命令。run 的返回值决定界面做什么。
      const commandsExport = exported("commands") ?? [];
      if (!Array.isArray(commandsExport)) throw new TypeError("the `commands` export must be an array");
      const commandDecls = commandsExport.map((command) => {
        const { id, title, keys } = command ?? {};
        if (typeof id !== "string" || typeof title !== "string") throw new TypeError("a command needs a string id and title");
        if (commands.has(id)) throw new TypeError(`command ${id} is defined twice`);
        commands.set(id, fn(command.run, `command ${id}: run`));
        return { id, title, keys: typeof keys === "string" ? keys : null };
      });
      const slashExport = exported("slash") ?? [];
      if (!Array.isArray(slashExport)) throw new TypeError("the `slash` export must be an array");
      const slashDecls = slashExport.map((command) => {
        const { name, description = "" } = command ?? {};
        if (typeof name !== "string" || !/^[a-z0-9][a-z0-9_-]{0,31}$/.test(name)) {
          throw new TypeError("a slash command needs a name of lowercase letters, digits, `-` and `_`");
        }
        if (slashes.has(name)) throw new TypeError(`slash command /${name} is defined twice`);
        slashes.set(name, fn(command.run, `slash command /${name}: run`));
        return { name, description: String(description) };
      });

      // services：常驻服务（要 background）。
      const servicesExport = exported("services") ?? [];
      if (!Array.isArray(servicesExport)) throw new TypeError("the `services` export must be an array");
      const serviceDecls = servicesExport.map((service) => {
        const { id } = service ?? {};
        if (typeof id !== "string" || id === "") throw new TypeError("each service needs a string id");
        if (services.has(id)) throw new TypeError(`service ${id} is defined twice`);
        services.set(id, fn(service.run, `service ${id}: run`));
        return { id };
      });

      // providers：模型服务商（要 provider）。地址和鉴权由插件给，流式协议用内置的客户端。
      const providersExport = exported("providers") ?? [];
      if (!Array.isArray(providersExport)) throw new TypeError("the `providers` export must be an array");
      const providerDecls = providersExport.map((provider) => {
        const { id, name, protocol, baseUrl } = provider ?? {};
        if (typeof id !== "string" || typeof name !== "string") throw new TypeError("a provider needs a string id and name");
        if (protocol !== "openai" && protocol !== "messages") throw new TypeError(`provider ${id}: protocol must be "openai" or "messages"`);
        if (typeof baseUrl !== "string") throw new TypeError(`provider ${id} needs a baseUrl`);
        const models = provider.models;
        if (!Array.isArray(models) && typeof models !== "function") throw new TypeError(`provider ${id}: models must be an array or a function`);
        if (provider.auth !== undefined) fn(provider.auth, `provider ${id}: auth`);
        providers.set(id, provider);
        return { id, name, protocol, baseUrl, models: Array.isArray(models) ? models.map(String) : null };
      });

      // themes：配色方案（纯数据）。
      const themesExport = exported("themes") ?? [];
      if (!Array.isArray(themesExport)) throw new TypeError("the `themes` export must be an array");
      const themeDecls = themesExport.map((theme) => {
        const { id, name, light = {}, dark = {} } = theme ?? {};
        if (typeof id !== "string" || typeof name !== "string") throw new TypeError("a theme needs a string id and name");
        const colors = (set, mode) => {
          if (set === null || typeof set !== "object") throw new TypeError(`theme ${id}: ${mode} must be an object`);
          for (const [key, value] of Object.entries(set)) {
            if (typeof value !== "string" || !/^#[0-9a-fA-F]{6}$/.test(value)) throw new TypeError(`theme ${id}: ${mode}.${key} must be a #rrggbb color`);
          }
          return set;
        };
        return { id, name, light: colors(light, "light"), dark: colors(dark, "dark") };
      });

      // ui：插件给界面加的入口和页面，以及要藏起来的内置入口。页面的节点树由宿主检查和绘制。
      const ui = { sidebar: [], settings: [], panels: [], status: [], hide: [], pages: [] };
      const uiExport = exported("ui");
      if (uiExport !== undefined) {
        if (uiExport === null || typeof uiExport !== "object") throw new TypeError("the `ui` export must be an object");
        for (const [name, page] of Object.entries(uiExport.pages ?? {})) {
          const render = typeof page === "function" ? page : page?.render;
          if (typeof render !== "function") throw new TypeError(`ui page ${name} has no render function`);
          const actions = (typeof page === "function" ? {} : page.actions) ?? {};
          for (const [action, handler] of Object.entries(actions)) {
            if (typeof handler !== "function") throw new TypeError(`ui page ${name}: action ${action} must be a function`);
          }
          pages.set(name, { render, actions });
        }
        ui.pages = [...pages.keys()];
        for (const place of ["sidebar", "settings", "panels", "status"]) {
          const entries = uiExport[place] ?? [];
          if (!Array.isArray(entries)) throw new TypeError(`ui.${place} must be an array`);
          for (const entry of entries) {
            if (entry === null || typeof entry !== "object") throw new TypeError(`each entry of ui.${place} must be an object`);
            const { id, icon, page } = entry;
            const label = entry.label ?? (place === "status" ? id : undefined);
            if (typeof id !== "string" || typeof label !== "string") throw new TypeError(`an entry of ui.${place} needs a string id and label`);
            if (!pages.has(page)) throw new TypeError(`ui.${place} entry ${id} opens page ${page}, which ui.pages does not define`);
            ui[place].push({ id, label, icon: typeof icon === "string" ? icon : null, page });
          }
        }
        const hide = uiExport.hide ?? [];
        if (!Array.isArray(hide) || hide.some((item) => typeof item !== "string")) throw new TypeError("ui.hide must be an array of strings");
        ui.hide = hide;
      }

      const specs = [];
      const list = exported("tools") ?? [];
      if (!Array.isArray(list)) throw new TypeError("the `tools` export must be an array");
      for (const tool of list) {
        if (tool === null || typeof tool !== "object") throw new TypeError("each entry of `tools` must be an object");
        if (typeof tool.name !== "string") throw new TypeError("a tool is missing its `name`");
        if (typeof tool.execute !== "function") throw new TypeError(`tool ${tool.name} has no execute function`);
        if (tools.has(tool.name)) throw new TypeError(`tool ${tool.name} is defined twice`);
        tools.set(tool.name, tool);
        specs.push({
          name: tool.name,
          description: tool.description === undefined ? "" : String(tool.description),
          inputSchema: tool.inputSchema ?? { type: "object", properties: {} },
          readOnly: tool.readOnly === true,
        });
      }

      // cards：{ 工具名: { title(input, output), render(input, output) } }，或者直接一个 render 函数。只能给自己的工具画。
      const cardsExport = exported("cards");
      if (cardsExport !== undefined) {
        if (cardsExport === null || typeof cardsExport !== "object") throw new TypeError("the `cards` export must be an object keyed by tool name");
        for (const [tool, card] of Object.entries(cardsExport)) {
          if (!tools.has(tool)) throw new TypeError(`cards.${tool}: this plugin has no tool named ${tool}`);
          const render = typeof card === "function" ? card : card?.render;
          const title = typeof card === "function" ? undefined : card?.title;
          if (render !== undefined && typeof render !== "function") throw new TypeError(`cards.${tool}.render must be a function`);
          if (title !== undefined && typeof title !== "function") throw new TypeError(`cards.${tool}.title must be a function`);
          if (render === undefined && title === undefined) throw new TypeError(`cards.${tool} has neither a title nor a render function`);
          cards.set(tool, { render, title });
        }
      }

      return JSON.stringify({
        tools: specs,
        events: [...projectors.keys()],
        labels: [...labels.keys()],
        renders: [...renders.keys()],
        prompt: prompt !== null,
        hooks: [...hooks.keys()],
        cards: [...cards.keys()],
        lifecycle: { activate: lifecycle.activate !== null, deactivate: lifecycle.deactivate !== null, state: lifecycle.state !== null },
        schedules: scheduleDecls,
        presets: presetDecls,
        permissions: permissionDecls,
        commands: commandDecls,
        slash: slashDecls,
        services: serviceDecls,
        providers: providerDecls,
        themes: themeDecls,
        on: [...listeners.keys()],
        api: [...apiMethods.keys()],
        mentions: mentionDecls,
        watch: watchDecls,
        ui,
      });
    },

    async prompt(contextJson) {
      if (prompt === null) return null;
      const text = await prompt(JSON.parse(contextJson), host);
      return text === undefined || text === null ? null : String(text);
    },

    async project(kind, dataJson, label) {
      const project = (label ? labels : projectors).get(kind);
      if (project === undefined) return null;
      const text = await project(JSON.parse(dataJson));
      return text === undefined || text === null ? null : String(text);
    },

    // 页面现在的节点树（JSON）。返回字符串时当作一段文字。
    async render(name) {
      const page = pages.get(name);
      if (page === undefined) throw new Error(`unknown ui page ${name}`);
      const tree = await page.render(host);
      return JSON.stringify(typeof tree === "string" ? { type: "text", text: tree } : tree ?? { type: "column", children: [] });
    },

    // 用户在页面上触发的动作；返回给用户看的一句话（可以没有）。
    async act(name, action, payloadJson) {
      const page = pages.get(name);
      if (page === undefined) throw new Error(`unknown ui page ${name}`);
      const handler = page.actions[action];
      if (handler === undefined) throw new Error(`ui page ${name} has no action ${action}`);
      const result = await handler(JSON.parse(payloadJson), host);
      return result === undefined || result === null ? null : String(result);
    },

    async call(name, inputJson) {
      const tool = tools.get(name) ?? dynamicTools.get(name);
      if (tool === undefined) throw new Error(`unknown tool ${name}`);
      let result = await tool.execute(JSON.parse(inputJson), host);
      // `{ content, images: [{ data, mediaType }] }`：图片交给模型（能看图的模型才看得到）。
      if (result !== null && typeof result === "object" && Array.isArray(result.images)) {
        const images = result.images.map((image) => ({
          media_type: String(image?.mediaType ?? "image/png"),
          data: typeof image?.data === "string" ? image.data : native.bytesToBase64(bytesOf(image?.data)),
        }));
        native.toolImages(JSON.stringify(images));
        result = result.content ?? "";
      }
      if (typeof result === "string") return result;
      if (result === undefined || result === null) return "";
      const json = JSON.stringify(result, null, 2);
      return json === undefined ? String(result) : json;
    },

    // 其余入口，结果都是 JSON 文本。
    async dispatch(kind, name, payloadJson) {
      const payload = JSON.parse(payloadJson);
      switch (kind) {
        case "hook": {
          const handler = hooks.get(name);
          if (handler === undefined) throw new Error(`unknown hook ${name}`);
          return asJson(await handler(payload, host));
        }
        case "permission": {
          const decide = permissionModes.get(name);
          if (decide === undefined) throw new Error(`unknown permission mode ${name}`);
          return asJson(await decide(payload, host));
        }
        case "card": {
          const card = cards.get(name);
          if (card === undefined) throw new Error(`no card for tool ${name}`);
          const part = card[payload.part];
          if (part === undefined) return "null";
          const result = await part(payload.input, payload.output, payload.isError === true);
          if (payload.part === "title") return asJson(result === undefined || result === null ? null : String(result));
          return asJson(typeof result === "string" ? { type: "text", text: result } : result);
        }
        case "event": {
          const render = renders.get(name);
          if (render === undefined) throw new Error(`no render function for event ${name}`);
          const tree = await render(payload);
          return asJson(typeof tree === "string" ? { type: "text", text: tree } : tree);
        }
        case "lifecycle": {
          if (name === "snapshot") return asJson(lifecycle.state ? await lifecycle.state.snapshot() : null);
          if (name === "restore") return asJson(lifecycle.state ? await lifecycle.state.restore(payload) : null);
          const handler = lifecycle[name];
          return asJson(handler ? await handler(host) : null);
        }
        case "command":
        case "slash": {
          const run = (kind === "command" ? commands : slashes).get(name);
          if (run === undefined) throw new Error(`unknown ${kind} ${name}`);
          return asJson(await run(kind === "slash" ? { args: payload.args ?? "" } : {}, host));
        }
        case "service": {
          const run = services.get(name);
          if (run === undefined) throw new Error(`unknown service ${name}`);
          // 服务一直跑到被停下：宿主停插件时中止这次调用，signal 随之 abort。
          const controller = new AbortController();
          return asJson(await run(host, controller.signal));
        }
        case "provider": {
          const provider = providers.get(name);
          if (provider === undefined) throw new Error(`unknown provider ${name}`);
          if (payload.part === "models") {
            const models = typeof provider.models === "function" ? await provider.models(host) : provider.models;
            return asJson((models ?? []).map(String));
          }
          return asJson(provider.auth ? await provider.auth(host) : {});
        }
        case "api": {
          const handler = apiMethods.get(name);
          if (handler === undefined) throw new Error(`no api method ${name}`);
          return asJson(await handler(payload.args, host));
        }
        case "watch": {
          const run = watchers.get(name);
          if (run === undefined) throw new Error(`unknown watch ${name}`);
          return asJson(await run(payload, host));
        }
        case "mention": {
          const source = mentionSources.get(name);
          if (source === undefined) throw new Error(`no mention source ${name}`);
          if (payload.part === "search") {
            const items = (await source.search(String(payload.query ?? ""), host)) ?? [];
            if (!Array.isArray(items)) throw new TypeError(`mention ${name}: search must return an array`);
            return asJson(items.slice(0, 20).map((item) => ({
              id: String(item?.id ?? ""),
              title: String(item?.title ?? item?.id ?? ""),
              detail: item?.detail === undefined || item?.detail === null ? null : String(item.detail),
            })));
          }
          const content = await source.resolve(payload.id, host);
          if (typeof content === "string") return asJson({ title: String(payload.title ?? payload.id), text: content });
          return asJson({ title: String(content?.title ?? payload.title ?? payload.id), text: String(content?.text ?? "") });
        }
        case "on": {
          // 精确的类型、命名空间、全部，依次调用。
          const type = String(payload.type ?? "");
          const namespace = type.split("/")[0];
          // 流式输出（stream/text 等）只交给明确订阅了它的处理函数，不进 "*"。
          const patterns = payload.stream === true ? [type, "stream/*"] : [type, `${namespace}/*`, "*"];
          for (const pattern of patterns) {
            const handler = listeners.get(pattern);
            if (handler !== undefined) await handler(payload, host);
          }
          return "null";
        }
        case "schedule": {
          const run = schedules.get(name);
          if (run === undefined) throw new Error(`unknown schedule ${name}`);
          return asJson(await run(host));
        }
        default:
          throw new Error(`unknown dispatch kind ${kind}`);
      }
    },

    // 触发到期的定时器；返回离下一个还有多少毫秒，没有定时器时是 -1。
    tick() {
      const now = Date.now();
      const due = [...timers.entries()].filter(([, timer]) => timer.due <= now).sort(([, a], [, b]) => a.due - b.due);
      for (const [id, timer] of due) {
        if (!timers.has(id)) continue;
        if (timer.interval === null) timers.delete(id);
        else timer.due = now + timer.interval;
        try {
          timer.callback(...timer.args);
        } catch (error) {
          console.error("uncaught error in a timer callback:", error);
        }
      }
      if (timers.size === 0) return -1;
      let next = Infinity;
      for (const timer of timers.values()) next = Math.min(next, timer.due);
      return Math.max(0, next - Date.now());
    },

    settle(id, ok, text) {
      const waiting = pending.get(id);
      if (waiting === undefined) return;
      pending.delete(id);
      if (ok) waiting.resolve(text);
      else waiting.reject(new Error(text));
    },

    reset() {
      pending.clear();
      timers.clear();
    },
  };
})
