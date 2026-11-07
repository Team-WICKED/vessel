// Links in agent output (Markdown in the timeline, OSC 8 hyperlinks in the
// terminal) are untrusted. One policy for both (security review, items 7
// and the verification's OSC 8 finding):
// - only absolute http(s) URLs are links; anything else (`javascript:`,
//   `mailto:`, `//host`, a relative path into this app) is not;
// - a link opens in a new tab with `noopener,noreferrer`, so the page it
//   opens cannot reach this one, and the UI tab is never navigated;
// - the terminal asks first, showing the whole URL (what xterm.js's own
//   handler did), since a click in a terminal is easy to make by accident.

/** Whether `href` may become a link: an absolute http(s) URL. */
export function isWebUrl(href: string): boolean {
  try {
    const url = new URL(href);
    return (
      (url.protocol === "http:" || url.protocol === "https:") &&
      /^https?:\/\//i.test(href)
    );
  } catch {
    return false;
  }
}

/** Open `href` in a new, isolated tab after the operator confirms it;
 * whether it was opened. Anything but an http(s) URL is never opened. */
export function openExternal(
  href: string,
  ask: (message: string) => boolean = (message) => window.confirm(message),
  open: (url: string) => void = (url) => {
    window.open(url, "_blank", "noopener,noreferrer");
  },
): boolean {
  if (
    !isWebUrl(href) ||
    !ask(`Open this link from the session in a new tab?\n\n${href}`)
  ) {
    return false;
  }
  open(href);
  return true;
}
