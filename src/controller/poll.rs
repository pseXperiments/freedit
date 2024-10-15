use std::collections::HashMap;

use ammonia::url::form_urlencoded;
use askama_axum::{IntoResponse, Response};
use axum::body::Bytes;
use axum::extract::Path;
use axum::response::Redirect;
use axum::{
    async_trait,
    extract::{FromRequest, Request},
};
use rinja_axum::{into_response, Template};

use axum_extra::headers::Cookie;
use axum_extra::TypedHeader;
use bincode::config::standard;
use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::controller::filters;
use crate::{get_one, AppError, DB};

use super::db_utils::u32_to_ivec;
use super::meta_handler::PageData;
use super::{Claim, Post, PostContent, SiteConfig, User};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum PollQuestion {
    Text {
        question: String,
    },
    Choice {
        question: String,
        options: Vec<String>,
        multiple: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Poll {
    pub(crate) title: String,
    pub(crate) entries: Vec<PollQuestion>,
}

#[derive(Debug, Encode, Decode)]

pub enum PollResponse {
    Text(String),
    SingleChoice(usize),
    MultipleChoice(Vec<usize>),
}

pub struct PollFormQuery(pub Bytes);

#[derive(Debug, Encode, Decode)]
pub struct PollResult(Vec<PollResponse>);

impl PollFormQuery {
    pub fn parse(&self, poll: &Poll) -> Result<PollResult, String> {
        let bytes: Vec<u8> = self.0.to_vec();

        dbg!(String::from_utf8_lossy(&bytes));

        let mut answers = vec![];

        let results: HashMap<_, _> = form_urlencoded::parse(&bytes)
            .into_iter()
            .into_owned()
            .collect();
        for (i, entry) in poll.entries.iter().enumerate() {
            match entry {
                PollQuestion::Text { .. } => {
                    if let Some(value) = results.get(&format!("q{i}")) {
                        answers.push(PollResponse::Text(value.to_string()));
                    } else {
                        answers.push(PollResponse::Text("".to_string()));
                    }
                }
                PollQuestion::Choice {
                    options, multiple, ..
                } => {
                    if *multiple {
                        let mut opt = vec![];
                        for o in 0..options.len() {
                            if results.contains_key(&format!("q{i}_{o}")) {
                                opt.push(o);
                            }
                        }
                        answers.push(PollResponse::MultipleChoice(opt));
                    } else {
                        let selected = results
                            .get(&format!("q{i}"))
                            .cloned()
                            .unwrap_or(String::new());
                        let pos = options.iter().position(|o| o == &selected).unwrap_or(0);
                        answers.push(PollResponse::SingleChoice(pos));
                    }
                }
            }
        }

        Ok(PollResult(answers))
    }
}

#[async_trait]
impl<S> FromRequest<S> for PollFormQuery
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let body = Bytes::from_request(req, state)
            .await
            .map_err(|err| err.into_response())?;

        Ok(Self(body))
    }
}

impl Poll {
    pub const HTML_PLACEHOLDER: &str = "__poll_placeholder__";

    pub fn from_markdown(content: &str) -> Option<Result<Poll, AppError>> {
        if let Some((_, toml)) = content.split_once("```survey") {
            if let Some((toml, _)) = toml.split_once("```") {
                return Some(Poll::from_toml(toml));
            }
        }
        None
    }
    pub fn from_toml(toml: &str) -> Result<Poll, AppError> {
        toml::from_str(toml).map_err(|e| AppError::Custom(format!("Error parsing survey: {}", e)))
    }
    pub fn replace_content(
        &self,
        content: &str,
        iid: u32,
        pid: u32,
        _voted: Option<PollResult>,
    ) -> String {
        let html = self.html(iid, pid, _voted);
        content.replace(Self::HTML_PLACEHOLDER, &html)
    }
    fn html(&self, iid: u32, pid: u32, voted: Option<PollResult>) -> String {
        let mut html = String::new();
        html.push_str(&format!("<h1>{}</h1>", self.title));
        html.push_str(&format!(
            "<form action=\"/post/{iid}/{pid}/pollvote\" method=\"post\">"
        ));
        for (i, entry) in self.entries.iter().enumerate() {
            match entry {
                PollQuestion::Text { question } => {
                    let id = format!("q{i}");
                    let value = voted
                        .as_ref()
                        .and_then(|v| match &v.0[i] {
                            PollResponse::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    html.push_str(&format!(
                        "<p><b><label for={id}>{}</label></b></p>",
                        question
                    ));
                    html.push_str(&format!(
                        "<p><input type=\"text\" id={id} name={id} value=\"{value}\"></p>"
                    ));
                }
                PollQuestion::Choice {
                    question,
                    options,
                    multiple,
                } => {
                    html.push_str(&format!("<b>{}</b></p><ul>", question));
                    for (o, txt) in options.iter().enumerate() {
                        if *multiple {
                            let checked = voted
                                .as_ref()
                                .and_then(|v| match &v.0[i] {
                                    PollResponse::MultipleChoice(v) if v.contains(&o) => {
                                        Some("checked")
                                    }
                                    _ => None,
                                })
                                .unwrap_or("");

                            html.push_str(&format!(
                                "<li><input type=\"checkbox\" id=q{i}_{o} name=q{i}_{o} {checked}>"
                            ));
                        } else {
                            let checked = if let Some(voted) = voted.as_ref() {
                                match &voted.0[i] {
                                    PollResponse::SingleChoice(v) if *v == o => "checked",
                                    _ => "",
                                }
                            } else if o == 0 {
                                "checked"
                            } else {
                                ""
                            };

                            html.push_str(&format!(
                                "<li><input type=\"radio\" id=q{i} name=q{i} value=\"{txt}\" {checked}>"
                            ));
                        }
                        html.push_str(&format!("<label for=q{i}_{o}>&nbsp;{txt}</label></li>"));
                    }
                    html.push_str("</ul>");
                }
            }
        }
        html.push_str(
            "<input class=\"button is-link is-rounded\" type=\"submit\" value=\"Submit survey\"></form><br><br>",
        );

        html
    }
}

/// `POST /post/:iid/:pid/pollvote` to vote a post
///
/// if iid is 0, then create a new inn
pub(crate) async fn post_pollvote(
    cookie: Option<TypedHeader<Cookie>>,
    Path((iid, pid)): Path<(u32, u32)>,
    poll_response: PollFormQuery,
) -> Result<impl IntoResponse, AppError> {
    let cookie = cookie.ok_or(AppError::NonLogin)?;
    let site_config = SiteConfig::get(&DB)?;
    let claim = Claim::get(&DB, &cookie, &site_config).ok_or(AppError::NonLogin)?;

    let post: Post = get_one(&DB, "posts", pid)?;

    // Get the post content
    let md = if let PostContent::Markdown(md) = &post.content {
        md
    } else {
        return Err(AppError::Custom("Post is not a poll".into()));
    };

    // Get the poll inside the post
    let poll = if let Some(Ok(poll)) = Poll::from_markdown(md) {
        poll
    } else {
        return Err(AppError::Custom(
            "Post is not a poll or invalid poll".into(),
        ));
    };

    // Parse the user response
    let response = if let Ok(reponse) = poll_response.parse(&poll) {
        reponse
    } else {
        return Err(AppError::Custom("Invalid response".into()));
    };

    let response = bincode::encode_to_vec(&response, standard())?;

    let polls_tree = DB.open_tree("poll_contribution")?;
    let k = &[u32_to_ivec(pid), u32_to_ivec(claim.uid)].concat();
    polls_tree.insert(k, response)?;

    let target = format!("/poll/{iid}/{pid}");

    Ok(Redirect::to(&target))
}

/// Page data: `poll_results.html`
#[derive(Template)]
#[template(path = "poll_results.html", escape = "none")]
struct PollInfo<'a> {
    page_data: PageData<'a>,
    poll_info: String,
    iid: u32,
    pid: u32,
}

/// `GET /poll/:iid/:pid` post page
pub(crate) async fn poll_results(
    cookie: Option<TypedHeader<Cookie>>,
    Path((iid, pid)): Path<(u32, u32)>,
) -> Result<impl IntoResponse, AppError> {
    let site_config = SiteConfig::get(&DB)?;
    let claim = cookie.and_then(|cookie| Claim::get(&DB, &cookie, &site_config));
    let has_unread = if let Some(ref claim) = claim {
        User::has_unread(&DB, claim.uid)?
    } else {
        false
    };

    let mut poll_info = String::new();

    let polls_tree = DB.open_tree("poll_contribution")?;
    for entry in polls_tree.scan_prefix(u32_to_ivec(pid)) {
        let entry = entry
            .map_err(|err| AppError::Custom(format!("Error reading poll response: {}", err)))?;

        let (response, _): (PollResult, usize) = bincode::decode_from_slice(&entry.1, standard())
            .map_err(|err| {
            AppError::Custom(format!("Error decoding poll response: {}", err))
        })?;

        poll_info.push_str(&format!("{:?}\n", response));
    }

    let page_data = PageData::new(&"Poll Info", &site_config, claim, has_unread);
    let poll_info = PollInfo {
        page_data,
        poll_info,
        iid,
        pid,
    };
    Ok(into_response(&poll_info))
}

#[test]
fn test_survey_ecoding() {
    let survey = Poll {
        title: "Test Survey".to_string(),
        entries: vec![
            PollQuestion::Text {
                question: "What is your name?".to_string(),
            },
            PollQuestion::Choice {
                question: "What is your favorite color?".to_string(),
                options: vec!["Red".to_string(), "Green".to_string(), "Blue".to_string()],
                multiple: false,
            },
            PollQuestion::Choice {
                question: "What is your favorite color?".to_string(),
                options: vec!["Red".to_string(), "Green".to_string(), "Blue".to_string()],
                multiple: true,
            },
        ],
    };
    println!("{}", toml::to_string(&survey).unwrap());

    let txt = r#"
    title = "Test Survey"

[[entries]]
[entries.Text]
question = "What is your name?"

[[entries]]
[entries.Choice]
question = "What is your favorite color?"
options = ["Red", "Green", "Blue"]
multiple = false

[[entries]]
[entries.Choice]
question = "What is your favorite color?"
options = ["Red", "Green", "Blue"]
multiple = true
"#;

    let _poll: Poll = toml::from_str(txt).unwrap();

    unimplemented!()
}
