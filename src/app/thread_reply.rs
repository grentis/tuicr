//! `c` on a GitHub review thread (with `[forge] extended_comments`): answer it.
//!
//! The answer is a local draft tied to the thread by
//! `Comment::reply_to_thread`, stored at the thread's anchor, and sent on
//! `:submit` as a reply inside that thread.

use super::*;
use crate::forge::remote_comments::RemoteCommentSide;
use crate::forge::traits::ForgeKind;

/// The thread a comment being composed answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReplyTarget {
    pub thread_id: String,
    /// File the thread is on, as the session keys it.
    pub path: PathBuf,
    /// Author of the thread's first comment, for messages.
    pub author: Option<String>,
}

impl App {
    /// The remote review thread under the cursor.
    fn remote_thread_idx_at_cursor(&self) -> Option<usize> {
        match self.line_annotations.get(self.diff_state.cursor_line) {
            Some(AnnotatedLine::RemoteThreadLine { thread_idx, .. }) => Some(*thread_idx),
            _ => None,
        }
    }

    /// `c` on a remote thread: open the comment box to answer it. Returns
    /// false off a thread or with `[forge] extended_comments` off, so `c`
    /// keeps its usual meaning.
    pub fn enter_thread_reply_mode(&mut self) -> bool {
        if !self.forge_config.extended_comments {
            return false;
        }
        let Some(thread_idx) = self.remote_thread_idx_at_cursor() else {
            return false;
        };
        if let DiffSource::PullRequest(pr) = &self.diff_source
            && let Some(reason) = pr.read_only_reason()
        {
            self.set_warning(format!("Cannot reply: PR is {reason}"));
            return true;
        }
        if self.forge_kind() != Some(ForgeKind::GitHub) {
            self.set_message(format!(
                "Replying to {} threads is not supported",
                self.forge_display_name()
            ));
            return true;
        }
        let Some(thread) = self.forge_review_threads.get(thread_idx) else {
            return true;
        };
        let target = ThreadReplyTarget {
            thread_id: thread.id.clone(),
            path: PathBuf::from(&thread.path),
            author: thread.root().and_then(|root| root.author.clone()),
        };
        if !self.session.files.contains_key(&target.path) {
            self.set_warning(format!("{} is not part of this review", thread.path));
            return true;
        }
        let anchor = thread.line.map(|line| {
            let side = match thread.side {
                RemoteCommentSide::Left => LineSide::Old,
                RemoteCommentSide::Right => LineSide::New,
            };
            (line, side)
        });

        self.enter_comment_mode(anchor.is_none(), anchor);
        self.comment_line_range = None;
        self.editing_comment_id = None;
        let who = reply_target_label(target.author.as_deref());
        self.comment_reply = Some(target);
        self.set_message(format!("Replying to {who} \u{00b7} sent with :submit"));
        true
    }

    /// Save the reply as a local draft at the thread's anchor. Returns the
    /// status message (`Error:` on failure, as `save_comment` expects).
    pub(in crate::app) fn save_thread_reply(
        &mut self,
        target: ThreadReplyTarget,
        content: String,
    ) -> String {
        let side = self.comment_line.map(|(_, side)| side);
        let mut comment = Comment::new(content, self.comment_type.clone(), side)
            .with_author(self.username.clone());
        comment.reply_to_thread = Some(target.thread_id);
        let line = self.comment_line.map(|(line, _)| line);
        let Some(review) = self.session.get_file_mut(&target.path) else {
            return format!(
                "Error: Could not save reply: {} is not part of this review",
                target.path.display()
            );
        };
        match line {
            Some(line) => review.add_line_comment(line, comment),
            None => review.add_file_comment(comment),
        }
        self.session.updated_at = Utc::now();
        format!(
            "Reply to {} saved \u{00b7} sent with :submit",
            reply_target_label(target.author.as_deref())
        )
    }

    /// Header tag for a comment that answers a thread, such as
    /// `↳ reply to @alice`; `None` for an ordinary comment.
    pub fn reply_label(&self, comment: &Comment) -> Option<String> {
        // A local reply names whom it answers.
        if let Some(root_id) = comment.reply_to.as_deref() {
            let author = self
                .session
                .find_comment_by_id(root_id)
                .and_then(|location| self.session.comment_at(&location))
                .map(|root| root.author.clone());
            return Some(match author {
                Some(author) => format!("\u{21b3} reply to @{author} "),
                None => "\u{21b3} reply ".to_string(),
            });
        }
        let thread_id = comment.reply_to_thread.as_deref()?;
        let author = self
            .forge_review_threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.root())
            .and_then(|root| root.author.as_deref());
        Some(match author {
            Some(author) => format!("\u{21b3} reply to @{author} "),
            None => "\u{21b3} reply ".to_string(),
        })
    }
}

fn reply_target_label(author: Option<&str>) -> String {
    match author {
        Some(author) => format!("@{author}'s thread"),
        None => "the thread".to_string(),
    }
}
