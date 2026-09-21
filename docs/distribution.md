# Resource package distribution

The explicit package commands accept native plugins, text-only resources, or a bundle containing both. Session startup only reads installed files; it never downloads or builds a package. Local directories and tar/tar.gz archives, Git sources pinned to a resolved commit, and HTTPS archives pinned by SHA-256 all use the same installation and receipt path described in [workspace configuration](workspace.md#package-sources-and-locked-versions).

A text-only bundle needs this root `package.json` and its listed files or directories:

```json
{
  "resources": {
    "name": "review-skills",
    "version": "1.0.0",
    "skills": ["skills/review"],
    "templates": ["prompts"]
  }
}
```

Resource paths must exist inside the bundle; absolute paths, parent traversal, links and special files are rejected. Skill and template contents use the ordinary Markdown resource format. A text-only bundle does not declare `manifest`, host/SDK identifiers, a platform target, or a native library. It installs under the platform-independent `resources` target directory. Even an explicit `--build` does not execute a build for a text-only package.

A mixed bundle adds the existing native `manifest` alongside `resources`. The resource name and version must match the native descriptor. Native compatibility and explicit-build checks apply to the native part; both parts share one installed tree digest and removal identity. Dependencies may use any supported bundle type. Installation validates the complete dependency graph before publishing new versions.

```sh
eden package install /path/to/review-skills
eden package resolve '{"base":"/path/to/composition.json","packages":[{"name":"review-skills","version":"1.0.0"}],"roles":{}}'
```

Use the returned immutable composition path to start a session. Its `resource_packages` entries lock each resource manifest, absolute installed root and content digest separately from native `packages`; a text-only package adds no native role or library. The selected ResourceSource receives the validated resource packages and discovers their declared Skill/template roots using its normal rules. `skill_excludes` and `template_excludes` can exclude selected entries without disabling future discovery. Excluding a resource does not remove the installed package or its composition binding.

Installing a newer version leaves previous versions intact. Resolve a new composition to select the newer version; reopening with the older composition checks and loads the older digest. Missing or modified locked resources fail explicitly rather than falling back to another installed version. Resource identities also participate in saved session composition matching. Raw history retains the content already sent to the model.

Package removal checks dependencies, saved compositions and session references for native and text-only packages. Ordinary removal of a referenced version reports those bindings. Explicit forced removal reports affected bindings and preserves session history; subsequent reopening of a composition that still requires the removed version fails. Copied session references retain resource package roots through the same data-only reference tracking used for native library locations.
