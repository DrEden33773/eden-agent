# Third-party notices

The product is Apache-2.0. Dependencies retain their own licenses. Cargo.lock records the exact dependency graph. Assembled distributions include dependency license texts and their index in `third-party-licenses/`.

## Cordis

Cordis core is distributed under MIT. Cargo.lock selects crates.io package `cordis-core 0.1.3`, published from revision `fba0506191bfbc678d3646f338e42a94e0f6057b`.

The crates.io archive for 0.1.3 omits its license file. An unmodified copy is kept in `third-party/cordis-core-0.1.3-LICENSE` and packaged under `third-party-licenses/cordis-core-0.1.3/`, sourced from the [published revision's upstream LICENSE](https://github.com/dshbox/cordis-rs/blob/fba0506191bfbc678d3646f338e42a94e0f6057b/LICENSE). The packaging fallback applies only to this package version.

## FFF search and native dependencies

The bundled search plugin uses MIT-licensed `fff-search`, `fff-grep` and `fff-query-parser` 0.10.6, published from [c6013ba6a5918221b6c482486aca01acc0830825](https://github.com/dmtrKovalenko/fff/tree/c6013ba6a5918221b6c482486aca01acc0830825). Their crates.io archives omit the root license; the exact upstream MIT text is retained in `third-party/fff-0.10.6-LICENSE` and included for each package. The search scope exclusion list is adapted from that version's `src/ignore.rs` under the same license. No Pi implementation is incorporated.

Exact MIT license texts for `heed 0.22.1`, `heed-traits 0.20.0` and `heed-types 0.21.0` are retained under `third-party/` because their published archives omit them. `scripts/license_bundle.py` records the upstream commit links and version-specific fallbacks. It also chooses the offered Apache-2.0 license for glidesort and includes Apache-2.0 for the LMDB wrapper.

The assembled native license bundle additionally retains LMDB's OpenLDAP license and copyright notice, libgit2's complete COPYING text (including its GPLv2 linking exception and embedded dependency notices) and AUTHORS, and zlib's license from their vendored source directories. Consult the distribution's `third-party-licenses/index.json` for exact versions and source paths.
