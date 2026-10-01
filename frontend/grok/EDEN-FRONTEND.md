# Eden frontend modifications

Fixed reference: `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. This source was imported from the accepted prepared pager at Eden `57836744e2664f9359aa649b5f963c0e45e0035b`; the prepared integration changes are retained as source, not regenerated during build.

The terminal runs as an Eden library. `eden-session-workspace` owns the catalog, drafts, reversible removal and host attachments. The former separate frontend executable and local JSON-RPC byte transport have been removed. Explicit project and preference inputs replace launcher environment wiring. The terminal retains local drafts on failed loads, separate view identities, ordered replay and native Grok editor/rendering components. Eden's entry excludes Grok authentication, cloud session startup, leader control and global process teardown; retained presentation effects use Eden hosts. The original license and third-party notices accompany redistribution.

Build and install instructions are in [the frontend guide](../README.md).
