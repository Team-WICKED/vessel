// The UI end to end, against real nodes running the fake harnesses
// (e2e/fleet.ts): what the plan's B7 lists.

import { execFileSync } from "node:child_process";
import { join } from "node:path";

import { expect, type Page, test } from "@playwright/test";

import { openUi } from "./open";
import { loadEnv } from "./fleet";

async function open(page: Page): Promise<void> {
  await openUi(page);
}

/** Create a session from the New session dialog. */
async function create(
  page: Page,
  node: string,
  mode: "Structured" | "Terminal",
): Promise<void> {
  await page.getByRole("button", { name: "New", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New session" });
  await dialog.getByLabel("Node").selectOption({ label: `${node} · serving` });
  await dialog.getByLabel("Harness").selectOption("claude-code");
  // The radio sits inside its segment's label, which is what a person clicks.
  await dialog.locator(".seg label").filter({ hasText: mode }).click();
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(dialog).toBeHidden();
}

test("the fleet lists the book's nodes with what they report", async ({
  page,
}) => {
  await open(page);
  const nodes = page.getByRole("table", { name: "Nodes" });
  for (const name of ["laptop", "gpu-box", "build-server"]) {
    await expect(
      nodes.getByRole("row", { name: new RegExp(name) }),
    ).toContainText("serving");
  }
  // This machine's node has no token here: listed, not connected.
  await expect(nodes.getByRole("row", { name: /this machine/ })).toContainText(
    "offline",
  );
  await nodes.getByRole("row", { name: /gpu-box/ }).click();
  const panel = page.getByRole("complementary", { name: "gpu-box" });
  await expect(panel).toContainText("protocol");
  await expect(panel).toContainText("claude");
});

test("a terminal session runs and echoes keys", async ({ page }) => {
  await open(page);
  await create(page, "laptop", "Terminal");
  const terminal = page.getByLabel("Session terminal");
  await expect(terminal).toContainText("ready");
  await terminal.click();
  await page.keyboard.type("hello vessel\r");
  await expect(terminal).toContainText("echo: hello vessel");
});

test("a structured session runs a turn and answers its permission", async ({
  page,
}) => {
  await open(page);
  await create(page, "gpu-box", "Structured");
  await expect(page.getByText(/Ready · harness session/)).toBeVisible();
  const prompt = page.getByRole("textbox", { name: "Prompt" });
  await prompt.fill("tidy the overview");
  await prompt.press("Control+Enter");
  await expect(page.getByText("Run fake tool").first()).toBeVisible();
  const waiting = page.locator(".perm-sticky");
  await expect(waiting).toContainText("Waiting on permission");
  await waiting.getByRole("button", { name: "Allow" }).click();
  await expect(waiting).toBeHidden();
  await expect(page.getByText(/Turn ended · end turn/)).toBeVisible();
  await expect(page.getByText("Permission · Allow")).toBeVisible();
});

test("a session is killed", async ({ page }) => {
  await open(page);
  await create(page, "build-server", "Terminal");
  await expect(page.getByLabel("Session terminal")).toContainText("ready");
  await page.getByRole("button", { name: "Kill" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Kill" }).click();
  await expect(page.locator(".session-head .badge").first()).not.toHaveText(
    "running",
  );
});

test("a device is invited, pairs, and is revoked", async ({ page }) => {
  const env = loadEnv();
  await open(page);
  await page.goto(`${new URL(page.url()).origin}/devices?node=laptop`);
  await page.getByLabel("Label").fill("phone");
  await page.getByRole("button", { name: "New link" }).click();
  await expect(
    page.getByRole("button", { name: "Show pairing link" }),
  ).toBeVisible();
  await expect(page.locator(".link-text")).toHaveCount(0);
  await expect(
    page.getByRole("img", { name: "Pairing link as a QR code" }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Show pairing link" }).click();
  await expect(
    page.getByRole("img", { name: "Pairing link as a QR code" }),
  ).toBeVisible();
  const link = (await page.locator(".link-box .link-text").textContent()) ?? "";
  expect(link.startsWith("vessel://pair#")).toBe(true);
  await page.context().grantPermissions(["clipboard-write"]);
  await page.getByRole("button", { name: "Copy pairing link" }).click();
  await expect(page.locator(".link-text")).toHaveCount(0);
  await expect(
    page.getByRole("img", { name: "Pairing link as a QR code" }),
  ).toHaveCount(0);
  // Another device pairs with the link, as `vessel nodes add --pair` does.
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
  await page.reload();
  const devices = page.getByRole("table", { name: "Paired devices" });
  const row = devices.getByRole("row").filter({ hasText: "phone" });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Revoke" }).click();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Revoke" })
    .click();
  await expect(row).toBeHidden();
});

test("terminal replay cannot answer device queries, while keys and paste still work", async ({
  page,
}) => {
  const env = loadEnv();
  await open(page);
  await create(page, "laptop", "Terminal");
  await expect(page.getByLabel("Session terminal")).toContainText("ready");
  const sessionUrl = page.url();
  const session = new URL(sessionUrl).pathname.split("/").at(-1);
  if (session === undefined) throw new Error("No session id");
  await page.goto(`${new URL(sessionUrl).origin}/fleet`);
  const esc = "\x1b";
  const bel = "\x07";
  const queries = `${esc}[c${esc}[6n${esc}]10;?${bel}${esc}P$qm${esc}\\REPLAY_END`;
  execFileSync(env.vessel, ["--node", "laptop", "send", session, queries], {
    env: {
      PATH: process.env.PATH,
      HOME: env.root,
      USERPROFILE: env.root,
      VESSEL_CONFIG_DIR: join(env.root, "config"),
      VESSEL_DATA_DIR: join(env.root, "ui-data"),
    },
  });
  let inputs = 0;
  page.on("websocket", (socket) => {
    socket.on("framesent", (event) => {
      const text =
        typeof event.payload === "string"
          ? event.payload
          : event.payload.toString("utf8");
      const frame = JSON.parse(text) as {
        type: string;
        call?: { method: string };
      };
      if (frame.type === "call" && frame.call?.method === "session.send")
        inputs += 1;
    });
  });
  await page.goto(sessionUrl);
  const terminal = page.getByLabel("Session terminal");
  await expect(terminal).toContainText("REPLAY_END");
  expect(inputs).toBe(0);
  await terminal.click();
  await page.keyboard.type("typed\r");
  await expect(terminal).toContainText("echo: typed");
  // Browser text insertion follows the same public input path as IME.
  await page.keyboard.insertText("한글 pasted");
  await page.keyboard.press("Enter");
  await expect(terminal).toContainText("echo: 한글 pasted");
  expect(inputs).toBeGreaterThan(0);
});
