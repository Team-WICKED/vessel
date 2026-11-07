// A small fleet for the end-to-end tests and the screenshots: real
// `vesseld` nodes in temporary directories with the testkit's fake
// harnesses (`fake-tui` as `claude`/`codex`, `fake-acp` as their ACP
// adapters), a node book naming them, and `vessel ui` serving the built
// page. Nothing touches the machine's own account: every process gets a
// temporary home, data and config directory, and the nodes listen on
// loopback ports chosen by the system.
//
// The Rust binaries are built beforehand (`cargo build -p vessel-cli -p
// vessel-node -p vessel-testkit --bins`, with the bundle embedded); their
// directory comes from `VESSEL_TARGET_DIR` or `cargo metadata`.

import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

export interface FleetEnv {
  /** The UI's address with the launch's token. */
  url: string;
  root: string;
  /** The `vessel` binary, for setting things up the way a user would. */
  vessel: string;
  pids: number[];
  nodes: Record<string, { port: number; data: string }>;
}

const EXE = process.platform === "win32" ? ".exe" : "";

function targetDir(): string {
  const given = process.env.VESSEL_TARGET_DIR;
  if (given !== undefined && given !== "") {
    return given;
  }
  const metadata = execFileSync(
    "cargo",
    ["metadata", "--format-version", "1", "--no-deps"],
    {
      cwd: join(import.meta.dirname, ".."),
      encoding: "utf8",
    },
  );
  const parsed = JSON.parse(metadata) as { target_directory: string };
  return parsed.target_directory;
}

function binary(name: string): string {
  const path = join(targetDir(), "debug", `${name}${EXE}`);
  if (!existsSync(path)) {
    throw new Error(`${path} is missing: build the Rust binaries first`);
  }
  return path;
}

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port =
        typeof address === "object" && address !== null ? address.port : 0;
      server.close(() => {
        resolve(port);
      });
    });
    server.on("error", reject);
  });
}

async function until(
  what: string,
  ready: () => boolean,
  ms = 30_000,
): Promise<void> {
  const deadline = Date.now() + ms;
  while (!ready()) {
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

/** The fake harnesses under the names a node looks harnesses up by. */
function installFakes(dir: string): void {
  mkdirSync(dir, { recursive: true });
  const tui = binary("fake-tui");
  const acp = binary("fake-acp");
  const links: [string, string][] = [
    ["claude", tui],
    ["claude-agent-acp", acp],
    ["codex", tui],
    ["codex-acp", acp],
  ];
  for (const [name, target] of links) {
    if (process.platform === "win32") {
      writeFileSync(
        join(dir, `${name}.cmd`),
        `@echo off\r\n"${target}" %*\r\nexit /b %ERRORLEVEL%\r\n`,
      );
    } else {
      symlinkSync(target, join(dir, name));
    }
  }
}

function spawnLogged(
  root: string,
  name: string,
  command: string,
  args: string[],
  env: NodeJS.ProcessEnv,
): ChildProcess {
  const child = spawn(command, args, {
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const log = join(root, `${name}.log`);
  child.stdout.on("data", (chunk: Buffer) => {
    writeFileSync(log, chunk, { flag: "a", mode: 0o600 });
  });
  child.stderr.on("data", (chunk: Buffer) => {
    writeFileSync(log, chunk, { flag: "a", mode: 0o600 });
  });
  return child;
}

/** Start the nodes and the UI. */
export async function startFleet(): Promise<FleetEnv> {
  const root = mkdtempSync(join(tmpdir(), "vessel-e2e-"));
  const fakes = join(root, "bin");
  installFakes(fakes);
  const config = join(root, "config");
  const uiData = join(root, "ui-data");
  mkdirSync(config, { recursive: true });
  mkdirSync(uiData, { recursive: true });
  // The fakes and the system's own directories only: a harness installed
  // for the machine's user (in ~/.local/bin, say) must not be found.
  const system =
    process.platform === "win32"
      ? `${process.env.SYSTEMROOT ?? "C:\\Windows"}\\System32`
      : "/usr/local/bin:/usr/bin:/bin";
  const base: NodeJS.ProcessEnv = {
    PATH: `${fakes}${process.platform === "win32" ? ";" : ":"}${system}`,
    SYSTEMROOT: process.env.SYSTEMROOT,
    RUST_LOG: "info",
  };
  const pids: number[] = [];
  const nodes: FleetEnv["nodes"] = {};
  for (const name of ["laptop", "gpu-box", "build-server"]) {
    const port = await freePort();
    const data = join(root, name);
    const home = join(root, `${name}-home`);
    mkdirSync(home, { recursive: true });
    const args = [
      "--data-dir",
      data,
      "--bind",
      `127.0.0.1:${port}`,
      "--name",
      name,
    ];
    if (name === "laptop") {
      // Pairing links name a direct route, so a device can pair here.
      args.push("--public", `127.0.0.1:${port}`);
    }
    const child = spawnLogged(root, name, binary("vesseld"), args, {
      ...base,
      HOME: home,
      USERPROFILE: home,
    });
    if (child.pid !== undefined) {
      pids.push(child.pid);
    }
    nodes[name] = { port, data };
    await until(`${name}'s token`, () => existsSync(join(data, "token")));
  }
  const vessel = binary("vessel");
  const cli = {
    ...base,
    HOME: root,
    USERPROFILE: root,
    VESSEL_CONFIG_DIR: config,
    VESSEL_DATA_DIR: uiData,
  };
  for (const [name, node] of Object.entries(nodes)) {
    const args = [
      "nodes",
      "add",
      name,
      `ws://127.0.0.1:${node.port}`,
      "--token-file",
      join(node.data, "token"),
    ];
    if (name === "laptop") {
      args.push("--default");
    }
    execFileSync(vessel, args, { env: cli });
  }
  const ui = spawnLogged(
    root,
    "ui",
    vessel,
    ["ui", "--no-open", "--port", "0"],
    cli,
  );
  if (ui.pid !== undefined) {
    pids.push(ui.pid);
  }
  const log = join(root, "ui.log");
  let url = "";
  await until("vessel ui's address", () => {
    const match = existsSync(log)
      ? /Vessel UI: (http:\/\/\S+)/.exec(readFileSync(log, "utf8"))
      : null;
    url = match?.[1] ?? "";
    return url !== "";
  });
  return { url, root, vessel, pids, nodes };
}

/** Stop a process of the fleet (to show a node going away). */
export function stopProcess(pid: number): void {
  try {
    process.kill(pid);
  } catch {
    // Already gone.
  }
}

export function stopFleet(env: FleetEnv): void {
  for (const pid of env.pids) {
    stopProcess(pid);
  }
  // Give the processes a moment to let go of their files.
  setTimeout(() => {
    rmSync(env.root, { recursive: true, force: true });
  }, 500);
}

const ENV_FILE = join(import.meta.dirname, "..", "artifacts", "fleet.json");

export function saveEnv(env: FleetEnv): void {
  mkdirSync(join(import.meta.dirname, "..", "artifacts"), { recursive: true });
  writeFileSync(ENV_FILE, JSON.stringify(env), { mode: 0o600 });
}

export function loadEnv(): FleetEnv {
  return JSON.parse(readFileSync(ENV_FILE, "utf8")) as FleetEnv;
}
