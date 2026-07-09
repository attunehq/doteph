// GitHub Pages serves this site at https://attunehq.github.io/doteph, so
// every root-relative href (`/guide`, `/favicon.svg`, `/llms.txt`, ...) needs
// that `/doteph` prefix or it 404s. Astro exposes the configured base as
// `BASE_URL`, but whether it carries a trailing slash depends on the base
// value itself (present for the default "/", absent for "/doteph"), so
// normalize it once here rather than re-deriving that at every call site.
const BASE = import.meta.env.BASE_URL.replace(/\/+$/, "");

/** Prefix a root-relative path ("/guide", "/llms.txt") with the deploy base. */
export function withBase(path: string): string {
  if (path === "" || path === "/") return BASE === "" ? "/" : BASE;
  const rel = path.startsWith("/") ? path : `/${path}`;
  return `${BASE}${rel}`;
}
