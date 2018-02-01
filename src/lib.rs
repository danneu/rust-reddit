extern crate reqwest;
extern crate serde;
#[macro_use]
extern crate serde_derive;

use reqwest::{StatusCode, Url};
use reqwest::header::{self};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::collections::HashMap;
use std::ops::Sub;

const SECONDS_PER_YEAR: u64 = 31_536_000;
const SECONDS_PER_DAY: u64 = 86_400;

pub fn fetch_token(creds: &Creds, user_agent: &str) -> Result<OAuth, reqwest::Error> {
    let url = Url::parse("https://www.reddit.com/api/v1/access_token").unwrap();

    let mut form = HashMap::new();
    form.insert("grant_type", "password");
    form.insert("username", creds.username.as_ref());
    form.insert("password", creds.password.as_ref());

    // TODO: Pass (reuse) in client
    let client = reqwest::Client::new();

    let mut res= client.post(url)
        .basic_auth(creds.app_id.to_string(), Some(creds.app_secret.to_string()))
        .header(header::UserAgent::new(user_agent.to_string()))
        .form(&form)
        .send()?;

    // TODO: Handle non-200 response
    assert_eq!(res.status(), StatusCode::Ok);

    let body: AccessTokenResponse = res.json().unwrap();

    let oauth = OAuth {
        access_token: body.access_token,
        ttl: Duration::from_secs(body.expires_in),
        fetched_at: Instant::now(),
    };

    Ok(oauth)
}

#[derive(Debug)]
pub struct OAuth {
    access_token: String,
    ttl: Duration,
    fetched_at: Instant,
}

impl OAuth {
    pub fn should_renew(oauth: &OAuth) -> bool {
        let now = Instant::now();
        now.sub(oauth.fetched_at) >= oauth.ttl -Duration::from_secs(60 * 2)
    }
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Clone, Debug)]
pub struct Creds {
    pub username: String,
    pub password: String,
    pub app_id: String,
    pub app_secret: String,
}

// Stuff the crawler updates each crawl
#[derive(Clone, Debug)]
struct CrawlState {
    after: Option<String>,
    page: u32,
    max: SystemTime,
    interval: Duration,
    prev_request_at: SystemTime,
}


#[derive(Clone, Debug)]
pub struct State {
    subreddit: String,
    crawl_state: CrawlState,
    min_interval: Duration,
    max_interval: Duration,
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
    pub fn new(subreddit: String, config: Config) -> State {
        let reddit_offset = Duration::from_secs(60 * 60 * 8);
        let crawl_state = CrawlState {
            after: None,
            page: 1,
            interval: config.init_interval,
            max: config.init_max + reddit_offset,
            prev_request_at: UNIX_EPOCH
        };
        State {
            subreddit,
            min_interval: config.min_interval,
            max_interval: config.max_interval,
            crawl_state,
        }
    }
}

fn cloud_search(oauth: &OAuth, state: &State, user_agent: &str) -> reqwest::Result<(Vec<Submission>, Option<String>)> {
    let q = {
        // Clamp lower bound to epoch to handle distance_between(epoch, max) > interval
        let min = if state.crawl_state.max <= UNIX_EPOCH {
            UNIX_EPOCH
        } else {
            state.crawl_state.max -state.crawl_state.interval
        };
        let start = min.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let stop = state.crawl_state.max.duration_since(UNIX_EPOCH).unwrap().as_secs();

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

    if let Some(ref after) = state.crawl_state.after {
        params.push(("after", after));
    }

    let url = Url::parse_with_params(&format!("https://oauth.reddit.com/r/{}/search", state.subreddit), &params[..]).unwrap();

    // MAKE REQUEST

    // TODO: Pass (reuse) in client
    let client = reqwest::Client::new();

    let mut res= client.get(url)
        .header(header::Authorization(format!("bearer {}", oauth.access_token)))
        .header(header::UserAgent::new(user_agent.to_string()))
        .send()?;

    // TODO: Handle non-200 responses
    assert_eq!(res.status(), StatusCode::Ok);

    let body: CloudSearchResponse = res.json().unwrap();

    let subs = body.data.children.into_iter().map(|x| { x.data }).collect();
    let after = body.data.after;

    Ok((subs, after))
}

#[derive(Deserialize, Debug)]
struct CloudSearchResponse {
    data: CloudSearchEnvelope
}

#[derive(Deserialize, Debug)]
struct CloudSearchEnvelope {
    after: Option<String>,
    children: Vec<SubmissionEnvelope>
}

#[derive(Deserialize, Debug)]
struct SubmissionEnvelope {
    data: Submission
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

pub fn crawl(oauth: &OAuth, state: &State, user_agent: &str) -> reqwest::Result<Option<(Vec<Submission>, State)>> {
    // Ensure a second has elapsed since last request.
    {
        let one_second = Duration::from_secs(1);
        let elapsed = SystemTime::now().duration_since(state.crawl_state.prev_request_at).unwrap();
        let delay = if elapsed >= one_second {
            Duration::from_secs(0)
        } else {
            one_second - elapsed
        };
        std::thread::sleep(delay);
    }

    let (subs, next_after) = cloud_search(oauth, state, user_agent)?;

    // If we queried with maxInterval and still found nothing,
    // then we assume we've reached the end of the subreddit
    if state.crawl_state.interval == state.max_interval && subs.is_empty() {
        return Ok(None)
    }

    // The next request's max will be the min of the prev request to
    // crawl back in time.
    let next_max = match next_after {
        None =>
            state.crawl_state.max - state.crawl_state.interval,
        Some(_) =>
            state.crawl_state.max
    };

    // We adjust the interval until we get 50-99 submissions per request.
    // We want 50+ results so that we're making fewer requests to reddit.
    // But we want <=99 results to avoid `after`-pagination which is lossy.
    // - For ex, you can't make a request for submissions between 0..9999999999 and then paginate
    //   the entire subreddit.
    let next_interval = if state.crawl_state.page == 1 {
        let next_interval = clamp(
            get_next_interval(state.crawl_state.interval, subs.len()),
            state.min_interval,
            state.max_interval
        );

        // Interval debug info
        {
            let prev_interval = state.crawl_state.interval;
            if next_interval < prev_interval {
                println!("[interval] shrunk: {} -> {} ({} subs)", pretty_dur(prev_interval), pretty_dur(next_interval), subs.len());
            } else if next_interval > prev_interval {
                println!("[interval] grew: {} -> {} ({} subs)", pretty_dur(prev_interval), pretty_dur(next_interval), subs.len());
            } else {
                println!("[interval] unchanged: {} ({} subs)", pretty_dur(prev_interval), subs.len());
            }
        }

        next_interval
    } else {
        // We don't change the interval while paginating
        state.crawl_state.interval
    };

    let next_page = match next_after {
        None =>
            1,
        Some(_) =>
            state.crawl_state.page + 1
    };

    let next_crawl_state = CrawlState {
        after: next_after,
        max: next_max,
        page: next_page,
        interval: next_interval,
        prev_request_at: SystemTime::now(),
        .. state.crawl_state.clone()
    };

    let next_state = State {
        crawl_state: next_crawl_state,
        .. state.clone()
    };

    Ok(Some((subs, next_state)))
}

fn clamp<T>(value: T, min: T, max: T) -> T where T: std::cmp::Ord {
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
    hours: f64
}

fn pretty_dur(dur: Duration) -> String {
    let x = duration_in_days(dur);
    format!("{}d:{:.2}h", x.days, x.hours)
}


fn duration_in_days(dur: Duration) -> DurationInDays {
    let secs = dur.as_secs();
    let days = secs / SECONDS_PER_DAY;
    let hours = (secs as f64 % SECONDS_PER_DAY as f64) / 60 as f64 / 60 as f64;
    DurationInDays { days, hours }
}

