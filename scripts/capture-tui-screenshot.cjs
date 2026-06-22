#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawn } = require("child_process");

let pty;
let chromium;
try {
  pty = require("node-pty");
  ({ chromium } = require("playwright"));
} catch (error) {
  pty = null;
  try {
    ({ chromium } = require("playwright"));
  } catch (_) {
    console.error(error.message);
    console.error("\nRun this through scripts/capture-tui-screenshot.sh.");
    process.exit(1);
  }
}

const ROOT = path.resolve(__dirname, "..");
const args = process.argv.slice(2);
const options = parseArgs(args);
const xtermRoot = path.dirname(require.resolve("@xterm/xterm/package.json"));

main().catch((error) => {
  console.error(error.stack || error.message || String(error));
  process.exit(1);
});

async function main() {
  fs.mkdirSync(path.dirname(options.output), { recursive: true });

  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({
    viewport: { width: options.width, height: options.height },
    deviceScaleFactor: options.scale,
  });

  await page.setContent(html());
  await page.addStyleTag({ path: path.join(xtermRoot, "css", "xterm.css") });
  await page.addScriptTag({ path: path.join(xtermRoot, "lib", "xterm.js") });
  await page.evaluate(({ cols, rows }) => {
    window.term = new Terminal({
      cols,
      rows,
      cursorBlink: false,
      fontFamily: "Menlo, Monaco, 'Courier New', monospace",
      fontSize: 15,
      lineHeight: 1.12,
      theme: {
        background: "#0f172a",
        foreground: "#d8dee9",
        black: "#111827",
        red: "#ff6b6b",
        green: "#7ddf64",
        yellow: "#fbbf24",
        blue: "#60a5fa",
        magenta: "#f472b6",
        cyan: "#2dd4bf",
        white: "#e5e7eb",
        brightBlack: "#64748b",
        brightRed: "#ff8787",
        brightGreen: "#a3e635",
        brightYellow: "#fde047",
        brightBlue: "#93c5fd",
        brightMagenta: "#f9a8d4",
        brightCyan: "#67e8f9",
        brightWhite: "#f8fafc",
      },
    });
    window.term.open(document.getElementById("terminal"));
    window.writeAnsi = (chunk) => window.term.write(chunk);
    window.terminalText = () => {
      const buffer = window.term.buffer.active;
      const lines = [];
      for (let index = 0; index < buffer.length; index += 1) {
        const line = buffer.getLine(index);
        if (line) lines.push(line.translateToString(true));
      }
      return lines.join("\n");
    };
  }, { cols: options.cols, rows: options.rows });

  const write = (data) => {
    page.evaluate((chunk) => window.writeAnsi(chunk), data).catch(() => {});
  };
  const child = startTui(options, write);

  if (options.waitFor) {
    try {
      await page.waitForFunction(
        (needle) => window.terminalText && window.terminalText().includes(needle),
        options.waitFor,
        { timeout: options.timeout },
      );
    } catch (error) {
      const text = await page.evaluate(() => window.terminalText && window.terminalText());
      console.error(text);
      throw error;
    }
  } else {
    await page.waitForTimeout(options.timeout);
  }

  await page.waitForTimeout(300);
  await page.locator("#shot").screenshot({ path: options.output });
  child.write(options.quit);
  child.kill();
  await browser.close();
  console.log(`wrote ${options.output}`);
}

function startTui(options, write) {
  const env = terminalEnv();
  if (pty) {
    try {
      const child = pty.spawn(options.command[0], options.command.slice(1), {
        cols: options.cols,
        rows: options.rows,
        cwd: ROOT,
        name: "xterm-256color",
        env,
      });
      child.onData(write);
      return {
        write: (data) => child.write(data),
        kill: () => child.kill(),
      };
    } catch (error) {
      console.error(`node-pty failed, falling back to script(1): ${error.message}`);
    }
  }

  const child = spawn(
    "script",
    [
      "-q",
      "/dev/null",
      "/bin/sh",
      "-c",
      `stty cols ${options.cols} rows ${options.rows}; exec "$@"`,
      "capture",
      ...options.command,
    ],
    {
      cwd: ROOT,
      env,
      stdio: ["inherit", "pipe", "pipe"],
    },
  );
  child.stdout.on("data", (data) => write(data.toString("utf8")));
  child.stderr.on("data", (data) => write(data.toString("utf8")));
  return {
    write: () => {},
    kill: () => child.kill(),
  };
}

function terminalEnv() {
  const env = { ...process.env, TERM: "xterm-256color", COLORTERM: "truecolor" };
  delete env.NO_COLOR;
  return env;
}

function parseArgs(argv) {
  const result = {
    output: path.join(ROOT, "docs/tui-screenshot.png"),
    cols: 120,
    rows: 34,
    width: 1240,
    height: 820,
    scale: 2,
    timeout: 5000,
    waitFor: "",
    quit: "q",
    command: [],
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--") {
      result.command = argv.slice(index + 1);
      break;
    }
    if (arg === "--output") result.output = path.resolve(argv[++index]);
    else if (arg === "--cols") result.cols = Number(argv[++index]);
    else if (arg === "--rows") result.rows = Number(argv[++index]);
    else if (arg === "--width") result.width = Number(argv[++index]);
    else if (arg === "--height") result.height = Number(argv[++index]);
    else if (arg === "--scale") result.scale = Number(argv[++index]);
    else if (arg === "--timeout") result.timeout = Number(argv[++index]);
    else if (arg === "--wait-for") result.waitFor = argv[++index];
    else if (arg === "--quit") result.quit = argv[++index];
    else usage(`unknown option: ${arg}`);
  }

  if (result.command.length === 0) usage("missing command after --");
  return result;
}

function usage(message) {
  if (message) console.error(message);
  console.error(`usage:
  scripts/capture-tui-screenshot.sh [options] -- <command> [args...]

options:
  --output <path>      default: docs/tui-screenshot.png
  --wait-for <text>    wait until the terminal buffer contains text
  --timeout <ms>       default: 5000
  --cols <n>           default: 120
  --rows <n>           default: 34`);
  process.exit(2);
}

function html() {
  return `<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <style>
    html, body { margin: 0; background: #030712; }
    #shot { display: inline-block; padding: 20px; background: #030712; }
    #terminal {
      padding: 12px 14px;
      background: #0f172a;
      border: 1px solid #334155;
      border-radius: 10px;
      box-shadow: 0 20px 55px rgba(0, 0, 0, 0.45);
    }
    .xterm .xterm-viewport { overflow: hidden !important; }
  </style>
</head>
<body>
  <div id="shot"><div id="terminal"></div></div>
</body>
</html>`;
}
