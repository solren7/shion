// Markdown → sanitized HTML for agent replies.
//
// Agent output may include text pulled from web pages (an indirect-injection
// surface) and it goes into the DOM via dangerouslySetInnerHTML, so the parsed
// HTML is run through DOMPurify — scripts, event handlers, and injected markup
// are stripped while ordinary markdown formatting renders.

import DOMPurify from "dompurify";
import { marked } from "marked";

marked.setOptions({ gfm: true, breaks: true });

export function renderMarkdown(md: string): string {
  const html = marked.parse(md ?? "", { async: false }) as string;
  return DOMPurify.sanitize(html);
}
