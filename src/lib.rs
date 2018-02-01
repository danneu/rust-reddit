extern crate reqwest;
extern crate serde;
#[macro_use]
extern crate serde_derive;

use reqwest::{StatusCode, Url};
use reqwest::header;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::collections::HashMap;
use std::ops::Sub;

const SECONDS_PER_YEAR: u64 = 31_536_000;
const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Debug)]
pub enum OAuthError {
    BadUserCreds, // username or password wrong
    BadAppCreds,  // app_id or app_secret wrong
    NetworkError(reqwest::Error),
    Other(Box<reqwest::Response>),
}

// Example:
//
//     {
//         "access_token": "xxxxxxxxxxxxxxxxxxxxxxxxxxx",
//         "expires_in": 3600,
//         "scope": "*",
//         "token_type": "bearer"
//     }
#[derive(Deserialize)]
struct RedditOAuthResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct RedditOAuthError {
    error: String,
}

// if app_id:app_secret auth combo is bad, response will be 401
// if username or password is bad, response will be 200 with json body {"error": "invalid_grant"}
pub fn fetch_token(
    creds: &Creds,
    user_agent: &str,
    client: &reqwest::Client,
) -> Result<OAuth, OAuthError> {
    let url = Url::parse("https://www.reddit.com/api/v1/access_token").unwrap();

    let mut form = HashMap::new();
    form.insert("grant_type", "password");
    form.insert("username", creds.username.as_ref());
    form.insert("password", creds.password.as_ref());

    let result = client
        .post(url)
        .basic_auth(creds.app_id.to_string(), Some(creds.app_secret.to_string()))
        .header(header::UserAgent::new(user_agent.to_string()))
        .form(&form)
        .send();

    let mut response = match result {
        Ok(res) => res,
        Err(err) => return Err(OAuthError::NetworkError(err)),
    };

    if response.status() == StatusCode::Unauthorized {
        return Err(OAuthError::BadAppCreds);
    };

    if let Ok(body) = response.json::<RedditOAuthResponse>() {
        let oauth = OAuth {
            access_token: body.access_token,
            ttl: Duration::from_secs(body.expires_in),
            fetched_at: SystemTime::now(),
        };

        return Ok(oauth);
    }

    if let Ok(body) = response.json::<RedditOAuthError>() {
        if body.error == "invalid_grant" {
            return Err(OAuthError::BadUserCreds);
        }
    }

    Err(OAuthError::Other(Box::new(response)))
}

#[derive(Debug)]
pub struct OAuth {
    pub access_token: String,
    pub ttl: Duration,
    pub fetched_at: SystemTime,
}

impl OAuth {
    // A token must be renewed if its ttl is expired 2 minutes from now. The 2 minutes are
    // a quick hack to avoid the case where a token expires between the check and the request.
    pub fn should_renew(oauth: &OAuth) -> bool {
        let now = SystemTime::now();
        now.duration_since(oauth.fetched_at).unwrap()
            >= safe_duration_sub(oauth.ttl, Duration::from_secs(60 * 2))
    }
}

// dur1 - dur2 fails if dur2 > dur1. this fn will just return a zero-length duration in that case.
fn safe_duration_sub(dur1: Duration, dur2: Duration) -> Duration {
    if dur1 < dur2 {
        Duration::from_secs(0)
    } else {
        dur1 - dur2
    }
}

impl std::default::Default for OAuth {
    // default oauth token will always trigger renewal.
    fn default() -> OAuth {
        OAuth {
            access_token: "".to_string(),
            ttl: Duration::from_secs(0),
            fetched_at: UNIX_EPOCH,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Creds {
    pub username: String,
    pub password: String,
    pub app_id: String,
    pub app_secret: String,
}

#[derive(Clone, Debug)]
pub struct State {
    subreddit: String,
    min_interval: Duration,
    max_interval: Duration,
    // The things that change between crawls
    after: Option<String>,
    page: u32,
    max: SystemTime,
    interval: Duration,
    prev_request_at: SystemTime,
}

// Stuff the user configures to create init state struct.
#[derive(Debug)]
pub struct Config {
    pub init_interval: Duration,
    pub min_interval: Duration,
    pub max_interval: Duration,
    pub init_max: SystemTime,
}

impl std::default::Default for Config {
    fn default() -> Config {
        Config {
            init_interval: Duration::from_secs(60 * 15),
            min_interval: Duration::from_secs(60 * 10),
            max_interval: Duration::from_secs(SECONDS_PER_YEAR),
            init_max: SystemTime::now(),
        }
    }
}

impl State {
    pub fn new(subreddit: String, config: &Config) -> State {
        let reddit_offset = Duration::from_secs(60 * 60 * 8);
        State {
            subreddit,
            min_interval: config.min_interval,
            max_interval: config.max_interval,
            after: None,
            page: 1,
            interval: config.init_interval,
            max: config.init_max + reddit_offset,
            prev_request_at: UNIX_EPOCH,
        }
    }
}

#[derive(Debug)]
pub enum ApiError {
    BadToken,
    NetworkError(reqwest::Error),
    UnexpectedBody(reqwest::Error),
    Other(Box<reqwest::Response>),
}

fn cloud_search(
    oauth: &OAuth,
    state: &State,
    user_agent: &str,
    client: &reqwest::Client,
) -> Result<(Vec<Submission>, Option<String>), ApiError> {
    let q = {
        // Clamp lower bound to epoch to handle distance_between(epoch, max) > interval
        let min = if state.max <= UNIX_EPOCH {
            UNIX_EPOCH
        } else {
            state.max - state.interval
        };
        let start = min.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let stop = state.max.duration_since(UNIX_EPOCH).unwrap().as_secs();

        format!("timestamp:{}..{}", start, stop)
    };

    let mut params = vec![
        ("q", q.as_ref()),
        ("syntax", "cloudsearch"),
        ("sort", "new"),
        ("type", "link"), // i.e. submissions only
        ("limit", "100"),
        ("restrict_sr", "true"),
        ("include_over_18", "on"),
        ("raw_json", "1"),
    ];

    if let Some(ref after) = state.after {
        params.push(("after", after));
    }

    let url = Url::parse_with_params(
        &format!("https://oauth.reddit.com/r/{}/search", state.subreddit),
        &params[..],
    ).unwrap();

    let result = client
        .get(url)
        .header(header::Authorization(format!(
            "bearer {}",
            oauth.access_token
        )))
        .header(header::UserAgent::new(user_agent.to_string()))
        .send();

    let mut res = match result {
        Err(err) => return Err(ApiError::NetworkError(err)),
        Ok(res) => res,
    };

    // Let's assume bad token if 401 or 403 because reddit responds with:
    // - Unauthorized when no bearer token header at all
    // - Forbidden when token is wrong
    if res.status() == StatusCode::Unauthorized || res.status() == StatusCode::Forbidden {
        return Err(ApiError::BadToken);
    }

    if res.status() == StatusCode::Ok {
        match res.json::<CloudSearchResponse>() {
            Ok(body) => {
                let subs = body.data.children.into_iter().map(|x| x.data).collect();
                let after = body.data.after;

                return Ok((subs, after));
            }
            Err(err) => return Err(ApiError::UnexpectedBody(err)),
        }
    }

    Err(ApiError::Other(Box::new(res)))
}

#[derive(Deserialize, Debug)]
struct CloudSearchResponse {
    data: CloudSearchEnvelope,
}

#[derive(Deserialize, Debug)]
struct CloudSearchEnvelope {
    after: Option<String>,
    children: Vec<SubmissionEnvelope>,
}

#[derive(Deserialize, Debug)]
struct SubmissionEnvelope {
    data: Submission,
}

#[derive(Deserialize, Debug)]
// TODO: split SubmissionData (direct parse) into a transform(SubmissionData) -> Submission step
//       For ex, most of these f64 values are actually unsigned (cannot be negative)
pub struct Submission {
    pub title: String,
    pub url: String,
    pub ups: f64,
    pub downs: f64,
    pub score: f64,
    pub author: String,
    pub subreddit: String,
    pub stickied: bool,
    pub permalink: String,
    pub locked: bool,
    // note: created is with reddit's 8hr offset
    pub created: f64,
    pub created_utc: f64,
    pub is_self: bool,
    pub is_video: bool,
    pub id: String,
    pub name: String,
    pub num_comments: f64,
    pub domain: String,
    // empty string if there is no thumbnail
    pub thumbnail: String,
}

pub fn crawl(
    oauth: &OAuth,
    state: &State,
    user_agent: &str,
    client: &reqwest::Client,
) -> Result<Option<(Vec<Submission>, State)>, ApiError> {
    // Ensure a second has elapsed since last request.
    {
        let one_second = Duration::from_secs(1);
        let elapsed = SystemTime::now()
            .duration_since(state.prev_request_at)
            .unwrap();
        let delay = if elapsed >= one_second {
            Duration::from_secs(0)
        } else {
            one_second - elapsed
        };
        std::thread::sleep(delay);
    }

    let (subs, next_after) = cloud_search(oauth, state, user_agent, client)?;

    // If we queried with maxInterval and still found nothing,
    // then we assume we've reached the end of the subreddit
    if state.interval == state.max_interval && subs.is_empty() {
        return Ok(None);
    }

    // The next request's max will be the min of the prev request to
    // crawl back in time.
    let next_max = match next_after {
        None => state.max - state.interval,
        Some(_) => state.max,
    };

    // We adjust the interval until we get 50-99 submissions per request.
    // We want 50+ results so that we're making fewer requests to reddit.
    // But we want <=99 results to avoid `after`-pagination which is lossy.
    // - For ex, you can't make a request for submissions between 0..9999999999 and then paginate
    //   the entire subreddit.
    let next_interval = if state.page == 1 {
        let next_interval = clamp(
            get_next_interval(state.interval, subs.len()),
            state.min_interval,
            state.max_interval,
        );

        // Interval debug info
        {
            if next_interval < state.interval {
                println!(
                    "[interval] shrunk: {} -> {} ({} subs)",
                    pretty_dur(state.interval),
                    pretty_dur(next_interval),
                    subs.len()
                );
            } else if next_interval > state.interval {
                println!(
                    "[interval] grew: {} -> {} ({} subs)",
                    pretty_dur(state.interval),
                    pretty_dur(next_interval),
                    subs.len()
                );
            } else {
                println!(
                    "[interval] unchanged in 50-99 sweetspot: {} ({} subs)",
                    pretty_dur(state.interval),
                    subs.len()
                );
            }
        }

        next_interval
    } else {
        // We don't change the interval while paginating
        println!(
            "[interval] unchanged during pagination: {} ({} subs)",
            pretty_dur(state.interval),
            subs.len()
        );
        state.interval
    };

    let next_page = match next_after {
        None => 1,
        Some(_) => state.page + 1,
    };

    let next_state = State {
        after: next_after,
        max: next_max,
        page: next_page,
        interval: next_interval,
        prev_request_at: SystemTime::now(),
        ..state.clone()
    };

    Ok(Some((subs, next_state)))
}

// Ensure a value exists in an inclusive range. If out of range, the violated bound is returned.
//
//     clamp(-1, 3, 10) == 3
//     clamp(11, 3, 10) == 10
//     clamp(5, 3, 10) == 5
fn clamp<T>(value: T, min: T, max: T) -> T
where
    T: std::cmp::Ord,
{
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

fn get_next_interval(interval: Duration, page_size: usize) -> Duration {
    if page_size == 0 {
        // no results: increasing interval
        interval + (interval / 2)
    } else if page_size < 50 {
        // found <50 submissions: increasing interval
        interval + (interval / 2)
    } else if page_size == 100 {
        // i.e. first page is full thus there's another page
        // found 100 submissions: shrinking interval
        interval - (interval / 20)
    } else {
        // found 50-99 submissions: not changing interval
        interval
    }
}

struct DurationInDays {
    days: u64,
    hours: f64,
}

fn pretty_dur(dur: Duration) -> String {
    let x = duration_in_days(dur);
    format!("{}d:{:.2}h", x.days, x.hours)
}

fn duration_in_days(dur: Duration) -> DurationInDays {
    let secs = dur.as_secs();
    let days = secs / SECONDS_PER_DAY;
    let hours = (secs as f64 % SECONDS_PER_DAY as f64) / f64::from(60 * 60);
    DurationInDays { days, hours }
}
