import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import { Markdown } from "./markdown";

// Keep this fixture at the rendering boundary, including encoded schemes.
test("agent links cannot navigate the UI tab", () => {
  const html = renderToStaticMarkup(
    <Markdown>
      {
        "[web](https://example.com) [mail](mailto:a@example.com) [relative](/settings) [scheme](javascript:alert%281%29) [encoded](java&#x73;cript:alert%281%29)"
      }
    </Markdown>,
  );
  expect(html).toContain('target="_blank"');
  expect(html).toContain('rel="noopener noreferrer"');
  expect(html.match(/<a /g)).toHaveLength(1);
});
