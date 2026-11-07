// Public-build cache poisoning and a history bootstrap replay, using only
// disposable browser profiles and the test fleet.
import { createServer } from "node:http";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import { loadEnv } from "./fleet";
import { launchUi } from "./open";

test("a persistent poisoned asset cache cannot execute under the real index", async ({
  page,
}) => {
  const probeUrl = new URL(loadEnv().url);
  probeUrl.hostname = "127.0.0.1";
  const probe = probeUrl.origin;
  const html = await (await fetch(probe)).text();
  const script = /src="(\/assets\/[^"?#]+\.js)"/.exec(html)?.[1];
  if (script === undefined) throw new Error("No built script");
  const evil = 'document.documentElement.dataset.poisoned = "yes";';
  const attacker = createServer((req, res) => {
    if (req.url === script) {
      res.writeHead(200, {
        "content-type": "text/javascript",
        "cache-control": "public, max-age=31536000, immutable",
      });
      res.end(evil);
    } else {
      res.writeHead(200, {
        "content-type": "text/html",
        "cache-control": "no-store",
      });
      res.end(`<script type="module" crossorigin src="${script}"></script>`);
    }
  });
  await new Promise<void>((resolve) => {
    attacker.listen(0, "127.0.0.1", resolve);
  });
  const address = attacker.address();
  if (address === null || typeof address === "string")
    throw new Error("No port");
  const origin = `http://127.0.0.1:${address.port}`;
  const profile = mkdtempSync(join(tmpdir(), "vessel-cache-test-"));
  let context = await chromium.launchPersistentContext(profile, {
    headless: true,
  });
  try {
    const poisoned = await context.newPage();
    await poisoned.goto(origin);
    await expect(poisoned.locator("html")).toHaveAttribute(
      "data-poisoned",
      "yes",
    );
    await context.close();
    await new Promise<void>((resolve, reject) => {
      attacker.close((error) => {
        if (error) reject(error);
        else resolve();
      });
    });
    const real = await launchUi(page, address.port);
    expect(new URL(real.origin).port).toBe(String(address.port));
    context = await chromium.launchPersistentContext(profile, {
      headless: true,
    });
    const clean = await context.newPage();
    await clean.goto(origin); // no capability is needed for the asset boundary
    await expect(clean.locator("html")).not.toHaveAttribute("data-poisoned");
    const integrity = await clean
      .locator('script[src], link[rel="stylesheet"]')
      .evaluateAll((elements) =>
        elements.every(
          (element) =>
            element.getAttribute("integrity")?.startsWith("sha384-") === true,
        ),
      );
    expect(integrity).toBe(true);
    const asset = await fetch(`${origin}${script}`);
    expect(asset.headers.get("cache-control")).toBe("no-cache");
    const etag = asset.headers.get("etag");
    expect(etag).not.toBeNull();
    const revalidated = await fetch(`${origin}${script}`, {
      headers: { "If-None-Match": etag ?? "" },
    });
    expect(revalidated.status).toBe(304);
  } finally {
    await context.close();
    attacker.close();
    rmSync(profile, { recursive: true });
  }
});

test("history retains only a consumed code; reload keeps a tab session", async ({
  page,
  browser,
}) => {
  const { url, origin } = await launchUi(page);
  // Avoid embedding capability values in assertion messages.
  try {
    await page.goto(url);
  } catch {
    throw new Error("UI navigation failed");
  }
  await expect(page.getByRole("navigation", { name: "Fleet" })).toBeVisible();
  expect(new URL(page.url()).hash === "").toBe(true);
  expect(
    await page.evaluate(() => localStorage.getItem("vessel.token") === null),
  ).toBe(true);
  expect(
    await page.evaluate(
      () => sessionStorage.getItem("vessel.session") !== null,
    ),
  ).toBe(true);
  await page.reload();
  await expect(page.getByRole("navigation", { name: "Fleet" })).toBeVisible();
  const other = await browser.newContext();
  try {
    const replay = await other.newPage();
    try {
      await replay.goto(url);
    } catch {
      throw new Error("Replay navigation failed");
    }
    await expect(
      replay.getByRole("heading", { name: /UI was restarted/ }),
    ).toBeVisible();
    await expect(replay.getByRole("navigation", { name: "Fleet" })).toHaveCount(
      0,
    );
    await page.goto(`${origin}/settings`);
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  } finally {
    await other.close();
  }
});

test("a new launch escapes cached HTML and a previous origin's service worker", async ({
  page,
}) => {
  const poison = `<!doctype html><html><body><script>
    document.documentElement.dataset.poisoned = "yes";
    document.documentElement.dataset.fragmentRead = String(location.hash.length > 0);
    navigator.serviceWorker.register('/sw.js');
  </script></body></html>`;
  const worker = `self.addEventListener('install', () => self.skipWaiting());
    self.addEventListener('activate', event => event.waitUntil(self.clients.claim()));
    self.addEventListener('fetch', event => event.respondWith(new Response(${JSON.stringify(poison)}, { headers: { 'content-type': 'text/html' } })));`;
  const attacker = createServer((req, res) => {
    const script = req.url === "/sw.js";
    res.writeHead(200, {
      "content-type": script ? "text/javascript" : "text/html",
      "cache-control": script
        ? "no-store"
        : "public, max-age=31536000, immutable",
    });
    res.end(script ? worker : poison);
  });
  await new Promise<void>((resolve) => {
    attacker.listen(0, "127.0.0.1", resolve);
  });
  const address = attacker.address();
  if (address === null || typeof address === "string")
    throw new Error("No port");
  const old = new URL(loadEnv().url);
  old.port = String(address.port);
  old.hash = "";
  const profile = mkdtempSync(join(tmpdir(), "vessel-origin-test-"));
  let context = await chromium.launchPersistentContext(profile, {
    headless: true,
  });
  try {
    const priming = await context.newPage();
    await priming.goto(old.origin);
    await expect(priming.locator("html")).toHaveAttribute(
      "data-poisoned",
      "yes",
    );
    await priming.evaluate(async () => {
      await navigator.serviceWorker.ready;
    });
    await priming.waitForFunction(
      () => navigator.serviceWorker.controller !== null,
    );
    await context.close();
    await new Promise<void>((resolve, reject) => {
      attacker.close((error) => {
        if (error) reject(error);
        else resolve();
      });
    });
    const fresh = await launchUi(page, address.port);
    context = await chromium.launchPersistentContext(profile, {
      headless: true,
    });
    const clean = await context.newPage();
    try {
      await clean.goto(fresh.url);
    } catch {
      throw new Error("Fresh-origin navigation failed");
    }
    await expect(clean.locator("html")).not.toHaveAttribute("data-poisoned");
    await expect(
      clean.getByRole("navigation", { name: "Fleet" }),
    ).toBeVisible();
    expect(new URL(fresh.origin).hostname !== old.hostname).toBe(true);
    expect(
      await clean.evaluate(() => navigator.serviceWorker.controller === null),
    ).toBe(true);
  } finally {
    await context.close();
    attacker.close();
    rmSync(profile, { recursive: true });
  }
});
