//! `@` mention suggestions in the comment box.
//!
//! With `[forge] extended_comments`, `@` in a PR review lists the PR's
//! participants, author first, then the forge's matches as the name grows.
//! Never the viewer. In local reviews `@` stays plain text.

use super::*;
use crate::forge::traits::MentionCandidate;

/// Rows the suggestion list shows at most.
pub const MENTION_LIST_MAX_ROWS: usize = 6;

/// GitHub logins are at most 39 characters; a longer token is not one.
const MAX_LOGIN_LEN: usize = 39;

/// The suggestion list open in the comment box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionState {
    /// Byte offset of the `@` in `comment_buffer`.
    pub start: usize,
    /// Text typed after the `@`.
    pub query: String,
    pub candidates: Vec<MentionCandidate>,
    pub selected: usize,
}

/// Forge lookups behind the suggestion list, kept for one PR.
#[derive(Debug, Default)]
pub struct MentionLookup {
    /// The signed-in user, never suggested.
    pub viewer_login: Option<String>,
    /// Forge results by lowercased query. Failures are stored empty, not
    /// retried.
    pub results: HashMap<String, Vec<MentionCandidate>>,
    /// Query of the one lookup in flight; a newer query waits for it.
    pub in_flight: Option<String>,
    pub rx: Option<std::sync::mpsc::Receiver<MentionEvent>>,
    /// `@` of the token whose list was closed; it stays closed while the cursor
    /// is in it.
    pub dismissed_at: Option<usize>,
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

fn is_login_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// The `@` token under the cursor: its `@` offset and the text up to the
/// cursor. An `@` after a word character, as in an email, starts none.
pub(crate) fn mention_token_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let query_start = before.trim_end_matches(is_login_char).len();
    let at = query_start.checked_sub(1)?;
    if before.as_bytes()[at] != b'@' {
        return None;
    }
    if before[..at]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '@' || c == '`')
    {
        return None;
    }
    let query = &before[query_start..];
    (query.len() <= MAX_LOGIN_LEN).then_some((at, query))
}

fn matches_query(candidate: &MentionCandidate, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    candidate.login.to_lowercase().contains(&query)
        || candidate
            .name
            .as_ref()
            .is_some_and(|name| name.to_lowercase().contains(&query))
}

impl App {
    /// Start over for a newly opened PR, signed in as `viewer_login`.
    pub(in crate::app) fn reset_mentions(&mut self, viewer_login: Option<String>) {
        self.mention = None;
        self.mention_lookup = MentionLookup::new(viewer_login);
    }

    /// `@` opens suggestions only in PR reviews with `extended_comments` on.
    fn mentions_available(&self) -> bool {
        self.forge_config.extended_comments && self.open_pr_target().is_some()
    }

    /// The PR's participants, author first. Teams and bots are left out: a
    /// plain `@login` cannot name them.
    fn mention_participants(&self) -> Vec<String> {
        let mut logins: Vec<&str> = Vec::new();
        if let Some(info) = self.pr_info.as_ref() {
            logins.extend(info.details.author.as_deref());
            logins.extend(info.requested_users.iter().map(String::as_str));
            logins.extend(
                info.latest_reviews
                    .iter()
                    .filter_map(|r| r.author.as_deref()),
            );
        }
        logins.extend(
            self.forge_review_summaries
                .iter()
                .filter_map(|s| s.author.as_deref()),
        );
        logins.extend(
            self.forge_review_threads
                .iter()
                .flat_map(|t| &t.comments)
                .filter_map(|c| c.author.as_deref()),
        );
        if let Some(info) = self.pr_info.as_ref() {
            logins.extend(
                info.issue_comments
                    .iter()
                    .filter_map(|c| c.author.as_deref()),
            );
        }
        logins
            .into_iter()
            .filter(|login| !login.is_empty() && login.chars().all(is_login_char))
            .map(str::to_string)
            .collect()
    }

    /// Matching participants, then forge matches. A query not looked up yet
    /// uses its longest looked-up prefix, so the list never empties.
    pub(crate) fn mention_candidates(&self, query: &str) -> Vec<MentionCandidate> {
        let viewer = self
            .mention_lookup
            .viewer_login
            .as_deref()
            .map(str::to_lowercase);
        let mut seen: HashSet<String> = viewer.into_iter().collect();
        let mut candidates = Vec::new();

        for login in self.mention_participants() {
            let candidate = MentionCandidate { login, name: None };
            if matches_query(&candidate, query) && seen.insert(candidate.login.to_lowercase()) {
                candidates.push(candidate);
            }
        }

        let lowered = query.to_lowercase();
        let remote = (0..=lowered.len())
            .rev()
            .filter(|&end| lowered.is_char_boundary(end))
            .find_map(|end| self.mention_lookup.results.get(&lowered[..end]));
        if let Some(remote) = remote {
            for candidate in remote {
                if matches_query(candidate, query) && seen.insert(candidate.login.to_lowercase()) {
                    candidates.push(candidate.clone());
                }
            }
        }
        candidates
    }

    /// Rebuild the list from the comment text and cursor after each edit.
    pub fn refresh_mention(&mut self) {
        let token = (self.input_mode == InputMode::Comment && self.mentions_available())
            .then(|| mention_token_at(&self.comment_buffer, self.comment_cursor))
            .flatten()
            .map(|(start, query)| (start, query.to_string()));
        let Some((start, query)) = token else {
            self.mention = None;
            self.mention_lookup.dismissed_at = None;
            return;
        };
        if self.mention_lookup.dismissed_at == Some(start) {
            self.mention = None;
            return;
        }
        self.mention_lookup.dismissed_at = None;
        self.request_mentionable_users(&query);

        let candidates = self.mention_candidates(&query);
        if candidates.is_empty() {
            self.mention = None;
            return;
        }
        // Keep the highlighted person highlighted while the list refines.
        let selected = self
            .mention
            .as_ref()
            .and_then(|old| old.candidates.get(old.selected))
            .and_then(|old| candidates.iter().position(|c| c.login == old.login))
            .unwrap_or(0);
        self.mention = Some(MentionState {
            start,
            query,
            candidates,
            selected,
        });
    }

    /// Look `query` up on the forge, unless known or a lookup is running. An
    /// empty query needs none.
    fn request_mentionable_users(&mut self, query: &str) {
        let query = query.to_lowercase();
        if query.is_empty()
            || self.mention_lookup.in_flight.is_some()
            || self.mention_lookup.results.contains_key(&query)
        {
            return;
        }
        let Some((key, details)) = self.open_pr_target() else {
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
        self.mention_lookup.in_flight = Some(query.clone());
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
                self.mention_lookup.in_flight = None;
                return false;
            }
        };
        self.mention_lookup.rx = None;
        self.mention_lookup.in_flight = None;

        let MentionEvent::Done { key, query, result } = event;
        let same_pr = matches!(&self.diff_source, DiffSource::PullRequest(pr) if pr.key == key);
        if !same_pr {
            return false;
        }
        // A failure only loses the forge's matches; no warning.
        self.mention_lookup
            .results
            .insert(query, result.unwrap_or_default());
        self.refresh_mention();
        true
    }

    pub fn mention_select_next(&mut self) {
        if let Some(state) = self.mention.as_mut() {
            state.selected = (state.selected + 1) % state.candidates.len();
        }
    }

    pub fn mention_select_prev(&mut self) {
        if let Some(state) = self.mention.as_mut() {
            let len = state.candidates.len();
            state.selected = (state.selected + len - 1) % len;
        }
    }

    /// Close the list without inserting anything.
    pub fn dismiss_mention(&mut self) {
        if let Some(state) = self.mention.take() {
            self.mention_lookup.dismissed_at = Some(state.start);
        }
    }

    /// Swap the token under the cursor for the highlighted login and a space.
    pub fn accept_mention(&mut self) {
        let Some(state) = self.mention.take() else {
            return;
        };
        let Some(candidate) = state.candidates.get(state.selected) else {
            return;
        };
        let tail: usize = self.comment_buffer[self.comment_cursor..]
            .chars()
            .take_while(|&c| is_login_char(c))
            .map(char::len_utf8)
            .sum();
        let end = self.comment_cursor + tail;
        let space = if self.comment_buffer[end..].starts_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        let inserted = format!("@{}{space}", candidate.login);
        self.comment_buffer
            .replace_range(state.start..end, &inserted);
        self.comment_cursor = state.start + inserted.len();
        self.mention_lookup.dismissed_at = Some(state.start);
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
    use super::mention_token_at;

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
