// Agent prose is untrusted. Only absolute web URLs become links; opening
// a new tab keeps the authenticated UI in place and isolates the opener.
import ReactMarkdown from "react-markdown";

import { isWebUrl } from "./links";

export function Markdown({ children }: { children: string }) {
  return (
    <ReactMarkdown
      skipHtml
      components={{
        a: ({ href, children: label }) =>
          href !== undefined && isWebUrl(href) ? (
            <a href={href} target="_blank" rel="noopener noreferrer">
              {label}
            </a>
          ) : (
            <>{label}</>
          ),
      }}
    >
      {children}
    </ReactMarkdown>
  );
}
