import { describe, expect, it, vi } from "vitest";

import { isWebUrl, openExternal } from "./links";

describe("links from agent output", () => {
  it("are only absolute http(s) URLs", () => {
    expect(isWebUrl("https://example.com/a?b=c")).toBe(true);
    expect(isWebUrl("HTTP://example.com")).toBe(true);
    for (const href of [
      "javascript:alert(1)",
      "mailto:someone@example.com",
      "//example.com/path",
      "/sessions/box/3f2a",
      "sessions/box",
      "data:text/html,<script>",
      "vessel://pair#abc",
      " https://example.com",
    ]) {
      expect(isWebUrl(href), href).toBe(false);
    }
  });

  it("open in a new isolated tab only after the operator saw the URL", () => {
    const asked: string[] = [];
    const ask = (message: string) => {
      asked.push(message);
      return true;
    };
    const open = vi.fn();
    expect(openExternal("https://example.com/x", ask, open)).toBe(true);
    expect(asked[0]).toContain("https://example.com/x");
    expect(open).toHaveBeenCalledWith("https://example.com/x");

    const declined = vi.fn();
    expect(openExternal("https://example.com/y", () => false, declined)).toBe(
      false,
    );
    expect(declined).not.toHaveBeenCalled();

    const never = vi.fn(() => true);
    const blocked = vi.fn();
    expect(openExternal("javascript:alert(1)", never, blocked)).toBe(false);
    expect(never).not.toHaveBeenCalled();
    expect(blocked).not.toHaveBeenCalled();
  });
});
