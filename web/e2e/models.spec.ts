// Model routing in the UI (milestone 5): laptop's routes (e2e/fleet.ts)
// in the fleet panel and the New session picker, and a routed session's
// model calls, which go through the node's proxy to `fake-provider`.

import { expect, type Page, test } from "@playwright/test";

import { openUi } from "./open";

/** Open New session on laptop with Claude Code on route `qwen-coder`. */
async function routed(
  page: Page,
  mode: "Structured" | "Terminal",
): Promise<void> {
  await page.getByRole("button", { name: "New", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New session" });
  await dialog.getByLabel("Node").selectOption({ label: "laptop · serving" });
  await dialog.getByLabel("Harness").selectOption("claude-code");
  const model = dialog.getByLabel("Model");
  // A route no profile of this harness names is listed, not offered.
  await expect(
    model.getByRole("option", { name: /gpt-oss .* no profile/ }),
  ).toHaveAttribute("disabled", "");
  await model.selectOption("qwen-coder");
  // Picking the route picks the profile on it, and the other way round.
  await expect(dialog.getByLabel("Profile")).toHaveValue("claude-qwen");
  await expect(dialog).toContainText(
    "Through profile claude-qwen, pinned to qwen/qwen3-coder and qwen/qwen3-coder-flash.",
  );
  await dialog.getByLabel("Profile").selectOption("");
  await expect(model).toHaveValue("");
  await dialog.getByLabel("Profile").selectOption("claude-qwen");
  await expect(model).toHaveValue("qwen-coder");
  await dialog.locator(".seg label").filter({ hasText: mode }).click();
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(dialog).toBeHidden();
}

test("a node's routes and its profiles' routes show in its panel", async ({
  page,
}) => {
  const origin = await openUi(page);
  await page.goto(`${origin}/fleet/laptop`);
  const panel = page.getByRole("complementary", { name: "laptop" });
  await expect(panel).toContainText("Model routes");
  await expect(panel).toContainText("openrouter · qwen/qwen3-coder");
  await expect(panel).toContainText("small qwen/qwen3-coder-flash");
  await expect(panel).toContainText("ollama · gpt-oss:20b");
  await expect(panel).toContainText("claude-qwen → qwen-coder");
  // A node without routes has no such section.
  await page.goto(`${origin}/fleet/gpu-box`);
  await expect(
    page.getByRole("complementary", { name: "gpu-box" }),
  ).not.toContainText("Model routes");
});

test("a routed structured session lists its model calls", async ({ page }) => {
  await openUi(page);
  await routed(page, "Structured");
  await expect(page.locator(".session-head .meta")).toContainText(
    "claude-qwen · qwen-coder (qwen/qwen3-coder)",
  );
  await expect(page.getByText(/Ready · harness session/)).toBeVisible();
  const prompt = page.getByRole("textbox", { name: "Prompt" });
  await prompt.fill("model what is a vessel?");
  await prompt.press("Control+Enter");
  await expect(
    page.getByText("model fake reply to: what is a vessel?"),
  ).toBeVisible();
  const call = page.locator(".t-item.model-call");
  await expect(call).toHaveCount(1);
  // fake-provider reports 12 prompt and 5 completion tokens.
  await expect(call).toContainText(
    /Model call · qwen-coder · qwen\/qwen3-coder · 12 in · 5 out · end turn · \d+ ms/,
  );
  const inspector = page.getByRole("complementary", {
    name: "Session inspector",
  });
  await expect(inspector).toContainText("Model calls");
  await expect(inspector).toContainText("calls1");
  await expect(inspector).toContainText("routeqwen-coder");
  await expect(page.locator(".composer-bar .model")).toHaveText(
    "qwen/qwen3-coder",
  );
});

test("a routed terminal session counts its model calls", async ({ page }) => {
  await openUi(page);
  await routed(page, "Terminal");
  const terminal = page.getByLabel("Session terminal");
  await expect(terminal).toContainText("ready");
  await terminal.click();
  await page.keyboard.type("model hello\r");
  await expect(terminal).toContainText("model fake reply to: hello");
  await expect(page.locator(".term-chrome .calls")).toHaveText(
    "1 model call · 12 in · 5 out",
  );
});
