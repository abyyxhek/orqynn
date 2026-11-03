# Third-Party Licenses

Orqyn itself is MIT-licensed; its notice lives in [`LICENSE`](LICENSE). Orqyn
communicates, as separate and independent works, with two upstream open-source
projects over the Model Context Protocol. Neither project's source is included
in, linked into, or derived from by this repository; both are run as separate
processes.

<!-- An earlier revision reproduced Orqyn's own MIT notice here under the heading
     "orqyn". It was removed because Orqyn is not a third party to itself, and a
     duplicate notice can drift out of sync with LICENSE. -->

## ai-memory (akitaonrails/ai-memory) — v2.4.0

Used as the **memory and retrieval substrate**. Orqyn consumes its
`memory_*` MCP tools (wiki read/write, FTS5/vector retrieval, handoff records,
hook-captured observations). No source from ai-memory is copied or linked; it
is run as a separate server process and spoken to over MCP.

```
MIT License

Copyright (c) 2026 Fabio Akita

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## handoff-mcp (alphaelements/handoff-mcp) — v0.35.1

Used as the **task, session, and agent substrate**. Orqyn consumes its
`handoff_*` MCP tools (task CRUD, dependencies, agent registry with heartbeat,
claim leases, session state machine). No source from handoff-mcp is copied or
linked; it is run as a separate stdio MCP server per project and spoken to over
MCP. Its Claude/Codex skills and JS workflow libraries are deliberately **not**
installed — Orqyn drives the task loop itself through its own harness driver,
not through handoff-mcp's bundled workflow assets.

```
MIT License

Copyright (c) 2026 Fabio Akita

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Attribution note

Both upstream projects are © 2026 Fabio Akita and are MIT-licensed, as recorded
in their respective repositories at the versions audited in
`docs/PHASE0-FORENSICS.md`. Orqyn is an independent work that
interoperates with them over MCP; it is not a derivative of either project's
source code.
