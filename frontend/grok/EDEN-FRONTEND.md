# Eden frontend modifications

Fixed reference: `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. This source was imported from the accepted prepared pager at Eden `57836744e2664f9359aa649b5f963c0e45e0035b`; the prepared integration changes are retained as source, not regenerated during build.

The tracked integration embeds `eden-frontend-session`, routes local management to `eden-session-lifecycle`, preserves local views and drafts on failed loads, and assigns each replay its own view identity. Eden business effects suppress Grok cloud, billing, search and log traffic. Layout, composer, renderer, scrolling, tool and Diff widgets retain the fixed source implementation. The original Apache-2.0 license and third-party notices accompany redistribution.

Build and install instructions are in [the frontend guide](../README.md).
