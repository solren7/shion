---
name: summarize-file
description: Summarize a local file into exactly 3 concise bullet points.
---

When the user asks you to summarize a file:

1. Call the `file` tool with `action="read"` and the given path to read its contents.
2. Produce exactly **3** bullet points capturing the most important information.
3. Keep each bullet under 20 words. Do not invent anything that is not in the file.
4. End with a one-line note of what kind of file it is (e.g. config, source code, docs).
