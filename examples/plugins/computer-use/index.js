// computer-use：让 agent 操作这台电脑——截屏、移动和点击鼠标、拖动、滚动、输入文字、按快捷键。
//
// 只用系统自带（或常见）的程序，不装任何东西：
//   Windows  powershell：System.Drawing 截屏，user32 的 SendInput 发鼠标键盘事件
//   macOS    screencapture + sips + base64 截屏，osascript（JXA 调 CoreGraphics）发鼠标键盘事件
//   Linux    import（ImageMagick）+ base64 截屏，xdotool 发鼠标键盘事件（只支持 X11）
//
// 坐标约定：模型看到的是缩放过的截图，工具参数里的坐标都是“最近一张截图里的像素坐标”，插件负责换算成屏幕坐标。
// 所以第一次操作之前必须先截一张图。

const TMP_IMAGE = "/tmp/dshr-computer-use.jpg";

/** 最近一张截图：{ imageWidth, imageHeight, screenWidth, screenHeight, platform, at, data? } */
let geometry = null;
/** 判断出来的系统：windows / macos / linux */
let detected = null;
/** 设置页上显示的最近一张图（data URI） */
let preview = null;

// ---------------------------------------------------------------- 工具

const ACTIONS = [
  "screenshot",
  "click",
  "double_click",
  "triple_click",
  "right_click",
  "middle_click",
  "move",
  "drag",
  "scroll",
  "type",
  "key",
  "cursor_position",
  "wait",
];

export const tools = [{
  name: "computer",
  description: [
    "Operate the user's computer: take screenshots of the primary display, move/click/drag the mouse, scroll, type text and press keys.",
    "Always start with action=screenshot. All x/y coordinates are pixel coordinates in the MOST RECENT screenshot (the plugin maps them to the real screen).",
    "After every other action a fresh screenshot is returned automatically (unless disabled), so check it to confirm the action worked before continuing.",
    "Keys use names joined by '+', e.g. 'ctrl+c', 'cmd+space', 'alt+tab', 'enter', 'escape', 'f5'; several combos may be separated by spaces, e.g. 'ctrl+a backspace'.",
    "Use 'cmd' on macOS, 'ctrl' on Windows/Linux ('meta'/'win'/'super' = the Windows/Command/Super key).",
    "Prefer keyboard shortcuts and typing over long chains of clicks. Never enter passwords, make purchases, send messages or delete data unless the user explicitly asked for exactly that.",
  ].join(" "),
  inputSchema: {
    type: "object",
    properties: {
      action: { type: "string", enum: ACTIONS, description: "What to do" },
      x: { type: "number", description: "X in screenshot pixels (click/move/scroll position; drag start)" },
      y: { type: "number", description: "Y in screenshot pixels" },
      to_x: { type: "number", description: "drag: end X in screenshot pixels" },
      to_y: { type: "number", description: "drag: end Y in screenshot pixels" },
      direction: { type: "string", enum: ["up", "down", "left", "right"], description: "scroll direction" },
      amount: { type: "integer", minimum: 1, maximum: 50, description: "scroll: number of wheel notches (default 3)" },
      text: { type: "string", description: "type: the text to type (newlines press Enter)" },
      keys: { type: "string", description: "key: key combo(s), e.g. 'ctrl+s' or 'ctrl+a backspace'" },
      modifiers: { type: "string", description: "click/drag: keys to hold while clicking, e.g. 'shift' or 'ctrl+shift'" },
      seconds: { type: "number", minimum: 0, maximum: 30, description: "wait: how long to wait before the screenshot (default 1)" },
      screenshot: { type: "boolean", description: "Return a screenshot after the action (default: plugin setting, normally true)" },
    },
    required: ["action"],
  },
  async execute(input, host) {
    return await perform(input, host);
  },
}];

export const cards = {
  computer: {
    title: (input) => describe(input),
    render: (input, output, isError) => {
      if (output === null) return { type: "progress", value: 0.5, label: `${describe(input)}…` };
      const text = typeof output === "string" ? output : (output?.content ?? JSON.stringify(output));
      return { type: "text", text: String(text).split("\n")[0], tone: isError ? "danger" : "neutral" };
    },
  },
};

function describe(input) {
  const at = input.x !== undefined && input.y !== undefined ? ` (${input.x}, ${input.y})` : "";
  switch (input.action) {
    case "type": return `输入文字 “${truncate(input.text ?? "", 30)}”`;
    case "key": return `按键 ${input.keys ?? ""}`;
    case "drag": return `拖动 (${input.x}, ${input.y}) → (${input.to_x}, ${input.to_y})`;
    case "scroll": return `滚动 ${input.direction ?? "down"} ×${input.amount ?? 3}${at}`;
    case "screenshot": return "截屏";
    case "wait": return `等待 ${input.seconds ?? 1} 秒`;
    default: return `${input.action}${at}`;
  }
}

function truncate(text, n) {
  return text.length > n ? `${text.slice(0, n)}…` : text;
}

// ---------------------------------------------------------------- 动作

async function perform(input, host) {
  const action = input.action;
  if (!ACTIONS.includes(action)) throw new Error(`unknown action "${action}"; use one of: ${ACTIONS.join(", ")}`);
  const platform = await detectPlatform(host);
  const settings = options(host);

  if (action === "screenshot") return await screenshotResult(host, platform, settings, "Screenshot taken.");

  if (action === "wait") {
    const seconds = clamp(Number(input.seconds ?? 1), 0, 30);
    await sleep(seconds * 1000);
    return await screenshotResult(host, platform, settings, `Waited ${seconds}s.`);
  }

  if (action === "cursor_position") {
    const g = requireGeometry(platform);
    const [sx, sy] = await backend(platform).cursor(host);
    const [x, y] = toImage(g, sx, sy);
    return `Cursor is at (${x}, ${y}) in screenshot coordinates.`;
  }

  const ops = [];
  let summary;
  const held = parseCombo(input.modifiers ?? "");
  const press = () => held.forEach((key) => ops.push({ op: "keydown", key }));
  const release = () => [...held].reverse().forEach((key) => ops.push({ op: "keyup", key }));

  switch (action) {
    case "click":
    case "double_click":
    case "triple_click":
    case "right_click":
    case "middle_click": {
      const button = action === "right_click" ? "right" : action === "middle_click" ? "middle" : "left";
      const count = action === "double_click" ? 2 : action === "triple_click" ? 3 : 1;
      const point = optionalPoint(input, platform);
      if (point) ops.push({ op: "move", x: point[0], y: point[1] }, { op: "sleep", ms: 40 });
      press();
      for (let i = 1; i <= count; i++) {
        ops.push({ op: "down", button, count: i }, { op: "sleep", ms: 30 }, { op: "up", button, count: i });
        if (i < count) ops.push({ op: "sleep", ms: 60 });
      }
      release();
      summary = `${count === 1 ? "Clicked" : count === 2 ? "Double-clicked" : "Triple-clicked"} ${button}` +
        (point ? ` at (${input.x}, ${input.y}).` : " at the current cursor position.");
      break;
    }
    case "move": {
      const [x, y] = point(input, platform);
      ops.push({ op: "move", x, y });
      summary = `Moved the mouse to (${input.x}, ${input.y}).`;
      break;
    }
    case "drag": {
      const [x1, y1] = point(input, platform);
      const [x2, y2] = point({ x: input.to_x, y: input.to_y }, platform, "to_x/to_y");
      ops.push({ op: "move", x: x1, y: y1 }, { op: "sleep", ms: 60 });
      press();
      ops.push({ op: "down", button: "left", count: 1 }, { op: "sleep", ms: 80 });
      const steps = 12;
      for (let i = 1; i <= steps; i++) {
        ops.push({ op: "move", x: Math.round(x1 + ((x2 - x1) * i) / steps), y: Math.round(y1 + ((y2 - y1) * i) / steps) });
        ops.push({ op: "sleep", ms: 15 });
      }
      ops.push({ op: "sleep", ms: 80 }, { op: "up", button: "left", count: 1 });
      release();
      summary = `Dragged from (${input.x}, ${input.y}) to (${input.to_x}, ${input.to_y}).`;
      break;
    }
    case "scroll": {
      const direction = input.direction ?? "down";
      const amount = clamp(Math.round(Number(input.amount ?? 3)), 1, 50);
      const point = optionalPoint(input, platform);
      if (point) ops.push({ op: "move", x: point[0], y: point[1] }, { op: "sleep", ms: 40 });
      const dy = direction === "down" ? amount : direction === "up" ? -amount : 0;
      const dx = direction === "right" ? amount : direction === "left" ? -amount : 0;
      ops.push({ op: "wheel", dy, dx });
      summary = `Scrolled ${direction} by ${amount}${point ? ` at (${input.x}, ${input.y})` : ""}.`;
      break;
    }
    case "type": {
      if (typeof input.text !== "string" || input.text === "") throw new Error("type needs a non-empty `text`");
      if (input.text.length > 5000) throw new Error("text is too long (max 5000 characters per call)");
      ops.push({ op: "text", text: input.text });
      summary = `Typed ${input.text.length} characters.`;
      break;
    }
    case "key": {
      const combos = String(input.keys ?? "").trim().split(/\s+/).filter(Boolean);
      if (combos.length === 0) throw new Error("key needs `keys`, e.g. 'ctrl+c' or 'enter'");
      for (const combo of combos) {
        const keys = parseCombo(combo);
        keys.forEach((key) => ops.push({ op: "keydown", key }));
        ops.push({ op: "sleep", ms: 20 });
        [...keys].reverse().forEach((key) => ops.push({ op: "keyup", key }));
        ops.push({ op: "sleep", ms: 40 });
      }
      summary = `Pressed ${combos.join(" ")}.`;
      break;
    }
  }

  await backend(platform).run(host, ops);

  const wantShot = input.screenshot ?? settings.autoScreenshot;
  if (!wantShot) return summary;
  await sleep(settings.settleMs);
  return await screenshotResult(host, platform, settings, summary);
}

async function screenshotResult(host, platform, settings, summary) {
  const shot = await backend(platform).screenshot(host, settings);
  geometry = {
    imageWidth: shot.imageWidth,
    imageHeight: shot.imageHeight,
    screenWidth: shot.screenWidth,
    screenHeight: shot.screenHeight,
    platform,
    at: Date.now(),
  };
  preview = `data:image/jpeg;base64,${shot.data}`;
  const content = `${summary}\nScreenshot: ${shot.imageWidth}x${shot.imageHeight} pixels ` +
    `(screen ${shot.screenWidth}x${shot.screenHeight}, ${platform}). Use coordinates in this image.`;
  return { content, images: [{ data: shot.data, mediaType: "image/jpeg" }] };
}

// ---------------------------------------------------------------- 坐标

function requireGeometry(platform) {
  if (!geometry || geometry.platform !== platform) {
    throw new Error("No screenshot yet: call the computer tool with action=screenshot first, then use coordinates from that image.");
  }
  return geometry;
}

function point(input, platform, name = "x/y") {
  const x = Number(input.x);
  const y = Number(input.y);
  if (!Number.isFinite(x) || !Number.isFinite(y)) throw new Error(`this action needs ${name} coordinates`);
  const g = requireGeometry(platform);
  if (x < 0 || y < 0 || x > g.imageWidth || y > g.imageHeight) {
    throw new Error(`(${x}, ${y}) is outside the screenshot (${g.imageWidth}x${g.imageHeight})`);
  }
  return [
    clamp(Math.round((x * g.screenWidth) / g.imageWidth), 0, g.screenWidth - 1),
    clamp(Math.round((y * g.screenHeight) / g.imageHeight), 0, g.screenHeight - 1),
  ];
}

function optionalPoint(input, platform) {
  return input.x === undefined && input.y === undefined ? null : point(input, platform);
}

function toImage(g, sx, sy) {
  return [Math.round((sx * g.imageWidth) / g.screenWidth), Math.round((sy * g.imageHeight) / g.screenHeight)];
}

// ---------------------------------------------------------------- 按键名

const ALIASES = {
  control: "ctrl", ctl: "ctrl",
  option: "alt", opt: "alt",
  cmd: "meta", command: "meta", super: "meta", win: "meta", windows: "meta", "⌘": "meta",
  return: "enter",
  esc: "escape",
  del: "delete",
  bksp: "backspace",
  pgup: "pageup", page_up: "pageup",
  pgdn: "pagedown", page_down: "pagedown",
  arrowup: "up", arrowdown: "down", arrowleft: "left", arrowright: "right",
  ins: "insert",
  caps: "capslock", caps_lock: "capslock",
  prtsc: "printscreen", print: "printscreen",
  plus: "=", minus: "-", comma: ",", period: ".", slash: "/", backslash: "\\", semicolon: ";",
  quote: "'", apostrophe: "'", backtick: "`", grave: "`", bracketleft: "[", bracketright: "]",
  spacebar: "space",
};
const MODIFIERS = ["ctrl", "shift", "alt", "meta"];

function normalizeKey(name) {
  const lower = name.toLowerCase();
  return ALIASES[lower] ?? (name.length === 1 ? name.toLowerCase() : lower);
}

function parseCombo(combo) {
  // “+” 键写作 plus（即 = 键）。
  return combo.split("+").filter(Boolean).map(normalizeKey);
}

// ---------------------------------------------------------------- 系统判断

async function detectPlatform(host) {
  const configured = host.config?.platform;
  if (configured && configured !== "auto") return configured;
  if (detected) return detected;
  const workspace = String(host.workspace ?? "");
  if (/^[A-Za-z]:[\\/]/.test(workspace) || workspace.startsWith("\\\\")) return (detected = "windows");
  try {
    const result = await host.exec("uname", ["-s"], { timeoutMs: 5000 });
    const name = result.stdout.trim();
    if (name === "Darwin") return (detected = "macos");
    if (name === "Linux" || name.endsWith("BSD")) return (detected = "linux");
    if (/MINGW|MSYS|CYGWIN/i.test(name)) return (detected = "windows");
  } catch {
    // Windows 上一般没有 uname
    return (detected = "windows");
  }
  throw new Error("cannot tell which operating system this is; set `platform` in the plugin settings");
}

function backend(platform) {
  switch (platform) {
    case "windows": return windows;
    case "macos": return macos;
    case "linux": return linux;
    default: throw new Error(`unsupported platform "${platform}"`);
  }
}

function options(host) {
  const c = host.config ?? {};
  return {
    maxWidth: clamp(Math.round(Number(c.max_width ?? 1280)), 320, 3840),
    quality: clamp(Math.round(Number(c.quality ?? 70)), 10, 100),
    autoScreenshot: c.auto_screenshot ?? true,
    settleMs: clamp(Math.round(Number(c.settle_ms ?? 600)), 0, 10000),
  };
}

async function run(host, program, args, extra = {}) {
  const result = await host.exec(program, args, { timeoutMs: 30000, ...extra });
  if (!result.ok) {
    const detail = (result.stderr || result.stdout || "").trim().slice(-2000);
    throw new Error(`${program} failed (exit ${result.code})${detail ? `: ${detail}` : ""}`);
  }
  return result.stdout;
}

// ---------------------------------------------------------------- Windows

const WIN_VK = {
  ctrl: 0x11, shift: 0x10, alt: 0x12, meta: 0x5b,
  enter: 0x0d, tab: 0x09, escape: 0x1b, backspace: 0x08, delete: 0x2e, insert: 0x2d, space: 0x20,
  up: 0x26, down: 0x28, left: 0x25, right: 0x27, home: 0x24, end: 0x23, pageup: 0x21, pagedown: 0x22,
  capslock: 0x14, printscreen: 0x2c, menu: 0x5d, pause: 0x13,
  ";": 0xba, "=": 0xbb, ",": 0xbc, "-": 0xbd, ".": 0xbe, "/": 0xbf, "`": 0xc0, "[": 0xdb, "\\": 0xdc, "]": 0xdd, "'": 0xde,
};
for (let i = 1; i <= 24; i++) WIN_VK[`f${i}`] = 0x6f + i;

function winVk(key) {
  if (WIN_VK[key] !== undefined) return WIN_VK[key];
  if (/^[a-z]$/.test(key)) return key.toUpperCase().charCodeAt(0);
  if (/^[0-9]$/.test(key)) return key.charCodeAt(0);
  throw new Error(`unknown key "${key}"`);
}

const WIN_NATIVE = String.raw`
using System;
using System.Runtime.InteropServices;
using System.Threading;
public static class DshrInput {
  [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Explicit)] struct UNION { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
  [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public UNION u; }
  [StructLayout(LayoutKind.Sequential)] struct POINT { public int X; public int Y; }
  [DllImport("user32.dll", SetLastError = true)] static extern uint SendInput(uint n, INPUT[] inputs, int size);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);

  static void Send(params INPUT[] inputs) {
    uint sent = SendInput((uint)inputs.Length, inputs, Marshal.SizeOf(typeof(INPUT)));
    if (sent != inputs.Length) throw new Exception("SendInput was blocked (error " + Marshal.GetLastWin32Error() + "); the target window may be running as administrator or the screen is locked");
  }
  static INPUT Mouse(uint flags, int data) { INPUT i = new INPUT(); i.type = 0; i.u.mi.dwFlags = flags; i.u.mi.mouseData = unchecked((uint)data); return i; }
  static INPUT Key(ushort vk, ushort scan, uint flags) { INPUT i = new INPUT(); i.type = 1; i.u.ki.wVk = vk; i.u.ki.wScan = scan; i.u.ki.dwFlags = flags; return i; }

  public static int ScreenWidth() { return GetSystemMetrics(0); }
  public static int ScreenHeight() { return GetSystemMetrics(1); }
  public static int[] Cursor() { POINT p; GetCursorPos(out p); return new int[] { p.X, p.Y }; }
  public static void Move(int x, int y) { SetCursorPos(x, y); }
  public static void Button(string button, bool down) {
    uint f = button == "right" ? (down ? 0x0008u : 0x0010u) : button == "middle" ? (down ? 0x0020u : 0x0040u) : (down ? 0x0002u : 0x0004u);
    Send(Mouse(f, 0));
  }
  public static void Wheel(int down, int right) {
    if (down != 0) Send(Mouse(0x0800, -down * 120));
    if (right != 0) Send(Mouse(0x1000, right * 120));
  }
  public static void KeyEvent(int vk, bool down) {
    bool extended = (vk >= 0x21 && vk <= 0x28) || vk == 0x2D || vk == 0x2E || vk == 0x5B || vk == 0x5C || vk == 0x5D;
    Send(Key((ushort)vk, 0, (down ? 0u : 2u) | (extended ? 1u : 0u)));
  }
  public static void Text(string text) {
    foreach (char c in text) {
      if (c == '\r') continue;
      if (c == '\n') { KeyEvent(0x0D, true); KeyEvent(0x0D, false); }
      else if (c == '\t') { KeyEvent(0x09, true); KeyEvent(0x09, false); }
      else Send(Key(0, c, 4u), Key(0, c, 6u));
      Thread.Sleep(3);
    }
  }
}
`;

const WIN_PRELUDE = `
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Add-Type -TypeDefinition @'
${WIN_NATIVE}
'@
[DshrInput]::SetProcessDPIAware() | Out-Null
function Emit($value) { [Console]::Out.Write((ConvertTo-Json -Compress -Depth 4 $value)) }
`;

const WIN_SCREENSHOT = `
Add-Type -AssemblyName System.Drawing
$w = [DshrInput]::ScreenWidth(); $h = [DshrInput]::ScreenHeight()
$full = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($full)
$g.CopyFromScreen(0, 0, 0, 0, $full.Size)
$g.Dispose()
$tw = [Math]::Min($w, [int]$P.maxWidth); $th = [int][Math]::Round($h * $tw / $w)
$img = New-Object System.Drawing.Bitmap $tw, $th
$g = [System.Drawing.Graphics]::FromImage($img)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.DrawImage($full, 0, 0, $tw, $th)
$g.Dispose(); $full.Dispose()
$codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object { $_.MimeType -eq 'image/jpeg' }
$params = New-Object System.Drawing.Imaging.EncoderParameters 1
$params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter ([System.Drawing.Imaging.Encoder]::Quality), ([long]$P.quality)
$stream = New-Object System.IO.MemoryStream
$img.Save($stream, $codec, $params)
$img.Dispose()
Emit @{ data = [Convert]::ToBase64String($stream.ToArray()); imageWidth = $tw; imageHeight = $th; screenWidth = $w; screenHeight = $h }
`;

const WIN_RUN = `
foreach ($o in $P.ops) {
  switch ($o.op) {
    'move' { [DshrInput]::Move([int]$o.x, [int]$o.y) }
    'down' { [DshrInput]::Button([string]$o.button, $true) }
    'up' { [DshrInput]::Button([string]$o.button, $false) }
    'wheel' { [DshrInput]::Wheel([int]$o.dy, [int]$o.dx) }
    'keydown' { [DshrInput]::KeyEvent([int]$o.vk, $true) }
    'keyup' { [DshrInput]::KeyEvent([int]$o.vk, $false) }
    'text' { [DshrInput]::Text([string]$o.text) }
    'sleep' { Start-Sleep -Milliseconds ([int]$o.ms) }
  }
}
Emit @{ ok = $true }
`;

const WIN_CURSOR = `
$c = [DshrInput]::Cursor()
Emit @{ x = $c[0]; y = $c[1] }
`;

async function powershell(host, body, params) {
  // 脚本经标准输入传过去（只含 ASCII，不受控制台代码页影响）；参数是 UTF-8 JSON 的 base64，中文等字符原样保留。
  const script = `$P = ConvertFrom-Json ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('${base64Utf8(JSON.stringify(params))}')))\n` +
    WIN_PRELUDE + body;
  const bootstrap = "$s = [Console]::In.ReadToEnd(); Invoke-Expression $s";
  const out = await run(host, "powershell", [
    "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", base64Utf16(bootstrap),
  ], { stdin: script });
  return parseJsonTail(out);
}

const windows = {
  async screenshot(host, settings) {
    return await powershell(host, WIN_SCREENSHOT, { maxWidth: settings.maxWidth, quality: settings.quality });
  },
  async run(host, ops) {
    const mapped = ops.map((op) => (op.op === "keydown" || op.op === "keyup") ? { op: op.op, vk: winVk(op.key) } : op);
    await powershell(host, WIN_RUN, { ops: mapped });
  },
  async cursor(host) {
    const { x, y } = await powershell(host, WIN_CURSOR, {});
    return [x, y];
  },
};

// ---------------------------------------------------------------- macOS

const MAC_KEYCODE = {
  a: 0, s: 1, d: 2, f: 3, h: 4, g: 5, z: 6, x: 7, c: 8, v: 9, b: 11, q: 12, w: 13, e: 14, r: 15, y: 16, t: 17,
  1: 18, 2: 19, 3: 20, 4: 21, 6: 22, 5: 23, "=": 24, 9: 25, 7: 26, "-": 27, 8: 28, 0: 29, "]": 30, o: 31, u: 32,
  "[": 33, i: 34, p: 35, enter: 36, l: 37, j: 38, "'": 39, k: 40, ";": 41, "\\": 42, ",": 43, "/": 44, n: 45, m: 46,
  ".": 47, tab: 48, space: 49, "`": 50, backspace: 51, escape: 53, meta: 55, shift: 56, capslock: 57, alt: 58, ctrl: 59,
  fn: 63, home: 115, pageup: 116, delete: 117, end: 119, pagedown: 121, left: 123, right: 124, down: 125, up: 126,
  f1: 122, f2: 120, f3: 99, f4: 118, f5: 96, f6: 97, f7: 98, f8: 100, f9: 101, f10: 109, f11: 103, f12: 111,
  f13: 105, f14: 107, f15: 113, f16: 106, f17: 64, f18: 79, f19: 80, f20: 90,
};
const MAC_FLAG = { shift: 0x20000, ctrl: 0x40000, alt: 0x80000, meta: 0x100000 };

function macKey(key) {
  const code = MAC_KEYCODE[key];
  if (code === undefined) throw new Error(`unknown key "${key}"`);
  return { code, flag: MAC_FLAG[key] ?? 0 };
}

// JXA：用 CoreGraphics 发事件。坐标是“点”（左上角为原点），和 screencapture 的像素差一个缩放系数（Retina 是 2）。
const MAC_RUN = `
ObjC.import('CoreGraphics');
ObjC.import('AppKit');
const BUTTONS = { left: [1, 2, 6, 0], right: [3, 4, 7, 1], middle: [25, 26, 27, 2] };
function sleep(ms) { $.NSThread.sleepForTimeInterval(ms / 1000); }
function post(event) { $.CGEventPost(0, event); }
let flags = 0;
let pressed = null;
let pos = $.CGEventGetLocation($.CGEventCreate(null));
function mouse(type, button, count) {
  const event = $.CGEventCreateMouseEvent(null, type, { x: pos.x, y: pos.y }, BUTTONS[button][3]);
  if (count) $.CGEventSetIntegerValueField(event, 1, count);
  $.CGEventSetFlags(event, flags);
  post(event);
}
function key(code, down) {
  const event = $.CGEventCreateKeyboardEvent(null, code, down);
  $.CGEventSetFlags(event, flags);
  post(event);
}
function paste(text) {
  const board = $.NSPasteboard.generalPasteboard;
  const saved = ObjC.unwrap(board.stringForType($.NSPasteboardTypeString));
  board.clearContents;
  board.setStringForType($(text), $.NSPasteboardTypeString);
  const before = flags;
  flags = 0x100000;
  key(9, true); key(9, false);
  flags = before;
  sleep(250);
  if (typeof saved === 'string') { board.clearContents; board.setStringForType($(saved), $.NSPasteboardTypeString); }
}
for (const op of P.ops) {
  switch (op.op) {
    case 'move':
      pos = { x: op.x, y: op.y };
      mouse(pressed ? BUTTONS[pressed][2] : 5, pressed || 'left');
      break;
    case 'down': pressed = op.button; mouse(BUTTONS[op.button][0], op.button, op.count); break;
    case 'up': mouse(BUTTONS[op.button][1], op.button, op.count); pressed = null; break;
    case 'wheel': {
      if (typeof $.CGEventCreateScrollWheelEvent2 !== 'function') throw new Error('scrolling needs macOS 13 or newer');
      const event = $.CGEventCreateScrollWheelEvent2(null, 1, 2, -op.dy, -op.dx, 0);
      $.CGEventSetFlags(event, flags);
      post(event);
      break;
    }
    case 'keydown': if (op.flag) flags |= op.flag; key(op.code, true); break;
    case 'keyup': key(op.code, false); if (op.flag) flags &= ~op.flag; break;
    case 'text': paste(op.text); break;
    case 'sleep': sleep(op.ms); break;
  }
}
JSON.stringify({ ok: true });
`;

const MAC_INFO = `
ObjC.import('CoreGraphics');
ObjC.import('AppKit');
const frame = $.NSScreen.screens.objectAtIndex(0).frame;
const pos = $.CGEventGetLocation($.CGEventCreate(null));
JSON.stringify({ width: frame.size.width, height: frame.size.height, x: pos.x, y: pos.y });
`;

async function jxa(host, body, params) {
  // 非 ASCII 字符写成 \\uXXXX，不依赖 osascript 读标准输入时用的编码。
  const literal = JSON.stringify(JSON.stringify(params)).replace(/[\u007f-￿]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`);
  const script = `const P = JSON.parse(${literal});\n${body}`;
  const out = await run(host, "osascript", ["-l", "JavaScript", "-"], { stdin: script });
  return parseJsonTail(out);
}

const macos = {
  async screenshot(host, settings) {
    await run(host, "screencapture", ["-x", "-m", "-t", "jpg", TMP_IMAGE]);
    const info = await run(host, "sips", ["-g", "pixelWidth", "-g", "pixelHeight", TMP_IMAGE]);
    let width = Number(/pixelWidth:\s*(\d+)/.exec(info)?.[1]);
    let height = Number(/pixelHeight:\s*(\d+)/.exec(info)?.[1]);
    if (!width || !height) throw new Error(`cannot read the screenshot size: ${info.trim()}`);
    if (width > settings.maxWidth) {
      await run(host, "sips", ["--resampleWidth", String(settings.maxWidth), "-s", "formatOptions", String(settings.quality), TMP_IMAGE]);
      height = Math.round((height * settings.maxWidth) / width);
      width = settings.maxWidth;
    }
    const [data, screen] = await Promise.all([run(host, "base64", ["-i", TMP_IMAGE]), jxa(host, MAC_INFO, {})]);
    return {
      data: data.replace(/\s+/g, ""),
      imageWidth: width,
      imageHeight: height,
      screenWidth: Math.round(screen.width),
      screenHeight: Math.round(screen.height),
    };
  },
  async run(host, ops) {
    const mapped = ops.map((op) => (op.op === "keydown" || op.op === "keyup") ? { op: op.op, ...macKey(op.key) } : op);
    await jxa(host, MAC_RUN, { ops: mapped });
  },
  async cursor(host) {
    const { x, y } = await jxa(host, MAC_INFO, {});
    return [Math.round(x), Math.round(y)];
  },
};

// ---------------------------------------------------------------- Linux（X11）

const X_KEYSYM = {
  ctrl: "Control_L", shift: "Shift_L", alt: "Alt_L", meta: "Super_L",
  enter: "Return", tab: "Tab", escape: "Escape", backspace: "BackSpace", delete: "Delete", insert: "Insert", space: "space",
  up: "Up", down: "Down", left: "Left", right: "Right", home: "Home", end: "End", pageup: "Prior", pagedown: "Next",
  capslock: "Caps_Lock", printscreen: "Print", menu: "Menu", pause: "Pause",
  "-": "minus", "=": "equal", "[": "bracketleft", "]": "bracketright", "\\": "backslash", ";": "semicolon",
  "'": "apostrophe", ",": "comma", ".": "period", "/": "slash", "`": "grave",
};
for (let i = 1; i <= 24; i++) X_KEYSYM[`f${i}`] = `F${i}`;

function xKey(key) {
  if (X_KEYSYM[key]) return X_KEYSYM[key];
  if (/^[a-z0-9]$/.test(key)) return key;
  if (/^[A-Za-z0-9_]+$/.test(key)) return key; // 其他 keysym 原样传，如 XF86AudioMute
  throw new Error(`unknown key "${key}"`);
}

const X_BUTTON = { left: "1", middle: "2", right: "3" };

const linux = {
  async screenshot(host, settings) {
    const geometry = (await run(host, "xdotool", ["getdisplaygeometry"])).trim().split(/\s+/).map(Number);
    const [screenWidth, screenHeight] = geometry;
    if (!screenWidth || !screenHeight) throw new Error("cannot read the display size (is DISPLAY set and is this X11?)");
    const imageWidth = Math.min(screenWidth, settings.maxWidth);
    const imageHeight = Math.round((screenHeight * imageWidth) / screenWidth);
    await run(host, "import", [
      "-silent", "-window", "root", "-resize", `${imageWidth}x${imageHeight}!`, "-quality", String(settings.quality), TMP_IMAGE,
    ]);
    const data = (await run(host, "base64", ["-w0", TMP_IMAGE])).trim();
    return { data, imageWidth, imageHeight, screenWidth, screenHeight };
  },
  async run(host, ops) {
    // 除了输入文字，其余操作串成一次 xdotool 调用（xdotool 支持命令链）。
    ops = ops.map((op) => (op.op === "keydown" || op.op === "keyup") ? { ...op, key: xKey(op.key) } : op); // 先检查按键名，再动手
    let chain = [];
    const flush = async () => {
      if (chain.length) await run(host, "xdotool", chain);
      chain = [];
    };
    for (const op of ops) {
      switch (op.op) {
        case "move": chain.push("mousemove", String(op.x), String(op.y)); break;
        case "down": chain.push("mousedown", X_BUTTON[op.button]); break;
        case "up": chain.push("mouseup", X_BUTTON[op.button]); break;
        case "wheel":
          if (op.dy) chain.push("click", "--repeat", String(Math.abs(op.dy)), "--delay", "30", op.dy > 0 ? "5" : "4");
          if (op.dx) chain.push("click", "--repeat", String(Math.abs(op.dx)), "--delay", "30", op.dx > 0 ? "7" : "6");
          break;
        case "keydown": chain.push("keydown", op.key); break;
        case "keyup": chain.push("keyup", op.key); break;
        case "sleep": chain.push("sleep", (op.ms / 1000).toFixed(3)); break;
        case "text":
          await flush();
          // xdotool 按 locale 解码参数：C locale 下中文等字符会报“Invalid multi-byte sequence”。
          await run(host, "xdotool", ["type", "--delay", "8", "--", op.text], { env: { LC_CTYPE: "C.UTF-8" } });
          break;
      }
    }
    await flush();
  },
  async cursor(host) {
    const out = await run(host, "xdotool", ["getmouselocation", "--shell"]);
    return [Number(/X=(\d+)/.exec(out)?.[1] ?? 0), Number(/Y=(\d+)/.exec(out)?.[1] ?? 0)];
  },
};

// ---------------------------------------------------------------- 设置页：试一试

export const ui = {
  pages: {
    main: {
      render: (host) => {
        const platform = host.config?.platform && host.config.platform !== "auto" ? host.config.platform : (detected ?? "（未判断）");
        const children = [
          { type: "text", text: "电脑操作", style: "heading" },
          {
            type: "markdown",
            text: "模型通过 `computer` 工具截屏、点击、输入。先点下面的按钮确认截屏和权限都正常：\n\n" +
              "- **macOS**：在“系统设置 → 隐私与安全性”里给 DSHR（或运行它的终端）打开**屏幕录制**和**辅助功能**。\n" +
              "- **Windows**：以管理员身份运行的窗口收不到普通权限程序发的输入。\n" +
              "- **Linux**：只支持 X11，需要装 `xdotool` 和 ImageMagick。",
          },
          { type: "kv", items: [
            { key: "系统", value: platform },
            { key: "最近截图", value: geometry ? `${geometry.imageWidth}×${geometry.imageHeight}（屏幕 ${geometry.screenWidth}×${geometry.screenHeight}）` : "还没有" },
          ] },
          { type: "row", children: [
            { type: "button", label: "截一张图试试", action: "shot", variant: "primary" },
            { type: "button", label: "清除", action: "clear", variant: "ghost" },
          ] },
        ];
        if (preview) children.push({ type: "image", src: preview, alt: "最近一张截图", width: 640 });
        return { type: "column", gap: "medium", children };
      },
      actions: {
        shot: async (_, host) => {
          const platform = await detectPlatform(host);
          await screenshotResult(host, platform, options(host), "");
          return `截图成功：${geometry.imageWidth}×${geometry.imageHeight}`;
        },
        clear: () => {
          preview = null;
        },
      },
    },
  },
  settings: [{ id: "computer-use", label: "电脑操作", page: "main" }],
};

// ---------------------------------------------------------------- 热重载时保留状态

export const state = {
  snapshot: () => ({ geometry, detected }),
  restore: (saved) => {
    geometry = saved?.geometry ?? null;
    detected = saved?.detected ?? null;
  },
};

// ---------------------------------------------------------------- 小工具

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, Number.isFinite(value) ? value : min));
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function parseJsonTail(text) {
  const trimmed = String(text).trim();
  const start = trimmed.lastIndexOf("\n{");
  const json = start >= 0 ? trimmed.slice(start + 1) : trimmed;
  try {
    return JSON.parse(json);
  } catch {
    throw new Error(`unexpected output: ${truncate(trimmed, 500)}`);
  }
}

function base64Utf8(text) {
  return bytesToBase64(new TextEncoder().encode(text));
}

function base64Utf16(text) {
  const bytes = new Uint8Array(text.length * 2);
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    bytes[i * 2] = code & 0xff;
    bytes[i * 2 + 1] = code >> 8;
  }
  return bytesToBase64(bytes);
}

function bytesToBase64(bytes) {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}
