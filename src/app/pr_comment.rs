//! `<leader>C` (with `[forge] extended_comments`): post to the PR
//! conversation right away, verbatim. A `<leader>c` comment only reaches the
//! forge inside the review `:submit` creates, which comment-command bots never
//! see.

use super::*;

impl App {
    /// Open the comment input for a PR conversation comment.
    pub fn enter_pr_comment_mode(&mut self) {
        if !self.forge_config.extended_comments {
            self.set_warning("PR comments need [forge] extended_comments");
            return;
        }
        if self.open_pr_target().is_none() {
            self.set_warning("PR comments need an open pull request");
            return;
        }
        if self.pr_comment_rx.is_some() {
            self.set_warning("Still posting the previous PR comment");
            return;
        }
        self.enter_review_comment_mode();
        self.comment_is_pr_comment = true;
        // A failed post's text comes back, so retrying is one key.
        if let Some(body) = self.pr_comment_unsent.take() {
            self.comment_cursor = body.len();
            self.comment_buffer = body;
        }
    }

    /// Send `body` to the PR conversation on a background thread.
    pub(in crate::app) fn post_pr_comment(&mut self, body: String) {
        let Some((key, details)) = self.open_pr_target() else {
            self.pr_comment_unsent = Some(body);
            self.set_warning("PR comments need an open pull request");
            return;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        self.pr_comment_rx = Some(rx);

        let local_checkout = self
            .forge_backend
            .as_deref()
            .and_then(|backend| backend.local_checkout_path());
        let show_pr_checks = self.show_pr_checks;
        let show_pr_comments = self.show_pr_comments;

        std::thread::spawn(move || {
            let backend = create_forge_backend(
                &details.repository,
                local_checkout,
                show_pr_checks,
                show_pr_comments,
            );
            let result = backend
                .create_pull_request_comment(&details, &body)
                .map_err(|error| (error.to_string(), body));
            let _ = tx.send(PrCommentEvent::Done { key, result });
        });

        self.set_message("Posting PR comment\u{2026}");
    }

    /// Apply a finished post. Returns whether a repaint is needed.
    pub fn poll_pr_comment_events(&mut self) -> bool {
        let Some(rx) = self.pr_comment_rx.as_ref() else {
            return false;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pr_comment_rx = None;
                return false;
            }
        };
        self.pr_comment_rx = None;

        let PrCommentEvent::Done { key, result } = event;
        match result {
            Ok(comment) => {
                // Show it without a reload, if that PR is still open.
                let same_pr = matches!(
                    &self.diff_source,
                    DiffSource::PullRequest(pr) if pr.key == key
                );
                if same_pr && let Some(info) = self.pr_info.as_mut() {
                    info.issue_comments.push(comment);
                    self.rebuild_annotations();
                }
                self.set_message("PR comment posted");
            }
            Err((error, body)) => {
                self.pr_comment_unsent = Some(body);
                self.set_error(format!(
                    "PR comment not posted \u{00b7} {error} \u{00b7} {}C to retry",
                    self.leader_key
                ));
            }
        }
        true
    }
}
