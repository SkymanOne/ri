# Vendored packages

The modules in this directory are esbuild bundles of npm packages that pi
extensions import. They are generated, not edited; the bundles share chunks so
each package is included once.

| Package | Version | License | Copyright |
|---|---|---|---|
| `@earendil-works/pi-tui` | 1.0.0 | MIT | Copyright (c) 2025 Mario Zechner |
| `typebox` | 1.3.27 | MIT | Copyright (c) 2017-2026 Haydn Paterson |
| `marked` (bundled by pi-tui) | 18.0.11 | MIT | Copyright (c) 2018+, MarkedJS; Copyright (c) 2011-2018, Christopher Jeffrey |
| `get-east-asian-width` (bundled by pi-tui) | 1.6.0 | MIT | Copyright (c) Sindre Sorhus |

The facades in `../pi` port small parts of pi-ai and pi-coding-agent 1.0.0
(MIT, Copyright (c) 2025 Mario Zechner).

## MIT License

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
