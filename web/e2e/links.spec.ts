// OSC 8 hyperlinks in a session's terminal follow the timeline's link
// policy (src/data/links.ts; verification finding 2, M7): a `javascript:`
// link does nothing; an http(s) link asks first, showing the URL, then
// opens a new tab that cannot reach this one; the UI tab never navigates.
// The http(s) target is answered by a route, so nothing leaves the machine.

import { expect, type Page, test } from "@playwright/test";

import { openUi } from "./open";

const ESC = "\x1b";
const BEL = "\x07";
const TARGET = "https://example.invalid/osc8";

/** The middle of the last occurrence of `text` in the terminal: the first
 * is the terminal's own echo of the typed line (escapes shown as ^[),
 * which is not a link; the last is the program's output. */
async function lastText(
  page: Page,
  text: string,
): Promise<{ x: number; y: number }> {
  const box = await page.evaluate((wanted) => {
    const rows = document.querySelector(".xterm-rows");
    if (rows === null) {
      return null;
    }
    const walker = document.createTreeWalker(rows, NodeFilter.SHOW_TEXT);
    let found: { x: number; y: number } | null = null;
    for (
      let node = walker.nextNode();
      node !== null;
      node = walker.nextNode()
    ) {
      const data = node.textContent ?? "";
      const at = data.lastIndexOf(wanted);
      if (at >= 0) {
        const range = document.createRange();
        range.setStart(node, at);
        range.setEnd(node, at + wanted.length);
        const rect = range.getBoundingClientRect();
        found = { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
      }
    }
    return found;
  }, text);
  if (box === null) {
    throw new Error(`${text} is not in the terminal`);
  }
  return box;
}

async function click(page: Page, at: { x: number; y: number }): Promise<void> {
  // xterm.js underlines and activates a link on hover, then click.
  await page.mouse.move(at.x - 30, at.y);
  await page.mouse.move(at.x, at.y, { steps: 5 });
  await page.waitForTimeout(300);
  await page.mouse.click(at.x, at.y);
}

test("terminal hyperlinks open only http(s), after a confirm, in an isolated tab", async ({
  page,
  context,
}) => {
  await context.route(`${TARGET}*`, (route) =>
    route.fulfill({ contentType: "text/html", body: "<title>landed</title>" }),
  );
  const dialogs: string[] = [];
  page.on("dialog", (dialog) => {
    dialogs.push(dialog.message());
    void dialog.accept();
  });

  await openUi(page);
  await page.getByRole("button", { name: "New", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New session" });
  await dialog.getByLabel("Node").selectOption({ label: "laptop · serving" });
  await dialog.getByLabel("Harness").selectOption("claude-code");
  await dialog.locator(".seg label").filter({ hasText: "Terminal" }).click();
  await dialog.getByRole("button", { name: "Create" }).click();
  const terminal = page.getByLabel("Session terminal");
  await expect(terminal).toContainText("ready");
  const ui = page.url();
  const title = await page.title();

  // `fake-tui` echoes the line, escapes and all: two OSC 8 links.
  await terminal.click();
  await page.keyboard.insertText(
    `${ESC}]8;;javascript:void(document.title='osc8-ran')${BEL}JSLINK${ESC}]8;;${BEL} ` +
      `${ESC}]8;;${TARGET}${BEL}HTTPLINK${ESC}]8;;${BEL} END`,
  );
  await page.keyboard.press("Enter");
  await expect(terminal).toContainText(/echo: .*END/);

  // javascript: nothing at all.
  const none = context
    .waitForEvent("page", { timeout: 1500 })
    .catch(() => null);
  await click(page, await lastText(page, "JSLINK"));
  expect(await none).toBeNull();
  expect(dialogs).toEqual([]);
  expect(await page.title()).toBe(title);

  // http(s): the URL is shown first, then a new tab without an opener.
  const opened = context.waitForEvent("page");
  await click(page, await lastText(page, "HTTPLINK"));
  const popup = await opened;
  await popup.waitForLoadState();
  expect(dialogs).toHaveLength(1);
  expect(dialogs[0]).toContain(TARGET);
  expect(popup.url()).toBe(TARGET);
  expect(await popup.evaluate(() => window.opener === null)).toBe(true);
  await popup.close();
  expect(page.url()).toBe(ui);
});
