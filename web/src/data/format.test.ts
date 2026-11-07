import { describe, expect, it } from "vitest";

import { ago, harnessName, shortId, statusWord } from "./format";

describe("formatting", () => {
  it("keeps the protocol's words", () => {
    expect(statusWord({ state: "exited", code: 3 })).toBe("exited 3");
    expect(statusWord({ state: "exited", code: null })).toBe("exited");
    expect(statusWord({ state: "interrupted" })).toBe("interrupted");
    expect(harnessName("claude-code")).toBe("claude");
    expect(harnessName("open-code")).toBe("opencode");
  });

  it("shortens ids and times", () => {
    expect(shortId("3f2a9c10-0000-4000-8000-000000000000")).toBe("3f2a");
    const now = Date.parse("2026-11-07T12:00:00Z");
    expect(ago("2026-11-07T11:59:30Z", now)).toBe("30s");
    expect(ago("2026-11-07T11:46:00Z", now)).toBe("14m");
    expect(ago("2026-11-07T09:00:00Z", now)).toBe("3h");
    expect(ago("2026-11-01T12:00:00Z", now)).toBe("6d");
  });
});
