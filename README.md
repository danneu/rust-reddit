# rust-reddit

Crawls all submissions in a subreddit.

## Example

You'll need to create an app at <https://www.reddit.com/prefs/apps> to get an `app_id` and `app_secret`.

- `reddit::crawl()` takes a state struct and returns a page of submissions and a new state struct.
- Continually feed state structs back into `reddit::crawl()` until it returns `None` (end of subreddit).
- A state struct represents the progress of a crawl through a subreddit. You can change the OAuth token and user agent
independently. Otherwise, calling `crawl()` with the same state struct will create identical requests.
- `crawl()` reads the `prev_request_at` timestamp from the state struct to wait one second between requests
as per reddit's API guidelines.

```rust
extern crate reddit;
extern crate reqwest;

use std::process;

fn main() {
    let user_agent = "my-crawler:0.0.1 (by /u/reddit_username)".to_string();

    let creds = reddit::Creds {
        username: "reddit_username".to_string(),
        password: "reddit_password".to_string(),
        app_id: "xxxxxxxxx_xxxx".to_string(),
        app_secret: "yyyy-yyyyyyyyyyyyyyyyyyyyyy".to_string(),
    };
    
    // The crawler's api methods accept an http client
    // so that you can control lower-level details about the request.
    let client = reqwest::Client::builder()
        // ... configure http client
        .build()
        .unwrap();

    // For more convenient code, the default oauth token always triggers renewal
    let mut oauth = reddit::OAuth::default();

    let mut state = reddit::State::new("rust".to_string(), reddit::Config::default());

    loop {
        // Every loop, we check to see if oauth token needs renewal
        oauth = if reddit::OAuth::should_renew(&oauth) {
            match reddit::fetch_token(&creds, &user_agent, &client) {
                Ok(oauth) => oauth,
                Err(err) => {
                    eprintln!("error fetching new oauth token: {:?}", err);
                    process::exit(1)
                }
            }
        } else {
            oauth
        };
        
        // Request more submissions
        let (subs, next_state) = match reddit::crawl(&oauth, &state, &user_agent, &client) {
            Err(err) => {
                eprintln!("Fetch error: {:?}", err);
                process::exit(1)
            },
            Ok(None) => {
                println!("End of subreddit");
                process::exit(0)
            }
            Ok(Some(data)) => data,
        };

        // Process submissions
        subs.iter().for_each(|sub| {
            println!("    -> {}", sub.title);
        });

        // Prepare for next loop
        state = next_state;
    }
}
```

## Implementation

- The crawler works by repeatedly hitting Reddit's CloudSearch API with a time range that starts now and decrements
  backwards into the past.
- The range is grown or shrunk based on the results of the previous request in an effort to arrive at a sweet spot 
  of 50-99 results per request to minimize requests yet avoid Reddit's lossy `after`-pagination system.
- The end of subreddit is reached when the crawler's range grows to MAX_INTERVAL (default: one year) yet
  no results are found. For a range to grow that large, the crawler has to repeatedly make increasingly broad
  requests that yield no results.
  
## License

MIT
