// `import path from "dshr:path"`：POSIX 风格的路径计算（和 Node 的 `path.posix` 一样的结果），只算字符串，不碰文件。
// host 上的文件函数两种分隔符都认，这里统一用 `/`。

const normalizeParts = (parts, absolute) => {
  const out = [];
  for (const part of parts) {
    if (part === "" || part === ".") continue;
    if (part === "..") {
      if (out.length && out[out.length - 1] !== "..") out.pop();
      else if (!absolute) out.push("..");
    } else {
      out.push(part);
    }
  }
  return out;
};

export const sep = "/";
export const delimiter = ":";

export function normalize(path) {
  const text = String(path).replace(/\\/g, "/");
  if (text === "") return ".";
  const absolute = text.startsWith("/");
  const trailing = text.endsWith("/");
  let result = normalizeParts(text.split("/"), absolute).join("/");
  if (result === "" && !absolute) result = ".";
  if (result !== "" && trailing) result += "/";
  return absolute ? `/${result}` : result;
}

export function join(...parts) {
  const joined = parts.map(String).filter((part) => part !== "").join("/");
  return joined === "" ? "." : normalize(joined);
}

export function isAbsolute(path) {
  return String(path).replace(/\\/g, "/").startsWith("/");
}

export function resolve(...parts) {
  let resolved = "";
  for (let i = parts.length - 1; i >= 0 && !resolved.startsWith("/"); i--) {
    const part = String(parts[i]).replace(/\\/g, "/");
    if (part !== "") resolved = resolved === "" ? part : `${part}/${resolved}`;
  }
  const absolute = resolved.startsWith("/");
  const result = normalizeParts(resolved.split("/"), absolute).join("/");
  return absolute ? `/${result}` : result || ".";
}

export function dirname(path) {
  const text = String(path).replace(/\\/g, "/").replace(/\/+$/, "");
  const at = text.lastIndexOf("/");
  if (at < 0) return ".";
  if (at === 0) return "/";
  return text.slice(0, at);
}

export function basename(path, suffix) {
  const text = String(path).replace(/\\/g, "/").replace(/\/+$/, "");
  let base = text.slice(text.lastIndexOf("/") + 1);
  if (suffix !== undefined && base.endsWith(suffix) && base !== suffix) base = base.slice(0, -suffix.length);
  return base;
}

export function extname(path) {
  const base = basename(path);
  const at = base.lastIndexOf(".");
  return at <= 0 ? "" : base.slice(at);
}

export function relative(from, to) {
  const a = normalizeParts(resolve(from).split("/"), true);
  const b = normalizeParts(resolve(to).split("/"), true);
  let common = 0;
  while (common < a.length && common < b.length && a[common] === b[common]) common++;
  return [...a.slice(common).map(() => ".."), ...b.slice(common)].join("/");
}

export function parse(path) {
  const text = String(path).replace(/\\/g, "/");
  const root = text.startsWith("/") ? "/" : "";
  const base = basename(text);
  const ext = extname(text);
  const dir = text.includes("/") ? dirname(text) : "";
  return { root, dir, base, ext, name: ext ? base.slice(0, -ext.length) : base };
}

export function format({ dir, root, base, name, ext }) {
  const file = base ?? `${name ?? ""}${ext ?? ""}`;
  const folder = dir ?? root ?? "";
  if (!folder) return file;
  return folder.endsWith("/") ? `${folder}${file}` : `${folder}/${file}`;
}

const path = { sep, delimiter, normalize, join, isAbsolute, resolve, dirname, basename, extname, relative, parse, format };
path.posix = path;
export default path;
