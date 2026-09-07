# Vendored browser libraries

Embedded into `conchd` and served under `/ui/vendor/`; nothing is loaded from a CDN.

| File | Package | Version | License |
| --- | --- | --- | --- |
| `marked.umd.js` | [marked](https://github.com/markedjs/marked) | 18.0.11 | MIT (`marked.LICENSE`) |
| `purify.min.js` | [DOMPurify](https://github.com/cure53/DOMPurify) | 3.4.15 | Apache-2.0 (`purify.LICENSE`) |

Both come straight out of the npm tarballs (`npm pack marked@18.0.11 dompurify@3.4.15`), which npm verifies
against the registry integrity hash. Bump by repeating that and updating this table and the version marker
asserted in `crates/conchd/tests/http.rs`.
