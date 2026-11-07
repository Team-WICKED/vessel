// `npm run screens`: every main screen, captured into artifacts/screens/
// (untracked) from a real fleet with some work on it, for review. Not a
// test of behaviour (app.spec.ts is); each step only waits for what the
// picture needs.

import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { hostname } from "node:os";

import { expect, type Page, test } from "@playwright/test";

import { openUi } from "./open";
import { type FleetEnv, loadEnv } from "./fleet";

const OUT =
  process.env.VESSEL_SCREEN_DIR ??
  join(import.meta.dirname, "..", "artifacts", "screens");

async function shot(page: Page, name: string): Promise<void> {
  mkdirSync(OUT, { recursive: true });
  // Let fonts and the last frames settle.
  await page.evaluate(async () => document.fonts.ready);
  await page.waitForTimeout(400);
  // Fleet join currently labels its device with the host name. Do not
  // publish the build machine's identity in otherwise synthetic fixtures.
  await page.screenshot({
    path: join(OUT, `${name}.png`),
    mask: [page.getByText(hostname(), { exact: true })],
  });
}

/** Run `vessel` as the user of this fleet would; `data` is the local data
 * directory (a node's own, for the commands that act on this machine's
 * node, such as `fleet init` and `fleet join`). */
function cli(
  env: FleetEnv,
  args: string[],
  input = "",
  data = join(env.root, "ui-data"),
): string {
  return execFileSync(env.vessel, args, {
    input,
    encoding: "utf8",
    env: {
      PATH: process.env.PATH,
      HOME: env.root,
      USERPROFILE: env.root,
      VESSEL_CONFIG_DIR: join(env.root, "config"),
      VESSEL_DATA_DIR: data,
    },
  });
}

/** Make laptop the fleet's home, with gpu-box and build-server joined. */
async function fleetHome(env: FleetEnv): Promise<void> {
  const sources = join(env.root, "sources");
  mkdirSync(join(sources, "skills", "release-notes"), { recursive: true });
  writeFileSync(
    join(sources, "AGENTS.md"),
    "# House rules\n\nSmall commits. Tests first.\n",
  );
  writeFileSync(
    join(sources, "skills", "release-notes", "SKILL.md"),
    "---\nname: release-notes\n---\nWrite them.\n",
  );
  const laptop = env.nodes.laptop;
  if (laptop === undefined) {
    return;
  }
  cli(
    env,
    [
      "fleet",
      "init",
      "--node",
      "laptop",
      "--instructions",
      join(sources, "AGENTS.md"),
      "--skills",
      join(sources, "skills"),
    ],
    "",
    laptop.data,
  );
  // The home publishes its first generation within a few seconds.
  await new Promise((resolve) => setTimeout(resolve, 3000));
  for (const member of ["gpu-box", "build-server"]) {
    const node = env.nodes[member];
    if (node === undefined) {
      continue;
    }
    const invite = cli(
      env,
      ["fleet", "invite", "--node", "laptop"],
      "",
      laptop.data,
    );
    const link = /vessel:\/\/pair#\S+/.exec(invite)?.[0];
    if (link !== undefined) {
      cli(
        env,
        ["fleet", "join", "--node", member, "--yes"],
        `${link}\n`,
        node.data,
      );
    }
  }
}

async function newSession(
  page: Page,
  node: string,
  mode: "Structured" | "Terminal",
  cwd: string,
  route = "",
): Promise<void> {
  await page.getByRole("button", { name: "New", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New session" });
  await dialog.getByLabel("Node").selectOption({ label: `${node} · serving` });
  await dialog.getByLabel("Harness").selectOption("claude-code");
  if (route !== "") {
    // Picks the profile on the route (e2e/fleet.ts: claude-qwen).
    await dialog.getByLabel("Model").selectOption(route);
  }
  await dialog.getByLabel("Working directory").fill(cwd);
  // The radio sits inside its segment's label, which is what a person clicks.
  await dialog.locator(".seg label").filter({ hasText: mode }).click();
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(dialog).toBeHidden();
}

test("every main screen", async ({ page }) => {
  test.setTimeout(240_000);
  const env = loadEnv();

  try {
    await fleetHome(env);
  } catch {
    throw new Error("Fleet sync setup failed");
  }
  const origin = await openUi(page);

  // Some work on the fleet: a structured session on laptop's model route
  // with two model calls and a pending permission, terminals on gpu-box
  // and build-server.
  const project = join(env.root, "laptop-home");
  await newSession(page, "gpu-box", "Terminal", join(env.root, "gpu-box-home"));
  const terminal = page.getByLabel("Session terminal");
  await expect(terminal).toContainText("ready");
  await terminal.click();
  // Seed screenshots only after the PTY is ready and acknowledges each
  // command. A screenshot setup should not race a new harness's startup.
  await page.keyboard.insertText("cwd");
  await expect(terminal).toContainText("cwd");
  await page.keyboard.press("Enter");
  await expect(terminal).toContainText("cwd /");
  await page.keyboard.insertText("args");
  await expect(terminal).toContainText("args");
  await page.keyboard.press("Enter");
  await expect(terminal).toContainText("args [");
  await newSession(
    page,
    "build-server",
    "Terminal",
    join(env.root, "build-server-home"),
  );
  await newSession(page, "laptop", "Structured", project, "qwen-coder");
  const prompt = page.getByRole("textbox", { name: "Prompt" });
  await expect(page.getByText(/Ready · harness session/)).toBeVisible();
  // `model <text>` is one call through laptop's proxy to fake-provider.
  for (const question of [
    "What does the fleet overview's generation column mean?",
    "Which member reported last?",
  ]) {
    await prompt.fill(`model ${question}`);
    await prompt.press("Control+Enter");
    await expect(
      page.getByText(`model fake reply to: ${question}`),
    ).toBeVisible();
  }
  await expect(page.locator(".t-item.model-call")).toHaveCount(2);
  await prompt.fill(
    "The fleet overview draws stale generation numbers. Check how members report what they applied, then fix the display.",
  );
  await prompt.press("Control+Enter");
  await expect(page.locator(".perm-sticky")).toBeVisible();
  await shot(page, "03-session-structured");
  const structured = page.url();
  // The same session scrolled back to its model calls.
  await page
    .locator(".t-item.model-call")
    .first()
    .evaluate((row) => {
      row.scrollIntoView({ block: "start" });
    });
  await shot(page, "03b-session-model-calls");

  await page.goto(`${origin}/fleet/laptop`);
  await expect(
    page.getByRole("complementary", { name: "laptop" }),
  ).toBeVisible();
  await shot(page, "01-fleet");

  await page.goto(`${origin}/sessions`);
  await expect(page.getByRole("table", { name: "Sessions" })).toBeVisible();
  await shot(page, "02-sessions");

  await page
    .getByRole("table", { name: "Sessions" })
    .getByRole("row")
    .filter({ hasText: "gpu-box" })
    .first()
    .click();
  await expect(page.getByLabel("Session terminal")).toContainText("args [");
  await shot(page, "04-session-terminal");

  await page.getByRole("button", { name: "New", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New session" });
  await dialog.getByLabel("Node").selectOption({ label: "laptop · serving" });
  await dialog.getByLabel("Harness").selectOption("claude-code");
  await dialog.getByLabel("Model").selectOption("qwen-coder");
  await expect(dialog.getByLabel("Profile")).toHaveValue("claude-qwen");
  await shot(page, "05-new-session");
  await page.keyboard.press("Escape");

  await page.goto(`${origin}/sync?node=laptop`);
  // Members report what they applied once they have pulled it.
  for (let attempt = 0; attempt < 20; attempt += 1) {
    await page.waitForTimeout(1000);
    if ((await page.getByText("current").count()) >= 2) {
      break;
    }
    await page.reload();
  }
  await shot(page, "06-sync");

  await page.goto(`${origin}/devices?node=laptop`);
  await page.getByLabel("Label").fill("phone");
  await page.getByRole("button", { name: "New link" }).click();
  await page.getByRole("button", { name: "Show pairing link" }).click();
  await expect(
    page.getByRole("img", { name: "Pairing link as a QR code" }),
  ).toBeVisible();
  const link = (await page.locator(".link-box .link-text").textContent()) ?? "";
  try {
    const phone = join(env.root, "phone");
    execFileSync(env.vessel, ["nodes", "add", "laptop", "--pair", "--yes"], {
      input: `${link}\n`,
      env: {
        PATH: process.env.PATH,
        HOME: phone,
        USERPROFILE: phone,
        VESSEL_CONFIG_DIR: join(phone, "config"),
      },
    });
  } catch {
    throw new Error("Device pairing failed");
  }
  await page.getByLabel("Label").fill("tablet");
  await page.getByRole("button", { name: "New link" }).click();
  await page.waitForTimeout(800);
  await expect(page.locator(".link-text")).toHaveCount(0);
  await expect(
    page.getByRole("img", { name: "Pairing link as a QR code" }),
  ).toHaveCount(0);
  await shot(page, "07-devices");

  await page.goto(`${origin}/settings`);
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  await shot(page, "08-settings");

  // The light theme, re-derived (DESIGN.md §4.2).
  await page.locator(".seg label").filter({ hasText: "Light" }).click();
  await page.goto(`${origin}/fleet/laptop`);
  await expect(
    page.getByRole("complementary", { name: "laptop" }),
  ).toBeVisible();
  await shot(page, "09-fleet-light");
  await page.goto(structured);
  await expect(page.locator(".perm-sticky")).toBeVisible();
  await shot(page, "10-session-structured-light");
  await page.goto(`${origin}/sessions`);
  await expect(page.getByRole("table", { name: "Sessions" })).toBeVisible();
  await shot(page, "11-sessions-light");
});
