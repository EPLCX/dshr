// `import { EventEmitter } from "dshr:events"`：Node 的 EventEmitter 的常用子集，纯 JS。

export class EventEmitter {
  #listeners = new Map();

  on(name, listener) {
    if (typeof listener !== "function") throw new TypeError("the listener must be a function");
    if (!this.#listeners.has(name)) this.#listeners.set(name, []);
    this.#listeners.get(name).push({ listener, once: false });
    return this;
  }

  addListener(name, listener) {
    return this.on(name, listener);
  }

  once(name, listener) {
    this.on(name, listener);
    const list = this.#listeners.get(name);
    list[list.length - 1].once = true;
    return this;
  }

  off(name, listener) {
    const list = this.#listeners.get(name);
    if (!list) return this;
    const index = list.findIndex((entry) => entry.listener === listener);
    if (index >= 0) list.splice(index, 1);
    if (list.length === 0) this.#listeners.delete(name);
    return this;
  }

  removeListener(name, listener) {
    return this.off(name, listener);
  }

  removeAllListeners(name) {
    if (name === undefined) this.#listeners.clear();
    else this.#listeners.delete(name);
    return this;
  }

  emit(name, ...args) {
    const list = this.#listeners.get(name);
    if (!list || list.length === 0) {
      if (name === "error") throw args[0] instanceof Error ? args[0] : new Error(`unhandled error event: ${args[0]}`);
      return false;
    }
    for (const entry of [...list]) {
      if (entry.once) this.off(name, entry.listener);
      entry.listener.apply(this, args);
    }
    return true;
  }

  listenerCount(name) {
    return this.#listeners.get(name)?.length ?? 0;
  }

  listeners(name) {
    return (this.#listeners.get(name) ?? []).map((entry) => entry.listener);
  }

  eventNames() {
    return [...this.#listeners.keys()];
  }
}

export function once(emitter, name) {
  return new Promise((resolve) => emitter.once(name, (...args) => resolve(args)));
}

export default EventEmitter;
