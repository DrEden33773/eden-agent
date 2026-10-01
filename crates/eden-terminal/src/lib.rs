//! Installed Eden's terminal composition root, with explicit Session ownership and retained Grok rendering.
use eden_session_workspace::{Lifecycle, OpenTarget};

/// Worker re-execution must be recognized before normal CLI parsing.
pub fn render_worker() -> Option<i32> {
    xai_grok_pager::app::mermaid_worker::maybe_run_render_subprocess()
}

/// Run the terminal in the CLI process. Saved Session hosts outlive this consumer.
pub async fn run(context: Lifecycle, target: OpenTarget, monochrome: bool) -> anyhow::Result<i32> {
    xai_grok_pager_minimal::install();
    xai_grok_pager::app::run_eden(xai_grok_pager::app::EdenLaunch {
        context,
        target,
        monochrome,
    })
    .await?;
    Ok(0)
}
