//! GraphQL query and parsing for `@` mention suggestions.
//!
//! `mentionableUsers` is what GitHub's own comment box suggests from, filtered
//! server side by login and name. Asking per prefix avoids downloading a large
//! organisation.
//!
//! Payload shape (only the fields we read):
//!
//! ```json
//! {
//!   "data": {
//!     "repository": {
//!       "mentionableUsers": {
//!         "nodes": [{ "login": "octocat", "name": "The Octocat" }]
//!       }
//!     }
//!   }
//! }
//! ```

use serde::Deserialize;

use crate::error::{Result, TuicrError};
use crate::forge::traits::MentionCandidate;

/// How many matches one lookup asks for; the suggestion list shows fewer.
pub const SEARCH_LIMIT: usize = 10;

pub const QUERY: &str = concat!(
    "query($owner:String!,$name:String!,$q:String!,$first:Int!){",
    "repository(owner:$owner,name:$name){",
    "mentionableUsers(first:$first,query:$q){nodes{login name}}}}"
);

#[derive(Debug, Deserialize)]
struct GhUser {
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhUsersConn {
    #[serde(default)]
    nodes: Vec<Option<GhUser>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRepository {
    #[serde(default)]
    mentionable_users: Option<GhUsersConn>,
}

#[derive(Debug, Deserialize)]
struct GhData {
    #[serde(default)]
    repository: Option<GhRepository>,
}

#[derive(Debug, Deserialize)]
struct GhError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct GhResponse {
    #[serde(default)]
    data: Option<GhData>,
    #[serde(default)]
    errors: Vec<GhError>,
}

/// Parse a `mentionableUsers` response, in GitHub's order (best match first).
pub fn parse(json: &str) -> Result<Vec<MentionCandidate>> {
    let response: GhResponse = serde_json::from_str(json)?;
    let Some(repository) = response.data.and_then(|data| data.repository) else {
        let message = response
            .errors
            .into_iter()
            .map(|error| error.message)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(TuicrError::Forge(if message.is_empty() {
            "GitHub returned no repository for the mention lookup".to_string()
        } else {
            format!("Mention lookup failed: {message}")
        }));
    };
    let nodes = repository
        .mentionable_users
        .map(|conn| conn.nodes)
        .unwrap_or_default();
    Ok(nodes
        .into_iter()
        .flatten()
        .filter_map(|user| {
            let login = user.login.filter(|login| !login.is_empty())?;
            let name = user.name.filter(|name| !name.trim().is_empty());
            Some(MentionCandidate { login, name })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_users_in_order_and_drop_blank_names() {
        let json = r#"{"data":{"repository":{"mentionableUsers":{"nodes":[
            {"login":"octocat","name":"The Octocat"},
            {"login":"hubot","name":""},
            {"login":"monalisa","name":null},
            null
        ]}}}}"#;

        let users = parse(json).unwrap();

        assert_eq!(
            users,
            vec![
                MentionCandidate {
                    login: "octocat".into(),
                    name: Some("The Octocat".into()),
                },
                MentionCandidate {
                    login: "hubot".into(),
                    name: None,
                },
                MentionCandidate {
                    login: "monalisa".into(),
                    name: None,
                },
            ]
        );
    }

    #[test]
    fn should_report_graphql_errors() {
        let json = r#"{"data":{"repository":null},"errors":[{"message":"Could not resolve to a Repository"}]}"#;

        let error = parse(json).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Could not resolve to a Repository"),
            "{error}"
        );
    }
}
