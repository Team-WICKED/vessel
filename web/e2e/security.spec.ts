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

