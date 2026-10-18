//! `c` on a local comment (review-level, file, or line), in a local review:
//! answer it.
//!
//! The answer is an ordinary local comment carrying `Comment::reply_to`, the
//! first comment of the thread, and stored on the thread's line right after
//! its last message, so every view draws it under the comment it answers. A
//! PR review keeps `c` as it was: there it starts a new comment.

use super::*;

/// The local thread a comment being composed answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalReplyTarget {
    /// The comment under the cursor; the reply joins its thread.
    pub parent_id: String,
    /// Author of the comment under the cursor, for messages.
    pub author: String,
}

impl App {
    /// What the comment box's header calls the comment being written:
    /// `Reply` when it answers a thread, local or on the forge, otherwise
    /// `Edit` or `Add`.
    pub fn comment_input_action(&self, editing: bool) -> &'static str {
        if self.local_reply.is_some() || self.comment_reply.is_some() {
            "Reply"
        } else if editing {
            "Edit"
        } else {
            "Add"
        }
    }

    /// `c` on a local file or line comment in a local review: open the
    /// comment box to answer it. Returns false when that does not apply, so
    /// `c` keeps its usual meaning.
    pub fn enter_local_reply_mode(&mut self) -> bool {
        if !self.forge_config.extended_comments
            || matches!(self.diff_source, DiffSource::PullRequest(_))
            || self.session.pr_session_key.is_some()
        {
            return false;
        }
        let location = self.find_comment_at_cursor();
        if let Some(CommentLocation::Review { index }) = location {
            let Some(comment) = self.session.review_comments.get(index).cloned() else {
                return false;
            };
            self.enter_review_comment_mode();
            return self.open_reply_draft(&comment);
        }
        let (comment, file_level, anchor) = match location {
            Some(CommentLocation::File { path, index }) => {
                let Some(comment) = self
                    .session
                    .files
                    .get(&path)
                    .and_then(|review| review.file_comments.get(index))
                else {
                    return false;
                };
                (comment.clone(), true, None)
            }
            Some(CommentLocation::Line {
                path,
                line,
                side,
                index,
            }) => {
                let Some(comment) = self
                    .session
                    .files
                    .get(&path)
                    .and_then(|review| review.line_comments.get(&line))
                    .and_then(|comments| comments.get(index))
                else {
                    return false;
                };
                (comment.clone(), false, Some((line, side)))
            }
            _ => return false,
        };
        self.enter_comment_mode(file_level, anchor);
        self.open_reply_draft(&comment)
    }

    /// Put an empty reply at the end of `parent`'s thread and edit it, so
    /// the comment box opens right where the reply will stand. Saving fills
    /// it in; cancelling takes it away again.
    fn open_reply_draft(&mut self, parent: &Comment) -> bool {
        let draft = Comment::new(String::new(), self.comment_type.clone(), None)
            .with_author(self.username.clone());
        let Some((_, draft)) = self.session.add_reply(&parent.id, draft) else {
            self.exit_comment_mode();
            return true;
        };
        self.comment_line_range = draft
            .line_range
            .map(|range| (range, draft.side.unwrap_or(LineSide::New)));
        self.editing_comment_id = Some(draft.id);
        self.rebuild_annotations();
        self.set_message(format!("Replying to @{}", parent.author));
        self.local_reply = Some(LocalReplyTarget {
            parent_id: parent.id.clone(),
            author: parent.author.clone(),
        });
        true
    }

    /// Fill in the reply being composed. Returns the status message,
    /// prefixed `Error:` on failure as `save_comment` expects.
    pub(in crate::app) fn save_local_reply(
        &mut self,
        target: LocalReplyTarget,
        content: String,
    ) -> String {
        let comment_type = self.comment_type.clone();
        let draft = self
            .editing_comment_id
            .clone()
            .and_then(|id| self.session.comment_mut(&id));
        match draft {
            Some(draft) => {
                draft.content = content;
                draft.comment_type = comment_type;
            }
            // The draft went missing, merged away from under the box: start
            // the reply afresh.
            None => {
                let reply =
                    Comment::new(content, comment_type, None).with_author(self.username.clone());
                if self.session.add_reply(&target.parent_id, reply).is_none() {
                    return "Error: Could not save reply: the comment it answers is gone"
                        .to_string();
                }
            }
        }
        self.session.updated_at = Utc::now();
        format!("Reply to @{} saved", target.author)
    }

    /// Leaving the box without saving: drop the empty reply it was editing.
    pub(in crate::app) fn discard_reply_draft(&mut self) {
        if self.local_reply.take().is_none() {
            return;
        }
        if let Some(id) = self.editing_comment_id.clone() {
            self.session.remove_comment_by_id(&id);
            self.rebuild_annotations();
        }
    }
}

impl App {
    /// The session `y` exports. In a local review a thread someone else has
    /// answered last is done, so it is left out; a thread whose last message
    /// is yours goes out whole, each reply naming its author. Comments
    /// nobody has answered, and PR reviews, export as they always did.
    pub fn session_for_export(&self) -> std::borrow::Cow<'_, ReviewSession> {
        let local = self.forge_config.extended_comments
            && !matches!(self.diff_source, DiffSource::PullRequest(_));
        let has_replies = self
            .session
            .review_comments
            .iter()
            .any(|c| c.reply_to.is_some())
            || self.session.files.values().any(|review| {
                review
                    .file_comments
                    .iter()
                    .chain(review.line_comments.values().flatten())
                    .any(|comment| comment.reply_to.is_some())
            });
        if !local || !has_replies {
            return std::borrow::Cow::Borrowed(&self.session);
        }
        let mut session = self.session.clone();
        keep_threads_waiting_on(&mut session.review_comments, &self.username);
        for review in session.files.values_mut() {
            keep_threads_waiting_on(&mut review.file_comments, &self.username);
            for comments in review.line_comments.values_mut() {
                keep_threads_waiting_on(comments, &self.username);
            }
            review
                .line_comments
                .retain(|_, comments| !comments.is_empty());
        }
        std::borrow::Cow::Owned(session)
    }
}

/// Drop the answered threads from `comments` (see `session_for_export`) and
/// tag the replies left with their author.
fn keep_threads_waiting_on(comments: &mut Vec<Comment>, me: &str) {
    let thread = |comment: &Comment| {
        comment
            .reply_to
            .clone()
            .unwrap_or_else(|| comment.id.clone())
    };
    let mut last_author = HashMap::new();
    let mut answered = std::collections::HashSet::new();
    for comment in comments.iter() {
        if comment.reply_to.is_some() {
            answered.insert(thread(comment));
        }
        last_author.insert(thread(comment), comment.author.clone());
    }
    comments.retain(|comment| {
        let thread = thread(comment);
        !answered.contains(&thread) || last_author.get(&thread).map(String::as_str) == Some(me)
    });
    for reply in comments.iter_mut().filter(|c| c.reply_to.is_some()) {
        reply.content = format!("\u{21b3} @{}: {}", reply.author, reply.content);
    }
}
