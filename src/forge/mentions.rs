//! `@` mentions in a comment being written: the token under the cursor, the
//! people to suggest for it, and the token swapped for the login taken.
//!
//! UI-free: tuicr's comment box uses these, and so can other programs that
//! use tuicr as a library.

use std::collections::{HashMap, HashSet};

use crate::forge::remote_comments::{RemoteReviewSummary, RemoteReviewThread};
use crate::forge::traits::{MentionCandidate, PullRequestInfo};

/// GitHub logins are at most 39 characters; a longer token is not one.
pub const MAX_LOGIN_LEN: usize = 39;

/// A character a GitHub login may hold.
pub fn is_login_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// The `@` token under the cursor (a byte offset into `text`): its `@`
/// offset and the text up to the cursor. An `@` after a word character, as
/// in an email, starts none.
pub fn mention_token_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
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

/// The candidate's login or name holds the query, any case.
pub fn matches_query(candidate: &MentionCandidate, query: &str) -> bool {
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

/// The pull request's participants, author first: requested reviewers,
/// reviewers, the review summaries' and the threads' authors, the
/// conversation's. Teams and bots are left out: a plain `@login` cannot
/// name them.
pub fn participants(
    info: Option<&PullRequestInfo>,
    summaries: &[RemoteReviewSummary],
    threads: &[RemoteReviewThread],
) -> Vec<String> {
    let mut logins: Vec<&str> = Vec::new();
    if let Some(info) = info {
        logins.extend(info.details.author.as_deref());
        logins.extend(info.requested_users.iter().map(String::as_str));
        logins.extend(
            info.latest_reviews
                .iter()
                .filter_map(|r| r.author.as_deref()),
        );
    }
    logins.extend(summaries.iter().filter_map(|s| s.author.as_deref()));
    logins.extend(
        threads
            .iter()
            .flat_map(|t| &t.comments)
            .filter_map(|c| c.author.as_deref()),
    );
    if let Some(info) = info {
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

/// The people to suggest for `query`: the participants who match, then the
/// forge's matches (`results`, by lowercased query; a query not looked up yet
/// uses its longest looked-up prefix, so the list never empties); each once,
/// never the viewer.
pub fn mention_candidates(
    participants: &[String],
    viewer: Option<&str>,
    query: &str,
    results: &HashMap<String, Vec<MentionCandidate>>,
) -> Vec<MentionCandidate> {
    let mut seen: HashSet<String> = viewer.map(str::to_lowercase).into_iter().collect();
    let mut candidates = Vec::new();
    for login in participants {
        let candidate = MentionCandidate {
            login: login.clone(),
            name: None,
        };
        if matches_query(&candidate, query) && seen.insert(candidate.login.to_lowercase()) {
            candidates.push(candidate);
        }
    }
    let lowered = query.to_lowercase();
    let remote = (0..=lowered.len())
        .rev()
        .filter(|&end| lowered.is_char_boundary(end))
        .find_map(|end| results.get(&lowered[..end]));
    if let Some(remote) = remote {
        for candidate in remote {
            if matches_query(candidate, query) && seen.insert(candidate.login.to_lowercase()) {
                candidates.push(candidate.clone());
            }
        }
    }
    candidates
}

/// The token from `start` (its `@`) through the login characters after the
/// cursor swapped for `@login` and a space (none when whitespace follows):
/// the new text, and the cursor after the space.
pub fn accept_mention(text: &str, start: usize, cursor: usize, login: &str) -> (String, usize) {
    let tail: usize = text[cursor..]
        .chars()
        .take_while(|&c| is_login_char(c))
        .map(char::len_utf8)
        .sum();
    let end = cursor + tail;
    let space = if text[end..].starts_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    let inserted = format!("@{login}{space}");
    let mut out = text.to_string();
    out.replace_range(start..end, &inserted);
    (out, start + inserted.len())
}

/// The `@` list open in a comment being written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionList {
    /// Byte offset of the `@` in the text.
    pub start: usize,
    /// Text typed after the `@`.
    pub query: String,
    pub candidates: Vec<MentionCandidate>,
    pub selected: usize,
}

/// A comment box's mentions: the list open, the forge's answers so far (by
/// lowercased query; a failure stored empty, not asked again), the one query
/// being asked, and the `@` whose list was closed (closed while the cursor
/// is in it). UI-free: the program asks the forge and draws the list.
#[derive(Debug, Default)]
pub struct MentionTracker {
    pub list: Option<MentionList>,
    pub results: HashMap<String, Vec<MentionCandidate>>,
    pub in_flight: Option<String>,
    pub dismissed_at: Option<usize>,
}

impl MentionTracker {
    /// The list rebuilt from the text and the cursor (a byte offset) after an
    /// edit: the participants who match, then the forge's matches, never the
    /// viewer. Returns a query to ask the forge for, marked in flight, when it
    /// is not known and none is being asked.
    pub fn refresh(
        &mut self,
        text: &str,
        cursor: usize,
        participants: &[String],
        viewer: Option<&str>,
    ) -> Option<String> {
        let Some((start, query)) = mention_token_at(text, cursor) else {
            self.close();
            return None;
        };
        if self.dismissed_at == Some(start) {
            self.list = None;
            return None;
        }
        self.dismissed_at = None;
        let lowered = query.to_lowercase();
        let ask = (!lowered.is_empty()
            && self.in_flight.is_none()
            && !self.results.contains_key(&lowered))
        .then(|| {
            self.in_flight = Some(lowered.clone());
            lowered
        });
        let candidates = mention_candidates(participants, viewer, query, &self.results);
        if candidates.is_empty() {
            self.list = None;
            return ask;
        }
        // The one highlighted stays highlighted while the list refines.
        let selected = self
            .list
            .as_ref()
            .and_then(|old| old.candidates.get(old.selected))
            .and_then(|old| candidates.iter().position(|c| c.login == old.login))
            .unwrap_or(0);
        self.list = Some(MentionList {
            start,
            query: query.to_string(),
            candidates,
            selected,
        });
        ask
    }

    /// The forge's matches for a query asked, in; a failure passes none.
    pub fn found(&mut self, query: String, found: Vec<MentionCandidate>) {
        if self.in_flight.as_deref() == Some(query.as_str()) {
            self.in_flight = None;
        }
        self.results.insert(query, found);
    }

    /// The highlight moved by `step`, going round within the first `rows`
    /// candidates (those shown).
    pub fn select(&mut self, step: isize, rows: usize) {
        if let Some(l) = &mut self.list {
            let n = l.candidates.len().min(rows).max(1) as isize;
            l.selected = (l.selected as isize + step).rem_euclid(n) as usize;
        }
    }

    /// The list closed, nothing taken: closed while the cursor is in this
    /// `@`.
    pub fn dismiss(&mut self) {
        if let Some(l) = self.list.take() {
            self.dismissed_at = Some(l.start);
        }
    }

    /// The box closed or started anew: no list, no `@` dismissed; the
    /// forge's answers and the query in flight kept.
    pub fn close(&mut self) {
        self.list = None;
        self.dismissed_at = None;
    }

    /// The highlighted login (or the `pick`-th) put in place of the `@`
    /// token: the new text and the cursor after it; none with no list.
    pub fn accept(
        &mut self,
        text: &str,
        cursor: usize,
        pick: Option<usize>,
    ) -> Option<(String, usize)> {
        let l = self.list.take()?;
        let c = l.candidates.get(pick.unwrap_or(l.selected))?;
        self.dismissed_at = Some(l.start);
        Some(accept_mention(text, l.start, cursor, &c.login))
    }
}
