//! `comment_style = "threads"`: local comments drawn as forge threads are,
//! one box per thread, a reply as an `↳ @author` divider inside it.

use super::*;

impl App {
    /// How a local comment's box is framed: as part of its thread with
    /// `comment_style = "threads"`, else on its own.
    pub fn thread_chrome(&self, comment: &Comment) -> crate::ui::comment_panel::ThreadChrome {
        crate::ui::comment_panel::ThreadChrome::of(comment, self.comment_threads)
    }

    /// The author a local comment's box names: always with threads, else
    /// only someone else.
    pub fn comment_box_author<'a>(&self, comment: &'a Comment) -> Option<&'a str> {
        (self.comment_threads || comment.author != self.username).then_some(comment.author.as_str())
    }
}
