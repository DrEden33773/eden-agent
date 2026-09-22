# Third-party notices

The product is Apache-2.0. Dependencies retain their own licenses. Cargo.lock records the exact dependency graph. Assembled distributions include dependency license texts and their index in `third-party-licenses/`.

## Cordis

Cordis core is distributed under MIT. Cargo.lock selects crates.io package `cordis-core 0.2.10`, published from revision `ba4babd73996b0c8821128e5a561e407ae2682b7`.

The crates.io archive for 0.2.10 omits its license file. An unmodified copy is kept in `third-party/cordis-core-0.2.10-LICENSE` and packaged under `third-party-licenses/cordis-core-0.2.10/`, sourced from the [published revision's upstream LICENSE](https://github.com/dshbox/cordis-rs/blob/ba4babd73996b0c8821128e5a561e407ae2682b7/LICENSE). The packaging fallback applies only to this package version.

## FFF search and native dependencies

The bundled search plugin uses MIT-licensed `fff-search`, `fff-grep` and `fff-query-parser` 0.10.6, published from [c6013ba6a5918221b6c482486aca01acc0830825](https://github.com/dmtrKovalenko/fff/tree/c6013ba6a5918221b6c482486aca01acc0830825). Their crates.io archives omit the root license; the exact upstream MIT text is retained in `third-party/fff-0.10.6-LICENSE` and included for each package. The search scope exclusion list is adapted from that version's `src/ignore.rs` under the same license. No Pi implementation is incorporated.

Exact MIT license texts for `heed 0.22.1`, `heed-traits 0.20.0` and `heed-types 0.21.0` are retained under `third-party/` because their published archives omit them. `scripts/license_bundle.py` records the upstream commit links and version-specific fallbacks. It also chooses the offered Apache-2.0 license for glidesort and includes Apache-2.0 for the LMDB wrapper.

The assembled native license bundle additionally retains LMDB's OpenLDAP license and copyright notice, libgit2's complete COPYING text (including its GPLv2 linking exception and embedded dependency notices) and AUTHORS, and zlib's license from their vendored source directories. Consult the distribution's `third-party-licenses/index.json` for exact versions and source paths.

## macOS framework bindings

The published crates `objc2-core-foundation 0.3.2` and `objc2-core-services 0.3.2` both identify revision `7b1abfd750a2cacaea71d6a56ecfb83cb7de560b` in their `.cargo_vcs_info.json`. Their archives omit the repository's [license and Apple SDK notice](https://github.com/madsmtm/objc2/blob/7b1abfd750a2cacaea71d6a56ecfb83cb7de560b/LICENSE.md). An unmodified copy is retained as `third-party/objc2-frameworks-0.3.2-LICENSE`; each package's assembled license directory includes this notice and the full Apache-2.0 text, one of the licenses offered by that revision. This fallback requires the exact registry package, version and published revision.

## License verification

Run `python scripts/license_bundle.py --check --all-platforms` to validate the locked Linux, macOS and Windows dependency metadata without compiling or installing the product. Use `--check --target <triple>` for one target and `python scripts/test_license_bundle.py` for filesystem regressions. Cargo may download missing exact package sources. Validation checks package-local license names without case sensitivity, declared `license_file` paths, pinned fallbacks and required native notices; it reports all affected packages before assembling a license bundle. Unrelated Cargo cache ancestors cannot supply a dependency's license.

## Pi model catalog

The bundled model catalog is data from `@earendil-works/pi-ai@0.85.1`, distributed under the MIT license. The license below is from [the pinned upstream source](https://github.com/earendil-works/pi/blob/d981de1229ef899957bbe968bc8dcda02a21f477/LICENSE). It applies to the model data embedded in the model-access library; Pi runtime code is not required.

```text
MIT License

Copyright (c) 2025 Mario Zechner

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

## Cloud authentication dependencies

The locked Google Cloud auth/gax/rpc/wkt crates declare Apache-2.0 but omit its full text from their archives. `third-party/google-cloud-rust-LICENSE` preserves the upstream license; the exact versions and source revisions are recorded in `scripts/license_bundle.py`. The text was checked at every recorded revision. The MIT texts omitted by `base64-simd 0.8.0`, `uuid-simd 0.8.0`, `vsimd 0.8.0` and `defmt-parser 1.0.0` are likewise preserved from their published source revisions in `third-party/`. The installation license bundle includes these texts alongside AWS SDK and other package-local licenses.

The `jsonschema-regex 0.56.0` and `jsonschema-value 0.56.0` archives omit their MIT license text. Both identify revision `1e244c994dd81a1feb7801556a813c6bf2d45dad` in `.cargo_vcs_info.json`; its [upstream LICENSE](https://github.com/Stranger6667/jsonschema/blob/1e244c994dd81a1feb7801556a813c6bf2d45dad/LICENSE) is retained unmodified in `third-party/jsonschema-0.56.0-LICENSE` and included for each exact package version.
