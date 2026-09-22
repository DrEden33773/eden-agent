# Update sources and managed installations

`eden.update-source.v1` is a replaceable SDK service. `Discover` lists the host and installed plugins, including sources that are not configured. `Check` returns a pinned candidate without installing it; `Prepare` explicitly downloads and validates it; `Activate` explicitly changes future host launches. Plugin preparation uses the existing distribution store and remains disabled until `package.resolve` creates a new composition. Saved sessions and their old installation paths are retained.

The official host source is the public GitHub Releases API for `DrEden33773/eden-agent`, without credentials. Stable uses `/releases/latest`, which excludes prereleases; prerelease uses the release list, including previews; tag selects an exact tag. The expected archive is `eden-agent-<native-target>.tar.gz`, with GitHub's `sha256:` asset digest. No matching official release is an empty check result. A release missing the target archive or its digest is a diagnostic, not an invitation to install an arbitrary artifact.

Source checkouts and manually copied installations support checks and original-channel instructions. Automatic host preparation requires an explicitly configured managed root. Updates do not introduce npm or Cargo installation channels.

## Configuration

The distribution package accepts an optional `updates` object alongside its existing `root` setting:

```json
{
  "updates": {
    "managed_root": "/absolute/managed-installation",
    "sources": [
      {
        "target": {"kind": "plugin", "name": "example"},
        "source": {"kind": "github", "repository": "owner/example", "asset": "example.tar.gz"}
      }
    ]
  }
}
```

Host `current_version` comes from the running executable's installation-root `release.json`; source builds without that manifest use the compiled crate version. Embedding hosts whose executable is outside Eden's `bin/` directory can set an absolute `updates.installation_root` explicitly. The active-launcher pointer does not change the identity of an already running old host.

Plugin checks compare the candidate with every installed exact version, so a version already prepared is not offered again. With one installed version, `current_version` contains it; with multiple versions it is null and `instructions` lists those identities without implying a latest version or an active session binding. Discover and Check use the same installed inventory.

Omit `managed_root` for source/manual installs. Plugin sources are opt-in and independent of their locked installed identities. Controlled tests may configure `{"kind":"local","path":"/absolute/releases.json"}`; that file contains GitHub-shaped release objects with a `source` field using the existing distribution local/HTTPS source format. This adapter performs no network requests and is not an official release channel.

## Complete release format

A release archive has `package.json` at its root (an empty object is sufficient for host releases) and `release.json` containing `version`, `target`, `executable`, and `files`. The version is the exact release tag, the target is the native target triple, and the executable is a relative path such as `bin/eden` or `bin/eden.exe`. `files` maps every regular file's relative slash-separated path to its SHA-256 digest, excluding only `release.json` itself. Include composition files, native libraries, workers, resources and license files. Links, special files, missing files, unlisted files and changed checksums are rejected.

Preparation obtains an operating-system file lock, verifies the complete inventory, runs the new executable's offline `installation-check`, and moves it into `releases/<version>-<target>`. An existing version with different bytes is rejected. Activation rechecks the prepared digest and startup, then atomically renames a new numbered record into `activations/`. Records contain a path relative to the managed root. A launcher uses the protocol's `active_installation` helper before loading native libraries. The helper rejects invalid or missing latest destinations rather than silently choosing an older release.

The append-only activation journal avoids replacing an open executable, DLL or pointer file on Windows. A crash before commit leaves the last committed activation in force; a subsequent explicit prepare clears abandoned staging. Failed checks, failed startup and cancellation before activation leave the prior record and old directory intact. Preparing or activating another version never deletes old installations or rewrites session compositions. A previously prepared, unchanged version can be explicitly activated again for rollback.

Real GitHub release publication is a separate operation. Controlled transaction tests establish local behavior; they do not claim a real official release exists or has been downloaded.

## Launching the selected installation

After explicit preparation and activation, invoke the stable launcher with the managed root followed by ordinary Eden arguments:

```sh
/path/to/original/bin/eden-launch /absolute/managed-installation installation-check
/path/to/original/bin/eden-launch /absolute/managed-installation
```

On Windows use `eden-launch.exe`. The launcher reads the latest committed activation before loading native libraries, starts that directory's `bin/eden`, and passes `EDEN_MANAGED_ROOT` to it. Keep the original launcher's directory available. Directly invoking an old directory's `bin/eden` remains supported and does not follow the active-version pointer. Explicit update commands outside the launcher can use `EDEN_MANAGED_ROOT` to select the same managed root; source/manual installations should leave it unset.
