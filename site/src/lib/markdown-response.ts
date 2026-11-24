/** A raw guide page body served as Markdown, trimmed to one trailing newline. */
export function markdownResponse(body: string): Response {
  return new Response(`${body.trim()}\n`, {
    headers: { "Content-Type": "text/markdown; charset=utf-8" },
  });
}
