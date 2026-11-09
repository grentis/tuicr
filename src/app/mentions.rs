//! `@` mention suggestions in the comment box.
//!
//! With `[forge] extended_comments`, `@` in a PR review lists the PR's
//! participants, author first, then the forge's matches as the name grows.
//! Never the viewer. In local reviews `@` stays plain text.

use super::*;
// The token, the participants, the candidates, the list and its state are
// the library's (`forge::mentions`), shared with programs that use tuicr;
// the app asks the forge and draws.
use crate::forge::mentions::{MentionList, MentionTracker, participants};
use crate::forge::traits::MentionCandidate;

/// Rows the suggestion list shows at most.
pub const MENTION_LIST_MAX_ROWS: usize = 6;

/// The suggestion list open in the comment box.
pub type MentionState = MentionList;

/// Forge lookups behind the suggestion list, kept for one PR: who is signed
/// in, the lookup running, and the list's state.
#[derive(Debug, Default)]
pub struct MentionLookup {
    /// The signed-in user, never suggested.
    pub viewer_login: Option<String>,
    pub rx: Option<std::sync::mpsc::Receiver<MentionEvent>>,
    pub tracker: MentionTracker,
}

impl MentionLookup {
    pub fn new(viewer_login: Option<String>) -> Self {
        Self {
            viewer_login,
            ..Self::default()
        }
    }
}

/// A finished forge lookup.
#[derive(Debug)]
pub enum MentionEvent {
    Done {
        key: crate::forge::traits::PrSessionKey,
        query: String,
        result: std::result::Result<Vec<MentionCandidate>, String>,
    },
}

impl App {
    /// Start over for a newly opened PR, signed in as `viewer_login`.
    pub(in crate::app) fn reset_mentions(&mut self, viewer_login: Option<String>) {
        self.mention_lookup = MentionLookup::new(viewer_login);
    }

    /// The suggestion list open, if any.
    pub fn mention(&self) -> Option<&MentionState> {
        self.mention_lookup.tracker.list.as_ref()
    }

    /// `@` opens suggestions only in PR reviews with `extended_comments` on.
    fn mentions_available(&self) -> bool {
        self.forge_config.extended_comments && self.open_pr_target().is_some()
    }

    /// Rebuild the list from the comment text and cursor after each edit;
    /// a name not known yet is looked up on the forge.
    pub fn refresh_mention(&mut self) {
        if self.input_mode != InputMode::Comment || !self.mentions_available() {
            self.mention_lookup.tracker.close();
            return;
        }
        // The PR's participants, author first; teams and bots left out (a
        // plain `@login` cannot name them).
        let participants = participants(
            self.pr_info.as_ref(),
            &self.forge_review_summaries,
            &self.forge_review_threads,
        );
        let ask = self.mention_lookup.tracker.refresh(
            &self.comment_buffer,
            self.comment_cursor,
            &participants,
            self.mention_lookup.viewer_login.as_deref(),
        );
        if let Some(query) = ask {
            self.request_mentionable_users(query);
        }
    }

    /// Look `query` up on the forge, off the UI thread; the tracker marked
    /// it in flight.
    fn request_mentionable_users(&mut self, query: String) {
        let Some((key, details)) = self.open_pr_target() else {
            self.mention_lookup.tracker.in_flight = None;
            return;
        };
        let local_checkout = self
            .forge_backend
            .as_deref()
            .and_then(|backend| backend.local_checkout_path());
        let show_pr_checks = self.show_pr_checks;
        let show_pr_comments = self.show_pr_comments;

        let (tx, rx) = std::sync::mpsc::channel();
        self.mention_lookup.rx = Some(rx);
        std::thread::spawn(move || {
            let backend = create_forge_backend(
                &details.repository,
                local_checkout,
                show_pr_checks,
                show_pr_comments,
            );
            let result = backend
                .search_mentionable_users(&details, &query)
                .map_err(|error| error.to_string());
            let _ = tx.send(MentionEvent::Done { key, query, result });
        });
    }

    /// Fold in a finished lookup. Returns whether a repaint is needed.
    pub fn poll_mention_events(&mut self) -> bool {
        let Some(rx) = self.mention_lookup.rx.as_ref() else {
            return false;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.mention_lookup.rx = None;
                self.mention_lookup.tracker.in_flight = None;
                return false;
            }
        };
        self.mention_lookup.rx = None;

        let MentionEvent::Done { key, query, result } = event;
        let same_pr = matches!(&self.diff_source, DiffSource::PullRequest(pr) if pr.key == key);
        if !same_pr {
            self.mention_lookup.tracker.in_flight = None;
            return false;
        }
        // A failure only loses the forge's matches; no warning.
        self.mention_lookup
            .tracker
            .found(query, result.unwrap_or_default());
        self.refresh_mention();
        true
    }

    pub fn mention_select_next(&mut self) {
        self.mention_lookup.tracker.select(1, usize::MAX);
    }

    pub fn mention_select_prev(&mut self) {
        self.mention_lookup.tracker.select(-1, usize::MAX);
    }

    /// Close the list without inserting anything.
    pub fn dismiss_mention(&mut self) {
        self.mention_lookup.tracker.dismiss();
    }

    /// Swap the token under the cursor for the highlighted login and a space.
    pub fn accept_mention(&mut self) {
        let Some((text, cursor)) =
            self.mention_lookup
                .tracker
                .accept(&self.comment_buffer, self.comment_cursor, None)
        else {
            return;
        };
        self.comment_buffer = text;
        self.comment_cursor = cursor;
        if self.comment_vim_editor.is_some() {
            self.comment_vim_editor = Some(CommentVimEditor::from_buffer(
                &self.comment_buffer,
                self.comment_cursor,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::forge::mentions::mention_token_at;

    #[test]
    fn should_find_the_token_the_cursor_is_in() {
        assert_eq!(mention_token_at("@", 1), Some((0, "")));
        assert_eq!(mention_token_at("hi @oct", 7), Some((3, "oct")));
        assert_eq!(mention_token_at("(@a-b", 5), Some((1, "a-b")));
        assert_eq!(mention_token_at("x\n@ab cd", 5), Some((2, "ab")));
    }

    #[test]
    fn should_ignore_text_that_is_not_a_mention() {
        assert_eq!(mention_token_at("mail me@x", 9), None);
        assert_eq!(mention_token_at("@ab cd", 6), None);
        assert_eq!(mention_token_at("no at", 5), None);
        assert_eq!(mention_token_at("@@a", 3), None);
        assert_eq!(mention_token_at(&format!("@{}", "a".repeat(40)), 41), None);
    }
}
