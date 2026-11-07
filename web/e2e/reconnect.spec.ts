// A tab whose connection to `vessel ui` keeps dropping stops redialling
// and says so, but keeps its session key: Retry, or a reload, resumes the
// same launch (verification finding 1, M7). The connection is dropped
// by routing the page's websocket, so the UI itself never goes away.

import { expect, test, type WebSocketRoute } from "@playwright/test";

import { openUi } from "./open";

test("a tab that lost its connection recovers with Retry and after a reload", async ({
  page,
}) => {
  let failing = false;
  const live: WebSocketRoute[] = [];
  await page.routeWebSocket(/\/api\/ws$/, (socket) => {
    if (failing) {
      void socket.close();
      return;
    }
    socket.connectToServer();
    live.push(socket);
  });
  const drop = async () => {
    failing = true;
    for (const socket of live.splice(0)) {
      await socket.close();
    }
  };
  const connected = page.getByText("vessel ui · connected");
  const disconnected = page.getByText(
    /Disconnected — vessel ui may have restarted/,
  );

  await openUi(page);
  await expect(connected).toBeVisible();

  // Five failed dials (about 7.5 s of backoff), then it stops and says so.
  await drop();
  await expect(disconnected).toBeVisible({ timeout: 30_000 });
  await expect(page.getByText("vessel ui · disconnected")).toBeVisible();
  failing = false;
  await page.getByRole("button", { name: "Retry" }).click();
  await expect(connected).toBeVisible();
  await expect(disconnected).toBeHidden();
  await expect(page.getByRole("table", { name: "Nodes" })).toBeVisible();

  // Again, and this time a reload: the key in this tab survived.
  await drop();
  await expect(disconnected).toBeVisible({ timeout: 30_000 });
  failing = false;
  await page.reload();
  await expect(connected).toBeVisible();
  await expect(page.getByRole("table", { name: "Nodes" })).toBeVisible();
});
