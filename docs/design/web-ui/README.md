# Web UI design (milestone 7)

The visual source of truth for `vessel ui` (ADR 0017, plan
`docs/plans/backend-m7.md`), drafted by the design agent on 2026-11-07:

- `DESIGN.md`: principles, information architecture, tokens, components,
  interactions, accessibility.
- `tokens.css`: the token contract the app's styles are built on (dark by
  default, a light theme re-derived, not inverted).
- `*.html` with `app.css`: static mockups of the main screens. `app.css`
  only composes the mockups; the app builds its own components from
  `DESIGN.md` and `tokens.css`, not from it. The mockups load Pretendard
  from a CDN for preview; the app ships it in its bundle.

Corrections the coordinator made on review, which the app follows and the
mockups may not yet show:

1. The sans stack is Pretendard, the system UI fonts, then `sans-serif`:
   no IBM Plex (the operator's font rule; fixed in `tokens.css` and
   `DESIGN.md` here).
2. The fleet detail panel lays keys and values out as a two-column `dl`
   grid, aligned.
3. A tool call's path is truncated from the left, keeping the file name in
   view, with the full path on hover and focus.
4. The user's prompt card in the structured timeline is denser (less
   padding), per "calm density".
5. The light theme is built, not only dark.

Screenshots of the mockups are not kept in the repository; the app's own
screens are captured by Playwright (`web/`, `npm run screens`).
