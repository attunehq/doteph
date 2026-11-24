# eph site

The source for [attunehq.github.io/doteph](https://attunehq.github.io/doteph):
a single-page explainer for what `eph` is and why you'd want it, plus the
hosted **user guide** under `/guide`.

It is a static [Astro](https://astro.build) site. The Rust crate in the
repository root is untouched by it; the Node toolchain is scoped entirely to
this directory.

## Develop

```sh
cd site
npm install
npm run dev      # http://localhost:4321/doteph
```

```sh
npm run build    # static output to site/dist (also builds the Pagefind search index)
npm run preview  # serve the production build locally
npm run check    # astro type/diagnostics check
```

Search is powered by [Pagefind](https://pagefind.app), whose index is
generated from the built HTML as a post-build step. It therefore only exists
after `npm run build`; under `npm run dev` the search box opens but reports
that the index has not been built yet.

## The guide is single-sourced from `docs/user-guide`

The site does not maintain its own copy of the guide. `src/content.config.ts`
defines a content collection glob-loaded straight from the repo-root
`docs/user-guide/*.md`, the same files people read on GitHub. Each chapter's
frontmatter (`title`, `summary`, `order`) is the entire contract this site
depends on:

- `order` drives reading order: the sidebar, prev/next navigation, and the
  `llms.txt` index all sort by it. `order: 0` is the guide's index page.
- `title` and `summary` drive page titles, meta descriptions, and the sidebar
  and `llms.txt` labels.

Chapter prose can be rewritten freely without touching this site; only the
frontmatter shape and `order` values are load-bearing.

- `src/lib/guide.ts` has the small helpers built on that collection:
  `routeFor`, `rawSlugFor`, `loadGuide`, `neighbors`.
- `src/lib/rehype-doc-links.mjs` (wired in `astro.config.mjs`) rewrites the
  guide's relative `.md` links at build time: links that stay inside the
  published guide become `/guide/*` routes, and links pointing elsewhere (the
  developer guide, the root README) become GitHub blob URLs.
- `src/layouts/Docs.astro` is the docs shell; `src/components/docs/` holds the
  header, sidebar, on-this-page TOC, prev/next, and per-page actions.
- `src/pages/guide/index.astro` and `src/pages/guide/[slug].astro` render the
  chapters; `src/styles/docs.css` styles the prose against the shared tokens.

## Agent / LLM surface

- Every guide page's raw Markdown lives at its page route plus `.md`
  (`/guide.md`, `/guide/concepts.md`): the representation agents probe for
  and the text the "Copy page" button copies. `markdownPathFor` in
  `src/lib/guide.ts` is the one place that rule is encoded;
  `src/pages/guide.md.ts` serves the index and `src/pages/guide/[slug].md.ts`
  serves the chapters (plus `/guide/index.md`, an alias kept for links
  published before `/guide.md` existed).
- GitHub Pages serves each page from a directory, so it 301s `/guide/concepts`
  to `/guide/concepts/`. The rule therefore reads "drop the trailing slash, add
  `.md`", and that is how `llms.txt` states it. Keep the two in sync if the
  output format or routing changes.
- `src/pages/llms.txt.ts` and `src/pages/llms-full.txt.ts` generate
  [`/llms.txt`](https://llmstxt.org) (a curated index that also states the
  `.md` rule) and `/llms-full.txt` (the whole guide concatenated for a single
  fetch). Every page, landing included, links `llms.txt` with
  `<link rel="alternate">`, and the guide pages also link their own `.md`.
- Outside the site, the shipped `using-eph` skill (`skills/using-eph`) and the
  top-level `eph --help` both point agents at `llms.txt`.

## The base path

GitHub Pages serves this as a project site rather than a custom domain, so it
lives at `attunehq.github.io/doteph` instead of a domain root. `astro.config.mjs`
sets `base: "/doteph"` (from `src/lib/base-path.mjs`, the single source of
truth for that value), and `src/lib/base.ts` exports a `withBase()` helper
that every hand-written internal href, the favicon link, the `llms.txt` link,
and the Pagefind bundle path all go through. `rehype-doc-links.mjs` (which
runs outside Vite and can't read `import.meta.env`) imports the same constant
directly. After changing any routing, grep the built output for stray
root-absolute links that escaped the base:

```sh
npm run build
grep -rEo '(href|src)="/[^"]*"' dist --include='*.html' | grep -v '="/doteph'
```

An empty result means every internal link is base-path-safe.

## Shared

- `src/styles/tokens.css` is the design system: colors (OKLCH), type scale,
  spacing, motion. `src/styles/global.css` is the reset, base type, and
  shared utilities.
- `public/` holds static assets: `favicon.svg` and nothing else. There is no
  `CNAME` (this deploys to the default project-pages URL, not a custom
  domain) and no `og.png` (the Open Graph image meta tags are omitted rather
  than pointed at a file that doesn't exist).

## Deploying

Pushes to `main` that touch `site/**` or `docs/user-guide/**` trigger
[`.github/workflows/site.yml`](../.github/workflows/site.yml), which builds
the site and deploys it to GitHub Pages at
[attunehq.github.io/doteph](https://attunehq.github.io/doteph). The only
one-time setup needed outside this repo is enabling **GitHub Actions** as the
Pages build source under the repo's **Settings -> Pages**.
