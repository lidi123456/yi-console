//! Chrome DevTools Protocol engine behind Yi Console's `yi_browser` MCP tools.
//!
//! Much of the design (launch arguments, `DevToolsActivePort`, snapshot roles and refs, click checks, dialog
//! handling, content boundaries) follows vercel-labs/agent-browser (Apache-2.0, Copyright Vercel, Inc.); see
//! `docs/proposals/browser.md`. No yi crate is used here, so this can be tested against a real Chrome on its own.

mod actions;
mod browser;
mod cdp;
mod extra;
mod keys;
pub mod launch;
pub mod net;
mod snapshot;
pub mod tools;
pub mod view;

pub use actions::{check_url, url_matches};
pub use browser::{Browser, Dialog, Hooks};
pub use launch::{find_browser, has_display, LaunchOptions, DEFAULT_VIEWPORT};
pub use view::{CastConfig, Frame, ViewState};
pub use snapshot::SnapshotOptions;
pub use tools::{boundary, category, tool_names, tools, Outcome, UploadRoots, READ_ONLY};

/// At most `max` characters, with an ellipsis when cut
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod e2e;
