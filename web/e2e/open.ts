// Each independent browser context gets a fresh one-use UI launch. Nodes
// stay shared, but a consumed bootstrap must never be reset for a test.
import { spawn } from "node:child_process";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { expect, type Page } from "@playwright/test";
import { loadEnv } from "./fleet";

export async function launchUi(
  page: Page,
  port = 0,
): Promise<{ url: string; origin: string }> {
  const env = loadEnv();
  const child = spawn(env.vessel, ["ui", "--no-open", "--port", String(port)], {
    env: {
      PATH: process.env.PATH,
      SYSTEMROOT: process.env.SYSTEMROOT,
      HOME: env.root,
      USERPROFILE: env.root,
      VESSEL_CONFIG_DIR: join(env.root, "config"),
      VESSEL_DATA_DIR: join(env.root, "ui-data"),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  page.on("close", () => {
    child.kill();
  });
  const lines = createInterface({ input: child.stdout });
  const url = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("UI launch timed out"));
    }, 15_000);
    lines.on("line", (line) => {
      const match = /^Vessel UI: (http:\/\/\S+)$/.exec(line);
      if (match?.[1] !== undefined) {
        clearTimeout(timer);
        resolve(match[1]);
      }
    });
    child.on("exit", () => {
      clearTimeout(timer);
      reject(new Error("UI ended before startup"));
    });
    child.on("error", () => {
      clearTimeout(timer);
      reject(new Error("UI could not start"));
    });
  });
  return { url, origin: new URL(url).origin };
}

export async function openUi(page: Page): Promise<string> {
  const { url, origin } = await launchUi(page);
  try {
    // Do not let Playwright's navigation error echo a capability URL.
    await page.goto(url);
  } catch {
    throw new Error("Could not open the UI");
  }
  await expect(page.getByRole("navigation", { name: "Fleet" })).toBeVisible();
  return origin;
}
